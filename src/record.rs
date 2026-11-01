//! Record a live device to a 16-bit PCM stereo WAV, the format every decoder (and Speakr's
//! transcription backends) accepts. An IOProc on the aggregate mixes all its streams to stereo
//! and hands the frames to a writer thread. Dropping the Recorder stops capture and finalizes it.

use std::{
    ffi::c_void,
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
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
            if let Err(e) = write_wav(file, rate as u32, rx) {
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
/// Stereo 16-bit PCM WAV; the data size is patched into the header at the end.
fn write_wav(file: File, rate: u32, rx: Receiver<Vec<f32>>) -> std::io::Result<()> {
    let mut w = BufWriter::new(file);
    header(&mut w, rate, 0)?;
    for samples in rx {
        for s in samples {
            // Mixed sources can sum past full scale: clip rather than wrap.
            w.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
        }
    }
    // ponytail: u32 RIFF sizes cap a file at 4 GB (~6 h of stereo 48 kHz 16-bit); RF64 past that.
    let len = w.stream_position()? as u32;
    w.seek(SeekFrom::Start(0))?;
    header(&mut w, rate, len - 44)?;
    w.flush()
}

fn header(w: &mut impl Write, rate: u32, data: u32) -> std::io::Result<()> {
    let (channels, bits) = (2u16, 16u16);
    let block = channels * bits / 8;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * block as u32).to_le_bytes())?;
    w.write_all(&block.to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data.to_le_bytes())
}

/// `<dir>/YYYY-MM-DD-hh.mm.ss-<device-name>.wav`, local time; the name lowercased, spaces → `-`. `~` expands to $HOME.
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
    dir.join(format!("{stamp}-{name}.wav"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn wav_roundtrip() {
        let path = std::env::temp_dir().join("recordr-test.wav");
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(vec![0.5f32, -0.5, 2.0, -0.25]).unwrap();
        drop(tx);
        super::write_wav(std::fs::File::create(&path).unwrap(), 48000, rx).unwrap();
        let b = std::fs::read(&path).unwrap();
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        assert_eq!(b.len(), 44 + 8);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32_at(4), 36 + 8);
        assert_eq!((u16_at(20), u16_at(22), u16_at(34)), (1, 2, 16)); // PCM, stereo, 16-bit
        assert_eq!((u32_at(28), u16_at(32)), (48000 * 4, 4)); // byte rate, block align
        assert_eq!(u32_at(40), 8);
        assert_eq!(u16_at(44) as i16, 16383);
        assert_eq!(u16_at(48) as i16, 32767, "clipped, not wrapped");
        // macOS's own decoder must accept it.
        let info = std::process::Command::new("afinfo")
            .arg(&path)
            .output()
            .unwrap();
        let info = String::from_utf8_lossy(&info.stdout);
        assert!(info.contains("Int16") && info.contains("2 ch"), "{info}");
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
        p.starts_with("/tmp/r") && f.ends_with("-record-in-speakr.wav"),
        "{f}"
    );
    assert_eq!(f.len(), "YYYY-MM-DD-hh.mm.ss-record-in-speakr.wav".len());
}
