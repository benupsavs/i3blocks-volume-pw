use std::{process::{Command, Stdio}, error::Error, thread::{self, JoinHandle}, io::{BufReader, BufRead}, sync::{mpsc::{Sender, Receiver, self}, Mutex}};
use lazy_static::lazy_static;
use serde::{Serialize, Deserialize};

use envconfig::Envconfig;
use regex::Regex;

const CHAR_AUDIO_MUTED:  char = '\u{1F507}';
const CHAR_AUDIO_LOW:    char = '\u{1F508}';
const CHAR_AUDIO_MEDIUM: char = '\u{1F509}';
const CHAR_AUDIO_HIGH:   char = '\u{1F50A}';

#[derive(Envconfig)]
pub struct Config {
    #[envconfig(from = "AUDIO_DELTA", default="5")]
    pub audio_delta: u8,
}

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

#[derive(Clone)]
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

fn fetch_sink_status() -> Vec<String> {
    let output = Command::new("pactl")
        .args(["list", "sinks"])
        .output()
        .unwrap();
    let full_output = String::from_utf8_lossy(&output.stdout).to_string();
    full_output.lines().map(|l| l.to_owned()).collect()
}

pub fn get_output(default_sink_node: Option<String>, lines: Vec<String>) -> String {
    lazy_static!(
        static ref RE_MUTE: Regex = Regex::new(r"^\t+?Mute: (\w+)").unwrap();
        static ref RE_STATE: Regex = Regex::new(r"^\t+?State: (\w+)").unwrap();
        static ref RE_VOLUME: Regex = Regex::new(r"^\t+?Volume:\s*(?:front-left|mono).*?\d*?(\d+?)%").unwrap();
        static ref RE_DEVICE_NAME: Regex = Regex::new(r#"^\t\tnode\.nick\s=\s"([^"]+?)""#).unwrap();
        static ref RE_SINK_NAME: Regex = Regex::new(r#"^\tName: (.+)$"#).unwrap();

    );
    let mut sinks = Vec::new();
    let mut sink = Sink{
        volume_percent: 0,
        device_name: String::new(),
        mute: false,
        active: false,
        got_mute: false,
        got_volume: false,
        got_device_name: false,
        sink_name: String::new(),
        got_sink_name: false,
    };
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
            if let Some(caps) = RE_DEVICE_NAME.captures(&line) {
                sink.device_name = caps[1].to_string();
                sink.got_device_name = true;
                continue;
            }
        }

        if !sink.got_sink_name {
            if let Some(caps) = RE_SINK_NAME.captures(&line.trim_end()) {
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
        let d = default_sink_node.unwrap();
        for sink in &sinks {
            if sink.sink_name == d {
                s = Some(sink);
                break;
            }
        }
    }
    // If no active or default sinks, use the first one
    if s.is_none() {
        s = Some(&sinks[0]);
    }
    let s = s.unwrap();

    let mut output = String::new();
    if s.mute {
        output.extend([CHAR_AUDIO_MUTED, ' ']);
    } else if s.volume_percent <= 20 {
        output.extend([CHAR_AUDIO_LOW, ' ']);
    } else if s.volume_percent <= 60 {
        output.extend([CHAR_AUDIO_MEDIUM, ' ']);
    } else {
        output.extend([CHAR_AUDIO_HIGH, ' ']);
    }

    output.extend([s.volume_percent.to_string()]);
    output.extend(['%', ' ']);
    output.extend(['[']);
    output.extend([s.device_name.clone()]);
    output.extend([']']);

    output
}

pub struct Control {
    sub: Mutex<Option<Subscription>>,
    active: bool,
    config: Config,
}

struct Subscription {
    j: JoinHandle<()>,
    tx: Sender<u8>,
    rx: Receiver<u8>,
}

impl Control {
    /// Gets the control for the given mixer.
    pub fn new(config: Config) -> Self {
        Self {
            sub: Mutex::new(None),
            active: true,
            config,
        }
    }

    /// Adjusts the volume by the given amount.
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
        let jh: JoinHandle<()> = thread::spawn(move || {
            let tx = tx2;
            let output_result = Command::new("pactl")
                .arg("subscribe")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn();
            if let Ok(mut output) = output_result {
                let mut child_out = BufReader::new(output.stdout.as_mut().unwrap());
                let mut line = String::new();
                loop {
                    match child_out.read_line(&mut line) {
                        Ok(_) => {
                            if line.contains("change") {
                                if let Err(_) = tx.send(0) {
                                    return;
                                }
                            }
                        },
                        Err(_) => return,
                    }
                    line.clear();
                }
            }
        });
        let sub = Subscription{
            j: jh,
            rx,
            tx,
        };

        *self.sub.lock().unwrap() = Some(sub);
    }

    pub fn refresh_loop(&self) {
        if let Ok(ref r1) = &self.sub.lock() {
            let r2 = r1.as_ref();
            if let Some(sub) = r2 {
                let rx = &sub.rx;
                while let Ok(button) = rx.recv() {
                    if button == 2 {
                        _ = self.toggle_mute();
                    } else if button == 4 {
                        _ = self.adjust_volume(self.config.audio_delta as i8);
                    } else if button == 5 {
                        _ = self.adjust_volume(-(self.config.audio_delta as i8));
                    }
                    let l = get_output(get_default_sink_node_name(), fetch_sink_status());
                    println!("{l}");
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

fn parse_click(json: &str) -> serde_json::Result<Click> {
    serde_json::from_str(json)
}

#[derive(Serialize, Deserialize)]
struct Click {
    name: String,
    instance: String,
    button: u8,
    modifiers: Vec<String>,
    x: i16,
    y: i16,
    relative_x: i16,
    relative_y: i16,
    output_x: i16,
    output_y: i16,
    width: u16,
    height: u16,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn active_output() {
        let response = include_str!("../tests/active.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let status_line = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines);

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
    }

    #[test]
    fn inactive_output() {
        let response = include_str!("../tests/inactive.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let status_line = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines);

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
    }

    #[test]
    fn parse_click_ethernet() {
        let click_json = include_str!("../tests/click.json");
        let click = parse_click(click_json).unwrap();
        assert_eq!("ethernet", click.name);
        assert_eq!("eth0", click.instance);
        assert_eq!(1 as u8, click.button);
        assert_eq!(2, click.modifiers.len());
        assert_eq!("Shift", click.modifiers[0]);
        assert_eq!("Mod1", click.modifiers[1]);
        assert_eq!(1925 as i16, click.x);
        assert_eq!(1400 as i16, click.y);
        assert_eq!(12 as i16, click.relative_x);
        assert_eq!(8 as i16, click.relative_y);
        assert_eq!(5 as i16, click.output_x);
        assert_eq!(1400 as i16, click.output_y);
        assert_eq!(50 as u16, click.width);
        assert_eq!(22 as u16, click.height);
    }
}
