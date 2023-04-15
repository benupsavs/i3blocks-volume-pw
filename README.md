# i3blocks-volume-pw
Pipewire volume display and control for i3blocks.

## Why?
Subscribes to volume/device updates and listens for click events at the same time. Uses few resources, but receives instant updates to volume and playback device events.

## Usage
Left click opens a program of your choosing. Default is `pavucontrol`. Change this using the `VOLUME_CONTROL_APP` environment variable.
Middle click toggles mute for the playback device.
Right click toggles display of the playback device.
Mouse wheel raises and lowers the playback volume. The delta is configured using the `AUDIO_DELTA` env variable, and should be represented as an integer percentage.

## Build (requires Rust)
Check out sources
```bash
cd /path/to/i3blocks-volume-pw
cargo build
cp target/release/i3blocks-volume-pw ~/.config/i3blocks/
```

## Configure
Add the block config to `~/.config/i3blocks/config`:

```
[i3blocks-volume-pw]
command=env AUDIO_DELTA=2 $HOME/.config/i3blocks/i3blocks-volume-pw
interval=persist
format=json
```

Reload your WM or log out and back in or reboot.
