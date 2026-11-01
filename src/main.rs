//! recordr — a Loopback-style virtual audio device for macOS, in Rust.
//!
//! Uses Core Audio process taps (macOS 14.2+) mixed down to stereo and exposed
//! through a public aggregate device, so any app (Zoom, OBS, Logic…) can pick
//! it as an input. No kernel/HAL driver needed.
// ponytail: taps over an AudioServerPlugIn driver; a driver only matters if you need <14.2 support.

mod gui;

use std::{ffi::c_void, ptr::NonNull, sync::mpsc};

use objc2::{AnyThread, rc::Retained};
use objc2_core_audio::*;
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

const USAGE: &str = "usage:
  recordr               open the window
  recordr list
  recordr run <name> [--app <bundle-id|pid>]... [--system] [--device <uid>]... [--mute]

  --app     include an app's audio (bundle-id prefix match, or pid). Repeatable.
  --system  include all system audio instead of specific apps
  --device  add a physical device's input (e.g. a mic) as extra channels. Repeatable.
  --mute    mute the tapped apps on the speakers";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("list") => {
            list();
            Ok(())
        }
        Some("run") => parse_run(&args[1..]).and_then(run),
        None => gui::run(),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

// ---------- property helpers ----------

fn addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn get<T: Copy>(obj: AudioObjectID, selector: u32, qualifier: Option<&[u8]>) -> Option<T> {
    let a = addr(selector);
    let mut size = size_of::<T>() as u32;
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let (qs, qp) = qualifier.map_or((0, std::ptr::null()), |q| {
        (q.len() as u32, q.as_ptr().cast())
    });
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&a),
            qs,
            qp,
            NonNull::from(&mut size),
            NonNull::new(out.as_mut_ptr().cast()).unwrap(),
        )
    };
    (st == 0).then(|| unsafe { out.assume_init() })
}

fn get_ids(obj: AudioObjectID, selector: u32) -> Vec<AudioObjectID> {
    let a = addr(selector);
    let mut size = 0u32;
    if unsafe {
        AudioObjectGetPropertyDataSize(
            obj,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    } != 0
    {
        return vec![];
    }
    let mut ids = vec![0 as AudioObjectID; size as usize / size_of::<AudioObjectID>()];
    if ids.is_empty() {
        return ids;
    }
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(ids.as_mut_ptr().cast()).unwrap(),
        )
    };
    if st != 0 {
        return vec![];
    }
    ids.truncate(size as usize / size_of::<AudioObjectID>());
    ids
}

fn get_string(obj: AudioObjectID, selector: u32) -> Option<String> {
    // CFString properties come back +1 retained; CFString is toll-free bridged to NSString.
    let p: *mut c_void = get(obj, selector, None)?;
    let s = unsafe { Retained::from_raw(p.cast::<NSString>())? };
    Some(s.to_string())
}

fn check(st: i32, what: &str) -> Result<(), String> {
    if st == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed: OSStatus {st} ({})", fourcc(st)))
    }
}

fn fourcc(st: i32) -> String {
    let b = st.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) {
        String::from_utf8_lossy(&b).into()
    } else {
        "?".into()
    }
}

// ---------- list ----------

struct Proc {
    id: AudioObjectID,
    pid: i32,
    bundle: String,
    playing: bool,
}

fn processes() -> Vec<Proc> {
    get_ids(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyProcessObjectList,
    )
    .into_iter()
    .map(|id| Proc {
        id,
        pid: get(id, kAudioProcessPropertyPID, None).unwrap_or(-1),
        bundle: get_string(id, kAudioProcessPropertyBundleID).unwrap_or_default(),
        playing: get::<u32>(id, kAudioProcessPropertyIsRunningOutput, None).unwrap_or(0) != 0,
    })
    .collect()
}

fn list() {
    println!("Processes with audio (* = playing now):");
    for p in processes() {
        println!(
            "  {} {:>6}  {}",
            if p.playing { "*" } else { " " },
            p.pid,
            p.bundle
        );
    }
    println!("\nDevices (uid — name):");
    for (uid, name) in device_uids() {
        println!("  {uid} — {name}");
    }
}

fn device_uids() -> Vec<(String, String)> {
    get_ids(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDevices,
    )
    .into_iter()
    .map(|d| {
        let uid = get_string(d, kAudioDevicePropertyDeviceUID).unwrap_or_default();
        (
            uid,
            get_string(d, kAudioObjectPropertyName).unwrap_or_default(),
        )
    })
    .collect()
}

/// Devices that can record (have at least one input stream): (uid, name).
/// Input-capable devices as (uid, name, transport type fourcc).
fn input_devices() -> Vec<(String, String, u32)> {
    get_ids(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDevices,
    )
    .into_iter()
    .filter(|&d| {
        let a = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreams,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut size = 0u32;
        let st = unsafe {
            AudioObjectGetPropertyDataSize(
                d,
                NonNull::from(&a),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
            )
        };
        st == 0 && size > 0
    })
    .map(|d| {
        let uid = get_string(d, kAudioDevicePropertyDeviceUID).unwrap_or_default();
        (
            uid,
            get_string(d, kAudioObjectPropertyName).unwrap_or_default(),
            get(d, kAudioDevicePropertyTransportType, None).unwrap_or(0),
        )
    })
    .collect()
}

// ---------- run ----------

#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct RunArgs {
    name: String,
    apps: Vec<String>,
    system: bool,
    devices: Vec<String>,
    mute: bool,
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let mut it = args.iter();
    let mut r = RunArgs {
        name: it.next().ok_or(USAGE)?.clone(),
        ..Default::default()
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--app" => r.apps.push(it.next().ok_or("--app needs a value")?.clone()),
            "--device" => r
                .devices
                .push(it.next().ok_or("--device needs a value")?.clone()),
            "--system" => r.system = true,
            "--mute" => r.mute = true,
            other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
        }
    }
    if r.apps.is_empty() && !r.system && r.devices.is_empty() {
        return Err("nothing to route: pass --app, --system or --device".into());
    }
    Ok(r)
}

/// Resolve `--app` values to Core Audio process objects.
/// A bundle-id matches as a prefix so `com.google.Chrome` also picks its helper processes.
fn resolve_apps(apps: &[String]) -> Result<Vec<AudioObjectID>, String> {
    let procs = processes();
    let mut ids = vec![];
    for app in apps {
        let found: Vec<_> = match app.parse::<i32>() {
            Ok(pid) => procs
                .iter()
                .filter(|p| p.pid == pid)
                .map(|p| p.id)
                .collect(),
            Err(_) => procs
                .iter()
                .filter(|p| p.bundle.starts_with(app.as_str()))
                .map(|p| p.id)
                .collect(),
        };
        if found.is_empty() {
            // ponytail: app must already have opened audio; watch ProcessObjectList to pick up late launches.
            return Err(format!(
                "no audio process matches '{app}' (is it running and has it played sound? see `recordr list`)"
            ));
        }
        ids.extend(found);
    }
    Ok(ids)
}

fn run(r: RunArgs) -> Result<(), String> {
    let lb = Loopback::create(&r)?;
    println!(
        "'{}' is live — select it as an input in any app. Ctrl-C to stop.",
        r.name
    );
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .map_err(|e| e.to_string())?;
    let _ = rx.recv();
    drop(lb);
    println!("\nremoved '{}'", r.name);
    Ok(())
}

/// A live virtual device. Dropping it removes the device and its tap.
struct Loopback {
    dev_id: AudioObjectID,
    tap_id: AudioObjectID,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        unsafe {
            AudioHardwareDestroyAggregateDevice(self.dev_id);
            if self.tap_id != 0 {
                AudioHardwareDestroyProcessTap(self.tap_id);
            }
        }
    }
}

impl Loopback {
    fn create(r: &RunArgs) -> Result<Self, String> {
        let uid = format!("recordr.{}", r.name);

        // Remove a stale device left behind by a crashed previous run.
        for d in get_ids(
            kAudioObjectSystemObject as u32,
            kAudioHardwarePropertyDevices,
        ) {
            if get_string(d, kAudioDevicePropertyDeviceUID).as_deref() == Some(uid.as_str()) {
                unsafe { AudioHardwareDestroyAggregateDevice(d) };
            }
        }

        // 1. Tap: one stereo mixdown of all requested apps (or the whole system).
        let mut tap_id: AudioObjectID = 0;
        let mut tap_uid = None;
        if r.system || !r.apps.is_empty() {
            let procs: Vec<_> = resolve_apps(&r.apps)?
                .into_iter()
                .map(NSNumber::new_u32)
                .collect();
            let procs = NSArray::from_retained_slice(&procs);
            let desc = unsafe {
                let alloc = CATapDescription::alloc();
                let d = if r.system {
                    CATapDescription::initStereoGlobalTapButExcludeProcesses(alloc, &NSArray::new())
                } else {
                    CATapDescription::initStereoMixdownOfProcesses(alloc, &procs)
                };
                d.setName(&NSString::from_str(&format!("{} tap", r.name)));
                d.setPrivate(false); // must be visible to the public aggregate's clients
                d.setMuteBehavior(if r.mute {
                    CATapMuteBehavior::Muted
                } else {
                    CATapMuteBehavior::Unmuted
                });
                d
            };
            check(
                unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id) },
                "create process tap (grant Audio Capture permission to your terminal?)",
            )?;
            tap_uid = Some(unsafe { desc.UUID() }.UUIDString().to_string());
        }

        // 2. Public aggregate device = tap + optional physical inputs (first --device is the clock).
        let subs = r.devices.clone();
        let dict = aggregate_description(&r.name, &uid, &subs, tap_uid.as_deref());
        let cf: &CFDictionary = unsafe { &*(Retained::as_ptr(&dict).cast()) }; // toll-free bridge
        let mut dev_id: AudioObjectID = 0;
        let st = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut dev_id)) };
        if let Err(e) = check(st, "create aggregate device") {
            if tap_id != 0 {
                unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            }
            return Err(e);
        }
        Ok(Loopback { dev_id, tap_id })
    }
}

fn aggregate_description(
    name: &str,
    uid: &str,
    subdevices: &[String],
    tap_uid: Option<&str>,
) -> Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> {
    use objc2::runtime::AnyObject;
    let s = |v: &str| -> Retained<AnyObject> { NSString::from_str(v).into() };
    let uid_dict = |v: &str| -> Retained<AnyObject> {
        NSDictionary::from_retained_objects(&[&*NSString::from_str("uid")], &[s(v)]).into()
    };
    let arr = |items: Vec<Retained<AnyObject>>| -> Retained<AnyObject> {
        NSArray::from_retained_slice(&items).into()
    };

    let mut keys = vec!["name", "uid", "private", "stacked"];
    let mut vals: Vec<Retained<AnyObject>> = vec![
        s(name),
        s(uid),
        NSNumber::new_bool(false).into(), // public: other apps can see it
        NSNumber::new_bool(false).into(),
    ];
    if let Some(main) = subdevices.first() {
        keys.extend(["master", "subdevices"]);
        vals.extend([
            s(main),
            arr(subdevices.iter().map(|d| uid_dict(d)).collect()),
        ]);
    }
    if let Some(t) = tap_uid {
        keys.extend(["taps", "tapautostart"]);
        vals.extend([arr(vec![uid_dict(t)]), NSNumber::new_bool(true).into()]);
    }
    let keys: Vec<_> = keys.into_iter().map(NSString::from_str).collect();
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    NSDictionary::from_retained_objects(&keys, &vals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_run_args() {
        let r = parse_run(&v(
            "Zoom --app com.spotify.client --app 123 --device mic --mute",
        ))
        .unwrap();
        assert_eq!(
            r,
            RunArgs {
                name: "Zoom".into(),
                apps: v("com.spotify.client 123"),
                devices: v("mic"),
                mute: true,
                system: false
            }
        );
        assert!(parse_run(&v("Zoom")).is_err(), "nothing to route");
        assert!(parse_run(&v("Zoom --bogus")).is_err());
        assert!(parse_run(&v("Zoom --app")).is_err());
        assert!(parse_run(&[]).is_err());
    }
}
