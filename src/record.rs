//! Record a live device to a stereo 128 kbps MP3: small, and accepted by every transcription
//! backend Speakr talks to (they rejected our WAVs). An IOProc on the aggregate mixes all its
//! streams to stereo and hands the frames to a writer thread that encodes with LAME.
//! Dropping the Recorder stops capture and flushes the file.

use std::{
    ffi::c_void,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::mpsc::{Receiver, Sender, channel},
    thread::JoinHandle,
};

use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};

use crate::{check, get};

pub struct Recorder {
    pub path: PathBuf,
    dev: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    tx: Option<Box<Sender<Vec<f32>>>>, // boxed for a stable address handed to the IOProc
    writer: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn start(dev: AudioObjectID, path: PathBuf) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let rate = get::<f64>(dev, kAudioDevicePropertyNominalSampleRate, None).unwrap_or(48000.0);
        let (tx, rx) = channel();
        let shown = path.clone();
        let writer = std::thread::spawn(move || {
            if let Err(e) = write_mp3(file, rate as u32, rx) {
                eprintln!("recording {}: {e}", shown.display());
            }
        });
        let mut r = Recorder {
            path,
            dev,
            proc_id: None,
            tx: Some(Box::new(tx)),
            writer: Some(writer),
        };
        let data = (&**r.tx.as_ref().unwrap() as *const Sender<_>)
            .cast_mut()
            .cast::<c_void>();
        // On error `r` drops here, which ends the writer thread.
        check(
            unsafe {
                AudioDeviceCreateIOProcID(dev, Some(io_proc), data, NonNull::from(&mut r.proc_id))
            },
            "record ioproc",
        )?;
        check(unsafe { AudioDeviceStart(dev, r.proc_id) }, "record start")?;
        Ok(r)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.dev, self.proc_id);
                AudioDeviceDestroyIOProcID(self.dev, self.proc_id);
            }
        }
        self.tx = None; // closes the channel: the writer patches the header and exits
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

/// Audio thread: interleave every stream's channels into one frame and send it on.
// ponytail: allocates a Vec per callback and assumes Float32; a preallocated ring buffer if it glitches.
unsafe extern "C-unwind" fn io_proc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _in_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _out_time: NonNull<AudioTimeStamp>,
    data: *mut c_void,
) -> i32 {
    let tx = unsafe { &*data.cast::<Sender<Vec<f32>>>() };
    let list = unsafe { input.as_ref() };
    let bufs =
        unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) };
    let streams: Vec<(usize, &[f32])> = bufs
        .iter()
        .filter(|b| !b.mData.is_null())
        .map(|b| {
            let n = b.mNumberChannels.max(1) as usize;
            let s = unsafe {
                std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
            };
            (n, s)
        })
        .collect();
    let _ = tx.send(mix_stereo(&streams));
    0
}

/// Interleaved stereo from the aggregate's streams (each `(channels, interleaved samples)`):
/// a mono stream (a mic) goes to both sides, otherwise channel i goes to side i % 2.
fn mix_stereo(streams: &[(usize, &[f32])]) -> Vec<f32> {
    let frames = streams.iter().map(|(n, s)| s.len() / n).min().unwrap_or(0);
    let mut out = vec![0f32; frames * 2];
    for (n, s) in streams {
        for f in 0..frames {
            for c in 0..*n {
                let v = s[f * n + c];
                if *n == 1 {
                    out[f * 2] += v;
                    out[f * 2 + 1] += v;
                } else {
                    out[f * 2 + c % 2] += v;
                }
            }
        }
    }
    out
}

/// Float32 WAV. The channel count comes from the first chunk; sizes are patched at the end.
/// Encode stereo f32 frames to MP3 as they arrive.
fn write_mp3(file: File, rate: u32, rx: Receiver<Vec<f32>>) -> Result<(), String> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality};
    let err = |e: &dyn std::fmt::Debug| format!("{e:?}");
    let mut enc = Builder::new()
        .ok_or("LAME init failed")?
        .with_num_channels(2)
        .and_then(|b| b.with_sample_rate(rate))
        .and_then(|b| b.with_brate(Bitrate::Kbps128))
        .and_then(|b| b.with_quality(Quality::Good))
        .and_then(|b| b.with_to_write_vbr_tag(false))
        .map_err(|e| err(&e))?
        .build()
        .map_err(|e| err(&e))?;
    let mut w = BufWriter::new(file);
    let mut out = Vec::new();
    for mut samples in rx {
        // Mixed sources can sum past full scale: clip rather than distort.
        samples.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
        out.clear();
        out.reserve(mp3lame_encoder::max_required_buffer_size(samples.len() / 2));
        enc.encode_to_vec(InterleavedPcm(&samples), &mut out)
            .map_err(|e| err(&e))?;
        w.write_all(&out).map_err(|e| e.to_string())?;
    }
    out.clear();
    out.reserve(7200);
    enc.flush_to_vec::<FlushNoGap>(&mut out)
        .map_err(|e| err(&e))?;
    w.write_all(&out)
        .and_then(|()| w.flush())
        .map_err(|e| e.to_string())
}

/// `<dir>/YYYY-MM-DD-hh.mm.ss-<device-name>.mp3`, local time; the name lowercased, spaces → `-`. `~` expands to $HOME.
pub fn file_path(dir: &str, name: &str) -> PathBuf {
    let dir = match dir.strip_prefix("~") {
        Some(rest) => {
            Path::new(&std::env::var("HOME").unwrap_or_default()).join(rest.trim_start_matches('/'))
        }
        None => PathBuf::from(dir),
    };
    let mut buf = [0u8; 32];
    let len = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%Y-%m-%d-%H.%M.%S".as_ptr(),
            &tm,
        )
    };
    let stamp = String::from_utf8_lossy(&buf[..len]);
    let name = name.to_lowercase().replace(['/', ' '], "-");
    dir.join(format!("{stamp}-{name}.mp3"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn mp3_roundtrip() {
        // 1 s of a loud 440 Hz tone (amplitude 1.5: must be clipped, not fail).
        let path = std::env::temp_dir().join("recordr-test.mp3");
        let (tx, rx) = std::sync::mpsc::channel();
        let tone: Vec<f32> = (0..48000)
            .flat_map(|i| {
                let v = 1.5 * (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin();
                [v, v]
            })
            .collect();
        tx.send(tone).unwrap();
        drop(tx);
        super::write_mp3(std::fs::File::create(&path).unwrap(), 48000, rx).unwrap();
        // macOS's own decoder must accept it.
        let info = std::process::Command::new("afinfo")
            .arg(&path)
            .output()
            .unwrap();
        let info = String::from_utf8_lossy(&info.stdout);
        assert!(info.contains(".mp3") && info.contains("2 ch"), "{info}");
        let secs: f64 = info
            .lines()
            .find_map(|l| {
                l.strip_prefix("estimated duration: ")?
                    .split(' ')
                    .next()?
                    .parse()
                    .ok()
            })
            .unwrap();
        assert!((secs - 1.0).abs() < 0.1, "{info}");
    }

    #[test]
    fn mixes_to_stereo() {
        // Stereo tap (L=0.1, R=0.2 per frame) + mono mic (0.5): the mic goes to both sides.
        let tap = [0.1f32, 0.2, 0.1, 0.2];
        let mic = [0.5f32, 0.5];
        let out = super::mix_stereo(&[(2, &tap), (1, &mic)]);
        assert_eq!(out, vec![0.6, 0.7, 0.6, 0.7]);
    }
}

#[test]
fn file_name() {
    let p = file_path("/tmp/r", "Record in Speakr");
    let f = p.file_name().unwrap().to_str().unwrap();
    assert!(
        p.starts_with("/tmp/r") && f.ends_with("-record-in-speakr.mp3"),
        "{f}"
    );
    assert_eq!(f.len(), "YYYY-MM-DD-hh.mm.ss-record-in-speakr.mp3".len());
}
