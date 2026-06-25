# i3blocks-volume-pw
Pipewire volume display and control for i3blocks.

## Why?
Subscribes to volume/device updates and listens for click events at the same time. Uses few resources, but receives instant updates to volume and playback device events.

It holds a single persistent connection to the PulseAudio-compatible API (pipewire-pulse) via `libpulse`, reacting to native subscribe events rather than polling — so it sits idle when nothing changes.

## Usage
Left click opens a program of your choosing. Default is `pavucontrol`. Change this using the `VOLUME_CONTROL_APP` environment variable.
Middle click toggles mute for the playback device.
Right click toggles display of the playback device.
Mouse wheel raises and lowers the playback volume. The delta is configured using the `AUDIO_DELTA` env variable, and should be represented as an integer percentage.

## Build (requires Rust)
Requires the PulseAudio client library and headers at build time (`libpulse`):
- Arch: `pacman -S libpulse`
- Debian/Ubuntu: `apt install libpulse-dev`
- Fedora: `dnf install pulseaudio-libs-devel`

Check out sources
```bash
cd /path/to/i3blocks-volume-pw
cargo build --release
cp target/release/i3blocks-volume-pw ~/.config/i3blocks/
```

## Configure
```
[i3blocks-volume-pw]
command=env AUDIO_DELTA=2 $HOME/.config/i3blocks/i3blocks-volume-pw
interval=persist
format=json
```

Log out and back in.
