//! Record a live device to a Float32 WAV: an IOProc on the aggregate hands interleaved frames
//! to a writer thread. Dropping the Recorder stops capture and finalizes the file.

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
    tx: Option<Box<Sender<(u16, Vec<f32>)>>>, // boxed for a stable address handed to the IOProc
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
    let tx = unsafe { &*data.cast::<Sender<(u16, Vec<f32>)>>() };
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
    let channels: usize = streams.iter().map(|(n, _)| n).sum();
    let frames = streams.iter().map(|(n, s)| s.len() / n).min().unwrap_or(0);
    let mut out = Vec::with_capacity(frames * channels);
    for f in 0..frames {
        for (n, s) in &streams {
            out.extend_from_slice(&s[f * n..(f + 1) * n]);
        }
    }
    let _ = tx.send((channels as u16, out));
    0
}

/// Float32 WAV. The channel count comes from the first chunk; sizes are patched at the end.
fn write_wav(file: File, rate: u32, rx: Receiver<(u16, Vec<f32>)>) -> std::io::Result<()> {
    let mut w = BufWriter::new(file);
    let mut channels = 0u16;
    for (ch, samples) in rx {
        if channels == 0 {
            channels = ch.max(1);
            header(&mut w, channels, rate, 0)?;
        }
        if ch != channels {
            continue; // ponytail: a stream layout change mid-recording drops those chunks
        }
        for s in samples {
            w.write_all(&s.to_le_bytes())?;
        }
    }
    // ponytail: u32 RIFF sizes cap a file at 4 GB (~3 h of stereo 48 kHz float); RF64 past that.
    let len = w.stream_position()? as u32;
    if channels == 0 {
        header(&mut w, 2, rate, 0)?; // nothing recorded: still a valid, empty WAV
    } else {
        w.seek(SeekFrom::Start(0))?;
        header(&mut w, channels, rate, len - 44)?;
    }
    w.flush()
}

fn header(w: &mut impl Write, channels: u16, rate: u32, data: u32) -> std::io::Result<()> {
    let block = channels * 4;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&3u16.to_le_bytes())?; // IEEE float
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * block as u32).to_le_bytes())?;
    w.write_all(&block.to_le_bytes())?;
    w.write_all(&32u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data.to_le_bytes())
}

/// `<dir>/<name> YYYY-MM-DD HH.MM.SS.wav`, local time. `~` expands to $HOME.
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
            c"%Y-%m-%d %H.%M.%S".as_ptr(),
            &tm,
        )
    };
    let stamp = String::from_utf8_lossy(&buf[..len]);
    let name = name.replace('/', "-");
    dir.join(format!("{name} {stamp}.wav"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn wav_roundtrip() {
        let path = std::env::temp_dir().join("recordr-test.wav");
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send((2, vec![0.5f32, -0.5, 0.25, -0.25])).unwrap();
        drop(tx);
        super::write_wav(std::fs::File::create(&path).unwrap(), 48000, rx).unwrap();
        let b = std::fs::read(&path).unwrap();
        assert_eq!(b.len(), 44 + 16);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 36 + 16);
        assert_eq!(u16::from_le_bytes(b[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 16);
        assert_eq!(f32::from_le_bytes(b[44..48].try_into().unwrap()), 0.5);
    }
}
