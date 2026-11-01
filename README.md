# recordr

recordr creates virtual audio inputs on your Mac. It routes sound from your apps and microphones into an input that any app (Zoom, OBS, Logic…) can pick as a microphone. You don't install a driver. It needs **macOS 14.2 or later**.

## Install

Download the latest nightly build: [recordr.dmg](https://github.com/rafadc/recordr/releases/download/nightly/recordr.dmg) (Apple Silicon). Open it and drag recordr into Applications.

recordr isn't notarized, so macOS blocks it the first time you open it. Go to System Settings → Privacy & Security and click **Open Anyway**. Or run:

```sh
xattr -dr com.apple.quarantine /Applications/recordr.app
```

Each nightly build has a new signature, so macOS asks for the permissions again after an update.

To build it from source instead (Rust needed):

```sh
./bundle.sh
cp -R target/recordr.app /Applications/
```

Open recordr from Applications, Spotlight, or Raycast. The first time you open it, recordr adds itself to your login items so your devices come back after a restart. To turn that off, go to System Settings → General → Login Items.

## Using it

1. **Create a device.** Click **+ New device** in the sidebar and give it a name.
2. **Pick sources.** Click the tiles to select them:
   - **Apps**: apps that have played sound since they launched. System processes are under **Other processes**.
   - **Input devices**: microphones and other inputs. They're added as extra channels.
   - **All system audio**: captures everything instead of specific apps.
   - **Mute sources on speakers**: you stop hearing the selected apps, but they still go into the device.
3. **Start it.** Click **▶ Start**. The device appears as an input in every app. To change its sources, stop it first.
4. **Record it (optional).** Click **● Record** to save the device's audio as a WAV file. It starts the device if it isn't running. Click **■ Stop recording** to finish the file. Recordings go to `~/Music/recordr`; change the folder in Settings.

Each tile shows live L/R level bars, so you can see what's making sound. The meters run while the window is visible, and macOS shows its recording indicators during that time. They stop when you minimize the window.

Selected apps that aren't running show up under **Closed apps**. Click one to remove it from the device.

Devices and their sources are saved. Devices that were running when you quit start again when recordr opens.

### Settings

To open settings, click **⚙ Settings** at the bottom of the sidebar.
- **Recordings → Folder**: where **● Record** saves files. The default is `~/Music/recordr`.
- **Show Continuity sources** (off by default): lists your iPhone's microphone as an input. iPhone and Bluetooth microphones never get a level meter, because opening them wakes the iPhone or switches AirPods to call quality.

## Permissions

macOS asks for two permissions the first time recordr needs them:
- **Audio Capture**, to capture app audio. If your devices are silent, check System Settings → Privacy & Security → Screen & System Audio Recording.
- **Microphone**, for input levels and for mixing in microphones.

**⚙ Settings → Permissions** shows whether each one is granted. If one is missing, the sidebar shows **⚠ Settings** and the device page shows a warning. Use **Open System Settings** to grant it, then relaunch recordr.

## Command line

The app's binary also works from a terminal. Put it on your PATH once with **⚙ Settings → Install command line tool**, or by hand:

```sh
sudo ln -s /Applications/recordr.app/Contents/MacOS/recordr /usr/local/bin/recordr
```

Then:

```sh
recordr list                                              # audio processes, devices and inputs
recordr run Podcast --app com.spotify.client              # Spotify -> "Podcast" input
recordr run Call --app com.google.Chrome --device BuiltInMicrophoneDevice
recordr run Everything --system --mute                    # all system audio, silenced on speakers
```

The device exists until you press Ctrl-C. From a terminal, macOS asks for the permissions on behalf of your terminal app (Terminal, iTerm…), not recordr.

## Limits

- An app only appears once it has played sound. An app launched after you start a device isn't captured until you stop and start the device again.
- Input devices are added as extra channels. They aren't mixed into the stereo pair.
- There's no per-source volume and no channel mapping.
- It isn't notarized, so macOS asks you to allow it the first time (see Install). Builds are Apple Silicon only.
