# recordr

Loopback-style virtual audio devices for macOS, in Rust. It routes app audio into a virtual input that any app (Zoom, OBS, Logic…) can select.

It uses Core Audio process taps and a public aggregate device, so you don't install a driver. It needs **macOS 14.2+**.

```sh
cargo build --release
recordr                                                   # open the window
recordr list                                              # audio processes + device UIDs
recordr run Podcast --app com.spotify.client              # Spotify -> "Podcast" input
recordr run Call --app com.google.Chrome --device BuiltInMicrophoneDevice
recordr run Everything --system --mute                    # all system audio, silenced on speakers
```

Devices you set up in the window are saved in `~/Library/Application Support/recordr` and reopen stopped. A device exists while it's started in the window or while `recordr run` is running. Stop, quit, or Ctrl-C removes it.

On the first run, macOS asks for **Audio Capture** permission for your terminal. If you get silence, check System Settings → Privacy & Security → Screen & System Audio Recording.

## Mac app
```sh
./bundle.sh                              # -> target/recordr.app
cp -R target/recordr.app /Applications/
```
macOS then asks for Audio Capture permission for *recordr* itself instead of your terminal. The ad-hoc signature only works on this Mac. To share the app, sign it with a Developer ID and notarize it.

## Limits (vs Loopback)
- `--app` only matches processes that have already opened audio (`recordr list`). Apps launched later aren't picked up.
- `--device` inputs show up as extra channels. They aren't mixed into the stereo pair.
- There's no per-source volume and no channel mapping.
