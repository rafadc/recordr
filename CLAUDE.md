# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

recordr is a Loopback-style virtual audio device app for macOS (14.2+), in Rust. It has a CLI and an egui window. There's no driver: it uses Core Audio process taps plus aggregate devices.

Developer docs go in this file; user-facing docs (install, usage, permissions, limits) go in `README.md`. Keep each where it belongs when updating.

## Commands

`cargo` is not on the shell PATH here; prefix commands with:
```sh
export PATH="$(dirname $(rustup which cargo)):$PATH"
```

```sh
cargo build
cargo test                       # one unit test: parses_run_args (src/main.rs)
cargo test parses_run_args       # single test by name
cargo fmt
cargo run -- list                # audio processes, device UIDs, inputs with transport fourcc
cargo run -- run <name> --app <bundle-id> [--system] [--device <uid>] [--mute]
cargo run                        # the window
```

Build, install, and relaunch the real app (`cargo run` doesn't exercise these):
```sh
./bundle.sh && pkill -f "recordr.app/Contents/MacOS/recordr"; rm -rf /Applications/recordr.app && cp -R target/recordr.app /Applications/ && open /Applications/recordr.app
```
Only the installed `.app` gets the TCC permissions (Audio Capture, Microphone), the login item, and the icon. The ad-hoc codesign only works on this Mac; distributing the app needs a Developer ID signature plus notarization.

Regenerate the icon after editing `assets/icon.svg`: `assets/make-icon.sh` (writes `assets/AppIcon.icns`).

## Architecture

- **`src/main.rs`**: the CLI entry point and everything Core Audio.
  - Property helpers: `get`, `get_ids`, `get_string`, `check`.
  - `processes()`: audio processes with their bundle id and playing state.
  - `input_devices()`: input devices as uid, name, and transport type.
  - `Loopback`: a live virtual device.
    1. A `CATapDescription` stereo mixdown of the selected processes, or a global tap for `--system`.
    2. An aggregate device built from `aggregate_description` that includes the tap plus optional physical inputs. The first input device is the clock.
    3. `Drop` destroys the aggregate and the tap. Devices exist only while recordr runs.
  - The `recordr.<name>` uid prefix marks our own devices. Stale ones from a crash are removed on create, and the GUI filters them out of the inputs.
- **`src/meter.rs`**: live L/R peak meters.
  - For an app: a *private, unmuted* tap and a *private* aggregate. For an input: an IOProc directly on the device.
  - The audio thread writes peaks into `AtomicU32` f32 bits, and the GUI reads them with `levels()`.
  - It assumes Float32 samples.
- **`src/gui.rs`**: the eframe/egui window.
  - Device configs (`RunArgs`) persist through eframe storage in `~/Library/Application Support/recordr`. Storage keys: `devices`, `running`, `login_item_registered`, `show_continuity`.
  - On launch:
    - Devices that were running at quit restart.
    - The app registers itself as a login item once through `SMAppService`, only when running from a `.app`.
  - Sources render as icon tiles:
    - App names and icons come from `NSRunningApplication`. For a helper process, the bundle id is trimmed to find its parent app.
    - Input icons are SF Symbols picked by Core Audio transport type, then tinted.
  - Meters run whenever the window isn't minimized or occluded, and the source lists refresh every 2 s. Keep the tile order stable; don't sort by "playing".
- **`build.rs`** embeds `Info.plist` into the binary (needed for TCC prompts even outside the bundle). `bundle.sh` also copies it into the `.app`.

## Gotchas

- New hardware or privacy access needs its usage string in `Info.plist`, or macOS denies it silently.
- Never open "remote" inputs for metering: Continuity (`ccwl`/`ccwd`, iPhone) or Bluetooth (`blue`/`blea`). Opening them wakes the iPhone or drops AirPods into call mode. See `is_remote` / `is_continuity` in `src/gui.rs`. Continuity inputs are hidden unless the "Show Continuity sources" setting is on.
- `--app` matches bundle-id prefixes, but only processes that have already opened audio. Apps launched later aren't picked up.
- Mark deliberate shortcuts with `// ponytail:` comments that name the limit.

## Workflow

Work on a branch and open a pull request against `main` (`gh pr create`); don't push to `main` directly. The PR description must explain **why** the change is made (the problem or motivation), not just what changed.
