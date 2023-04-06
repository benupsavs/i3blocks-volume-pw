mod protocol;
use protocol::*;

use std::{process::{Command, Stdio}, error::Error, thread::{self, JoinHandle}, io::{BufReader, BufRead}, sync::{mpsc::{Sender, Receiver, self}, Mutex}, ops::Index};
use lazy_static::lazy_static;

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
    #[envconfig(from = "VOLUME_CONTROL_APP", default="pavucontrol")]
    pub volume_control_app: String,
    #[envconfig(from = "SHOW_DEVICE_NAME", default="false")]
    pub show_device_name: bool,
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

pub fn get_output(default_sink_node: Option<String>, lines: Vec<String>, include_device_name: bool) -> String {
    lazy_static!(
        static ref RE_MUTE:        Regex = Regex::new(r"^\t+?Mute: (\w+)").unwrap();
        static ref RE_STATE:       Regex = Regex::new(r"^\t+?State: (\w+)").unwrap();
        static ref RE_VOLUME:      Regex = Regex::new(r"^\t+?Volume:\s*(?:front-left|mono).*?\d*?(\d+?)%").unwrap();
        static ref RE_DEVICE_NAME: Regex = Regex::new(r#"^\t\tnode\.nick\s=\s"([^"]+?)""#).unwrap();
        static ref RE_SINK_NAME:   Regex = Regex::new(r#"^\tName: (.+)$"#).unwrap();

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

pub struct Control {
    sub: Mutex<Option<Subscription>>,
    active: bool,
    show_device_name: bool,
    config: Config,
    previous_line: String,
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
            show_device_name: config.show_device_name,
            config,
            previous_line: String::new(),
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
        let jh: JoinHandle<()> = thread::Builder::new().name("change listener".to_string()).stack_size(16 * 1024).spawn(move || {
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
                            if line.contains("change") && tx.send(0).is_err() {
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
                    } else if button == 1 {
                        _ = Command::new(&self.config.volume_control_app).spawn()
                    } else if button == 3 {
                        self.show_device_name = !self.show_device_name;
                    }
                    let l = get_output(get_default_sink_node_name(), fetch_sink_status(), self.show_device_name);
                    if l != self.previous_line {
                        println!("{l}");
                        self.previous_line = l;
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

pub fn parse_click(json: &str) -> serde_json::Result<Click> {
    serde_json::from_str(json)
}

pub struct Port<'a1> {
    pub name: &'a1 str,
    pub priority: i16,
    pub available: bool,
}

/// Parses the port string.
///
/// Ex: `analog-output-headphones: Headphones (type: Headphones, priority: 9900, availability group: Legacy 4, available)`
pub fn parse_port<'a1>(port_str: &'a1 str) -> Option<Port<'a1>> {
    let mut name: Option<&'a1 str> = None;
    let mut priority: Option<i16> = None;
    let mut available: Option<bool> = None;

    let f = port_str.find('(');
    if let Some(pf) = f {
        let f = port_str.find(')');
        if let Some(pt) = f {
            let tags_str = &port_str[pf..pt];
            let kvs = tags_str.split(", ");
            for kv_str in kvs {
                let kv: Vec<&str> = Vec::from_iter(kv_str.split(": "));
                if kv.len() == 2 {
                    if kv[0] == "type" {
                        name = Some(kv[1]);
                    } else if kv[0] == "priority" {
                        priority = kv[1].parse::<i16>().ok();
                    } else if kv[0] == "availability group" {
                        available = Some(!kv[1].contains("not available"));
                    }
                }
            }
        }
    }

    if name.is_some() && priority.is_some() && available.is_some() {
        return Some(Port { name: name.unwrap(), priority: priority.unwrap(), available: available.unwrap() });
    }

    None
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
