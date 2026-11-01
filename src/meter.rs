//! Live stereo peak meters for the GUI: a private (invisible, unmuted) tap per app,
//! or an IOProc straight on an input device. Dropping a Meter tears it all down.

use std::{
    cell::Cell,
    ffi::c_void,
    ptr::NonNull,
    sync::atomic::{AtomicU32, Ordering::Relaxed},
};

use objc2::AnyThread;
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSNumber};

use crate::{aggregate_description, check, get_ids, get_string, processes};

pub struct Meter {
    peaks: Box<[AtomicU32; 2]>, // f32 bits, written by the audio thread; boxed for a stable address
    shown: Cell<[f32; 2]>,
    dev: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    owned: Option<(AudioObjectID, AudioObjectID)>, // (aggregate, tap) we created for an app
}

impl Meter {
    /// Meter every audio process with exactly this bundle id.
    pub fn app(bundle: &str) -> Result<Self, String> {
        let procs: Vec<_> = processes()
            .into_iter()
            .filter(|p| p.bundle == bundle)
            .map(|p| NSNumber::new_u32(p.id))
            .collect();
        if procs.is_empty() {
            return Err(format!("{bundle} has no audio process"));
        }
        let desc = unsafe {
            let d = CATapDescription::initStereoMixdownOfProcesses(
                CATapDescription::alloc(),
                &NSArray::from_retained_slice(&procs),
            );
            d.setPrivate(true);
            d.setMuteBehavior(CATapMuteBehavior::Unmuted);
            d
        };
        let mut tap = 0;
        check(
            unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut tap) },
            "meter tap",
        )?;
        let tap_uid = unsafe { desc.UUID() }.UUIDString().to_string();
        let dict = aggregate_description(
            "recordr meter",
            &format!("recordr.meter.{tap_uid}"),
            &[],
            Some(&tap_uid),
            true,
        );
        let cf: &CFDictionary = unsafe { &*(objc2::rc::Retained::as_ptr(&dict).cast()) };
        let mut agg = 0;
        let st = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut agg)) };
        if let Err(e) = check(st, "meter aggregate") {
            unsafe { AudioHardwareDestroyProcessTap(tap) };
            return Err(e);
        }
        Self::start(agg, Some((agg, tap)))
    }

    /// Meter an input device's first two channels.
    pub fn device(uid: &str) -> Result<Self, String> {
        let dev = get_ids(
            kAudioObjectSystemObject as u32,
            kAudioHardwarePropertyDevices,
        )
        .into_iter()
        .find(|&d| get_string(d, kAudioDevicePropertyDeviceUID).as_deref() == Some(uid))
        .ok_or_else(|| format!("device {uid} is gone"))?;
        Self::start(dev, None)
    }

    fn start(
        dev: AudioObjectID,
        owned: Option<(AudioObjectID, AudioObjectID)>,
    ) -> Result<Self, String> {
        let mut m = Meter {
            peaks: Box::new([AtomicU32::new(0), AtomicU32::new(0)]),
            shown: Cell::new([0.0; 2]),
            dev,
            proc_id: None,
            owned,
        };
        let data = (&*m.peaks as *const [AtomicU32; 2])
            .cast_mut()
            .cast::<c_void>();
        // On error `m` drops here and cleans up whatever was created.
        check(
            unsafe {
                AudioDeviceCreateIOProcID(dev, Some(io_proc), data, NonNull::from(&mut m.proc_id))
            },
            "meter ioproc",
        )?;
        check(unsafe { AudioDeviceStart(dev, m.proc_id) }, "meter start")?;
        Ok(m)
    }

    /// [L, R] in 0..1 on a -60..0 dB scale; instant attack, smooth fall.
    pub fn levels(&self) -> [f32; 2] {
        let mut s = self.shown.get();
        for (i, p) in self.peaks.iter().enumerate() {
            let peak = f32::from_bits(p.swap(0, Relaxed));
            let db = 20.0 * peak.max(1e-6).log10();
            let level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            s[i] = level.max(s[i] - 0.03);
        }
        self.shown.set(s);
        s
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.dev, self.proc_id);
                AudioDeviceDestroyIOProcID(self.dev, self.proc_id);
            }
            if let Some((agg, tap)) = self.owned {
                AudioHardwareDestroyAggregateDevice(agg);
                AudioHardwareDestroyProcessTap(tap);
            }
        }
    }
}

/// Audio thread: fold the buffer's per-channel peak into `peaks` (max since last read).
// ponytail: assumes Float32 samples, the HAL's canonical IOProc format on every device seen so far.
unsafe extern "C-unwind" fn io_proc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _in_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _out_time: NonNull<AudioTimeStamp>,
    data: *mut c_void,
) -> i32 {
    let peaks = unsafe { &*data.cast::<[AtomicU32; 2]>() };
    let list = unsafe { input.as_ref() };
    let bufs =
        unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) };
    let (mut peak, mut ch) = ([0f32; 2], 0usize);
    for b in bufs {
        let n = b.mNumberChannels.max(1) as usize;
        if !b.mData.is_null() && ch < 2 {
            let samples = unsafe {
                std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
            };
            for (i, s) in samples.iter().enumerate() {
                if let Some(p) = peak.get_mut(ch + i % n) {
                    *p = p.max(s.abs());
                }
            }
        }
        ch += n;
    }
    if ch == 1 {
        peak[1] = peak[0]; // mono mic: show it on both bars
    }
    for (p, v) in peaks.iter().zip(peak) {
        p.fetch_max(v.to_bits(), Relaxed); // positive f32 bits order like the floats
    }
    0
}
