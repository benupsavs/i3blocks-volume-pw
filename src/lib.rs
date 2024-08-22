mod protocol;
use protocol::*;

use std::{process::{Command, Stdio}, error::Error, thread::{self, JoinHandle}, io::{BufReader, BufRead}, sync::{mpsc::{Sender, Receiver, self}, Mutex}};
use lazy_static::lazy_static;

use envconfig::Envconfig;
use regex::Regex;

/// Character representing muted audio.
const CHAR_AUDIO_MUTED:  char = '\u{1F507}';
/// Character representing a low volume level.
const CHAR_AUDIO_LOW:    char = '\u{1F508}';
/// Character representing a medium volume level.
const CHAR_AUDIO_MEDIUM: char = '\u{1F509}';
/// Character representing a high volume level.
const CHAR_AUDIO_HIGH:   char = '\u{1F50A}';

#[derive(Envconfig)]
pub struct Config {
    #[envconfig(from = "AUDIO_DELTA", default="5")]
    pub audio_delta: u8,
    #[envconfig(from = "VOLUME_CONTROL_APP", default="pavucontrol")]
    pub volume_control_app: String,
    #[envconfig(from = "SHOW_DEVICE_NAME", default="false")]
    pub show_device_name: bool,
}

/// Gets the node name of the default PipeWire audio sink.
fn get_default_sink_node_name() -> Option<String> {
    match Command::new("pactl")
        .arg("get-default-sink")
        .output() {
            Ok(output) => {
                let full_output = String::from_utf8_lossy(&output.stdout).to_string();
                if full_output.len() <= 1 {
                    return None;
                }

                Some(full_output[0..full_output.len() - 1].to_string())
            },
            Err(_) => None,
        }
}

/// Represents a PipeWire audio sink.
#[derive(Clone, Default)]
struct Sink {
    volume_percent: u16,
    device_name: String,
    mute: bool,
    active: bool,
    got_mute: bool,
    got_volume: bool,
    got_device_name: bool,
    sink_name: String,
    got_sink_name: bool,
}

impl Sink {
    /// Reverts all fields to the default state.
    fn clear(&mut self) {
        self.volume_percent = 0;
        self.device_name = String::new();
        self.mute = false;
        self.active = false;
        self.got_mute = false;
        self.got_device_name = false;
        self.got_volume = false;
        self.sink_name = String::new();
        self.got_sink_name = false;
    }
}

/// Fetches the PipeWire audio sink status as a list of lines
/// from the output of the `pactl list sinks` command.
fn fetch_sink_status() -> Vec<String> {
    let output = Command::new("pactl")
        .args(["list", "sinks"])
        .output()
        .unwrap();
    let full_output = String::from_utf8_lossy(&output.stdout).to_string();
    full_output.lines().map(|l| l.to_owned()).collect()
}

/// Gets the output to be displayed to the user.
pub fn get_output(default_sink_node: Option<String>, lines: Vec<String>, include_device_name: bool) -> String {
    lazy_static!(
        static ref RE_MUTE: Regex = Regex::new(r"^\t+?Mute: (\w+)").unwrap();
        static ref RE_STATE: Regex = Regex::new(r"^\t+?State: (\w+)").unwrap();
        static ref RE_VOLUME: Regex = Regex::new(r"^\t+?Volume:\s*(?:front-left|mono).*?\d*?(\d+?)%").unwrap();
        static ref RE_DEVICE_NAME_1: Regex = Regex::new(r#"^\t\tnode\.nick\s=\s"([^"]+?)""#).unwrap();
        static ref RE_DEVICE_NAME_2: Regex = Regex::new(r#"^\t\tdevice\.alias\s=\s"([^"]+?)""#).unwrap();
        static ref RE_SINK_NAME: Regex = Regex::new(r#"^\tName: (.+)$"#).unwrap();

    );
    let mut sinks = Vec::new();
    let mut sink = Sink::default();
    let mut got_sink = false;
    for line in lines {
        if line.starts_with("Sink") {
            if got_sink {
                sinks.push(sink.clone());
                sink.clear();
            }
            got_sink = true;
            if sink.active {
                break;
            } else {
                continue;
            }
        }

        if let Some(caps) = RE_STATE.captures(&line) {
            sink.active = &caps[1] == "RUNNING";
            continue;
        }

        if !sink.got_mute {
            if let Some(caps) = RE_MUTE.captures(&line) {
                sink.mute = &caps[1] == "yes";
                sink.got_mute = true;
                continue;
            }
        }

        if !sink.got_volume {
            if let Some(caps) = RE_VOLUME.captures(&line) {
                sink.volume_percent = caps[1].parse::<u16>().unwrap_or_default();
                sink.got_volume = true;
                continue;
            }
        }

        if !sink.got_device_name {
            if let Some(caps) = RE_DEVICE_NAME_1.captures(&line) {
                sink.device_name = caps[1].to_string();
                sink.got_device_name = !sink.device_name.is_empty();
                continue;
            } else if let Some(caps) = RE_DEVICE_NAME_2.captures(&line) {
                sink.device_name = caps[1].to_string();
                sink.got_device_name = !sink.device_name.is_empty();
            }
        }

        if !sink.got_sink_name {
            if let Some(caps) = RE_SINK_NAME.captures(line.trim_end()) {
                sink.sink_name = caps[1].to_string();
                sink.got_sink_name = true;
                continue;
            }
        }
    }
    if got_sink {
        sinks.push(sink);
    } else {
        return String::new();
    }
    let mut s: Option<&Sink> = None;
    // Try to match the active sink
    for sink in &sinks {
        if sink.active {
            s = Some(sink);
            break;
        }
    }
    // Fallback to the default sink reported by pactl
    if s.is_none() && default_sink_node.is_some() {
        if let Some(d) = default_sink_node {
            for sink in &sinks {
                if sink.sink_name == d {
                    s = Some(sink);
                    break;
                }
            }
        }
    }
    // If no active or default sinks, use the first one
    if s.is_none() {
        s = Some(&sinks[0]);
    }
    let s = s.unwrap();

    let mut output = Output::default();

    let mut short_text = String::new();
    if s.mute {
        short_text.extend([CHAR_AUDIO_MUTED, ' ']);
    } else if s.volume_percent <= 20 {
        short_text.extend([CHAR_AUDIO_LOW, ' ']);
    } else if s.volume_percent <= 60 {
        short_text.extend([CHAR_AUDIO_MEDIUM, ' ']);
    } else {
        short_text.extend([CHAR_AUDIO_HIGH, ' ']);
    }

    short_text.extend([s.volume_percent.to_string()]);
    short_text.extend(['%']);

    let mut full_text = short_text.clone();
    output.short_text = Some(&short_text);

    if include_device_name {
        full_text.extend([' ', '[']);
        full_text.extend([s.device_name.clone()]);
        full_text.extend([']']);
    }
    output.full_text = &full_text;
    if s.volume_percent > 100 {
        output.urgent = Some(true);
    }

    serde_json::to_string(&output).unwrap_or(String::new())
}

/// Subscription to PipeWire audio events.
struct Subscription {
    j: JoinHandle<()>,
    tx: Sender<u8>,
    rx: Receiver<u8>,
}

/// PipeWire audio control API.
pub struct Control {
    sub: Mutex<Option<Subscription>>,
    active: bool,
    show_device_name: bool,
    config: Config,
    previous_line: String,
}

impl Control {
    /// Gets the control for the given mixer.
    pub fn new(config: Config) -> Self {
        Self {
            sub: Mutex::new(None),
            active: true,
            show_device_name: config.show_device_name,
            config,
            previous_line: String::new(),
        }
    }

    /// Adjusts the volume by the given amount. Negative values request lowering the volume.
    pub fn adjust_volume(&self, delta: i8) -> Result<(), Box<dyn Error>> {
        if delta == 0 {
            return Ok(())
        }
        let sign = if delta > 0 {"+"} else {"-"};
        let delta_str = format!("{sign}{}%", delta.abs());
        Command::new("pactl")
            .args(["set-sink-volume", "@DEFAULT_SINK@", &delta_str])
            .output()?;
        Ok(())
    }

    /// Mutes or unmutes the given control.
    pub fn toggle_mute(&self) -> Result<(), Box<dyn Error>> {
        Command::new("pactl")
            .args(["set-sink-mute", "@DEFAULT_SINK@", "toggle"])
            .output()?;
        Ok(())
    }

    /// Subscribes to change events.
    pub fn subscribe(&mut self) {
        let (tx, rx): (Sender<u8>, Receiver<u8>) = mpsc::channel();

        let tx2 = tx.clone();
        let jh: JoinHandle<()> = thread::Builder::new().name("change listener".to_string()).stack_size(16 * 1024).spawn(move || {
            let tx = tx2;
            let output_result = Command::new("pactl")
                .arg("subscribe")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn();
            if let Ok(mut output) = output_result {
                let mut child_out = BufReader::new(output.stdout.as_mut().unwrap());
                let mut line = String::new();
                loop {
                    match child_out.read_line(&mut line) {
                        Ok(n) => {
                            if n == 0 /* EOF */ || (line.contains("change") && tx.send(0).is_err()) {
                                return;
                            }
                        },
                        Err(_) => return,
                    }
                    line.clear();
                }
            }
        }).expect("create subscription thread");
        let sub = Subscription{
            j: jh,
            rx,
            tx,
        };

        *self.sub.lock().unwrap() = Some(sub);
    }

    /// Receives events and writes updates to stdout.
    pub fn refresh_loop(&mut self) {
        let lock = Mutex::new(0);

        if let Ok(ref r1) = &self.sub.lock() {
            let r2 = r1.as_ref();
            if let Some(sub) = r2 {
                let rx = &sub.rx;
                while let Ok(button) = rx.recv() {
                    let mut force_update = true;
                    if button == 2 {
                        _ = self.toggle_mute();
                    } else if button == 4 {
                        _ = self.adjust_volume(self.config.audio_delta as i8);
                    } else if button == 5 {
                        _ = self.adjust_volume(-(self.config.audio_delta as i8));
                    } else if button == 1 {
                        _ = Command::new(&self.config.volume_control_app).spawn()
                    } else if button == 3 {
                        self.show_device_name = !self.show_device_name;
                    } else {
                        force_update = false;
                    }
                    let mut do_update = force_update;
                    let acquired: Result<_, _>;
                    if !do_update {
                        acquired = lock.try_lock();
                        do_update = acquired.is_ok();
                    }

                    if do_update {
                        let l = get_output(get_default_sink_node_name(), fetch_sink_status(), self.show_device_name);
                        if l != self.previous_line {
                            println!("{l}");
                            self.previous_line = l;
                        }
                    }
                }
            }
        }
    }

    pub fn tx(&self) -> Option<Sender<u8>> {
        if let Ok(ref r1) = &self.sub.lock() {
            let r2 = r1.as_ref();
            if let Some(sub) = r2 {
                let tx = &sub.tx;
                return Some(tx.clone());
            }
        }

        None
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.active = false;
        if let Some(s) = self.sub.lock().unwrap().take() {
            _ = s.j.join();
        }
    }
}

/// Parses the i3block JSON that occurs as a result of a user mouse click.
pub fn parse_click(json: &str) -> serde_json::Result<Click> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn active_output() {
        let response = include_str!("../tests/active.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let status_line = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, true);

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
    }

    #[test]
    fn no_device_name() {
        let response = include_str!("../tests/active.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let status_line = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, false);

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(!status_line.contains("Creative USB Headset"));
    }

    #[test]
    fn inactive_output() {
        let response = include_str!("../tests/inactive.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let status_line = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, true);

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
    }

    #[test]
    fn parse_click_ethernet() {
        let click_json = include_str!("../tests/click.json");
        let click = parse_click(click_json).unwrap();
        let modifiers = click.modifiers.unwrap();
        assert_eq!(Some("ethernet"), click.name.as_deref());
        assert_eq!(Some("eth0"), click.instance.as_deref());
        assert_eq!(1_u8, click.button);
        assert_eq!(2, modifiers.len());
        assert_eq!("Shift", modifiers[0]);
        assert_eq!("Mod1", modifiers[1]);
        assert_eq!(1925_i16, click.x);
        assert_eq!(1400_i16, click.y);
        assert_eq!(12_i16, click.relative_x);
        assert_eq!(8_i16, click.relative_y);
        assert_eq!(5_i16, click.output_x.unwrap());
        assert_eq!(1400_i16, click.output_y.unwrap());
        assert_eq!(50_u16, click.width);
        assert_eq!(22_u16, click.height);
    }

    #[test]
    fn test_output() {
        let o = Output{
            full_text: "full text!",
            ..Default::default()
        };

        let r = serde_json::to_string(&o).unwrap();
        assert!(!r.is_empty());
    }
}
