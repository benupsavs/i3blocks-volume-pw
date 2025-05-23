mod protocol;
use protocol::*;

use std::{error::Error, io::{self, BufRead, BufReader, Write}, process::{Command, Stdio, ChildStdin}, sync::{mpsc::{self, Receiver, Sender}, Mutex}, thread::{self, JoinHandle}};
use lazy_static::lazy_static;

use envconfig::Envconfig;
use regex::Regex;
use std::time::{Duration, Instant};

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
    #[envconfig(from = "PRINT_HEADER", default="false")]
    pub print_header: bool,
    #[envconfig(from = "USE_WOB", default="false")]
    pub use_wob: bool,
}

lazy_static! {
    static ref RE_MUTE: Regex = Regex::new(r"^\t+?Mute: (\w+)").unwrap();
    static ref RE_STATE: Regex = Regex::new(r"^\t+?State: (\w+)").unwrap();
    static ref RE_VOLUME: Regex = Regex::new(r"^\t+?Volume:\s*(?:front-left|mono).*?\d*?(\d+?)%").unwrap();
    static ref RE_DEVICE_NAME_1: Regex = Regex::new(r#"^\t\tnode\.nick\s=\s"([^"]+?)""#).unwrap();
    static ref RE_DEVICE_NAME_2: Regex = Regex::new(r#"^\t\tdevice\.alias\s=\s"([^"]+?)""#).unwrap();
    static ref RE_SINK_NAME: Regex = Regex::new(r#"^\tName: (.+)$"#).unwrap();
}

/// Gets the node name of the default PipeWire audio sink.
fn get_default_sink_node_name() -> Option<String> {
    match Command::new("pactl")
        .arg("get-default-sink")
        .output() {
            Ok(output) if output.status.success() => {
                let name = String::from_utf8_lossy(&output.stdout).trim_end().to_string();
                if name.is_empty() { None } else { Some(name) }
            }
            _ => None,
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
fn fetch_sink_status() -> Result<Vec<String>, Box<dyn Error>> {
    let output = Command::new("pactl")
        .args(["list", "sinks"])
        .output()?;
    if !output.status.success() {
        return Err(format!("pactl list sinks failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    let full_output = String::from_utf8_lossy(&output.stdout);
    Ok(full_output.lines().map(|l| l.to_owned()).collect())

}

/// Gets the output to be displayed to the user.
/// The first element of the tuple is the status line,
/// and the second element is the volume percentage to display to the user.
pub fn get_output(default_sink_node: Option<String>, lines: Vec<String>, include_device_name: bool) -> Result<(String, u16), Box<dyn Error>> {
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
        return Ok((String::new(), 0)); // No sinks found
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
    if s.is_none() && !sinks.is_empty() {
        s = Some(&sinks[0]);
    } else if s.is_none() {
        return Ok((String::new(), 0)); // No suitable sink
    }
    let s = s.unwrap();

    let mut output = Output::default();

    let icon_char = if s.mute {
        CHAR_AUDIO_MUTED
    } else if s.volume_percent <= 20 {
        CHAR_AUDIO_LOW
    } else if s.volume_percent <= 60 {
        CHAR_AUDIO_MEDIUM
    } else {
        CHAR_AUDIO_HIGH
    };

    let base_text = format!("{} {}%", icon_char, s.volume_percent);

    if include_device_name && !s.device_name.is_empty() {
        output.full_text = format!("{} [{}]", base_text, s.device_name);
        output.short_text = Some(base_text);
    } else {
        output.full_text = base_text;
        output.short_text = None;
    }

    if s.volume_percent > 100 {
        output.urgent = Some(true);
    }

    let json_output = serde_json::to_string(&output)
        .map_err(|e| format!("Failed to serialize output: {}", e))?;
    Ok((json_output, if s.mute { 0 } else { s.volume_percent }))
}

/// PipeWire audio control API.
pub struct Control {
    pactl_event_thread_handle: Mutex<Option<JoinHandle<()>>>,
    event_tx: Sender<u8>,
    event_rx: Option<Receiver<u8>>,
    active: bool,
    show_device_name: bool,
    config: Config,
    previous_line: String,
    wob_stdin: Mutex<Option<ChildStdin>>,
}

impl Control {
    /// Gets the control for the given mixer.
    pub fn new(config: Config) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        Self {
            pactl_event_thread_handle: Mutex::new(None),
            event_tx,
            event_rx: Some(event_rx),
            active: true,
            show_device_name: config.show_device_name,
            config,
            previous_line: String::new(),
            wob_stdin: Mutex::new(None),
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
    pub fn subscribe(&mut self) -> Result<(), Box<dyn Error>> {
        let tx_for_pactl_thread = self.event_tx.clone();
        let jh: JoinHandle<()> = thread::Builder::new()
            .name("pactl_subscribe_listener".to_string())
            .stack_size(16 * 1024)
            .spawn(move || {
                let mut failures = Vec::new();
                loop {
                    let cmd_result = Command::new("pactl")
                        .arg("subscribe")
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped()) // Capture stderr for better error reporting
                        .spawn();

                    match cmd_result {
                        Ok(mut child) => {
                            if let Some(stdout) = child.stdout.take() {
                                let mut reader = BufReader::new(stdout);
                                let mut line = String::new();
                                loop {
                                    match reader.read_line(&mut line) {
                                        Ok(0) => break, // EOF, pactl exited
                                        Ok(_) => {
                                            if line.contains("change") {
                                                if tx_for_pactl_thread.send(0).is_err() {
                                                    // Receiver is gone, main loop likely exited
                                                    _ = child.wait(); // Ensure child process is cleaned up
                                                    return;
                                                }
                                            }
                                            line.clear();
                                        }
                                        Err(e) => {
                                            eprintln!("Error reading from pactl subscribe: {}", e);
                                            break; // Error, break to retry spawning pactl
                                        }
                                    }

                                }
                            } else {
                                eprintln!("pactl subscribe: stdout not available.");
                            }
                            // Ensure that the child process is cleaned up and check status
                            match child.wait() {
                                Ok(status) => if !status.success() {
                                    if let Some(code) = status.code() {
                                        eprintln!("'pactl subscribe' exited with code: {}", code);
                                    } else {
                                        eprintln!("'pactl subscribe' terminated by signal");
                                    }
                                }
                                Err(e) => eprintln!("Failed to wait on 'pactl subscribe': {}", e),
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to spawn 'pactl subscribe': {}", e);
                            // Track failures and only allow 3 in the past second to prevent busy-looping
                            let now = Instant::now();
                            failures.retain(|&t| now.duration_since(t) < Duration::from_secs(1));
                            failures.push(now);
                            if failures.len() > 3 {
                                eprintln!("'pactl subscribe' failed too many times, listener thread exiting.");
                                return; // Exit thread
                            }
                            thread::sleep(Duration::from_millis(500)); // Wait before retrying
                        }
                    }
                // Thread ends if loop is exited (e.g., due to too many failures or sender dropping)
                }
            })
            .map_err(|e| Box::new(e) as Box<dyn Error>)?;

        *self.pactl_event_thread_handle.lock().unwrap() = Some(jh);
        Ok(())
    }

    /// Receives events and writes updates to stdout.
    pub fn refresh_loop(&mut self) {
        // Take the receiver. If it's already taken, something is wrong.
        let event_rx = self.event_rx.take().expect("event_rx already taken in refresh_loop");

        let mut stdout = io::stdout().lock();
        if self.config.print_header {
            // Print i3bar header
            let header = Header {
                version: 1,
                click_events: Some(true),
                ..Default::default()
            };
            let header_str = serde_json::to_string(&header).unwrap();
            if writeln!(stdout, "{}", header_str).is_err() { self.active = false; return; }
            if stdout.flush().is_err() { self.active = false; return; }
        }

        if self.config.use_wob {
            let spawn_result = Command::new("wob")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = spawn_result {
                *self.wob_stdin.lock().unwrap() = child.stdin.take();
            } else {
                eprintln!("Failed to spawn wob: {:?}", spawn_result);
            }
        }

        let mut first_update = true;
        let mut last_volume = 0u16;

        while self.active {
            match event_rx.recv_timeout(Duration::from_secs(1)) { // Use recv_timeout to periodically check self.active
                Ok(button) => {
                    let mut action_affects_display = false;

                    if button == 2 { // Mute toggle
                        _ = self.toggle_mute().map_err(|e| eprintln!("Error toggling mute: {}", e));
                        action_affects_display = true;
                    } else if button == 4 { // Volume up
                        _ = self.adjust_volume(self.config.audio_delta as i8).map_err(|e| eprintln!("Error adjusting volume up: {}", e));
                        action_affects_display = true;
                    } else if button == 5 { // Volume down
                        _ = self.adjust_volume(-(self.config.audio_delta as i8)).map_err(|e| eprintln!("Error adjusting volume down: {}", e));
                        action_affects_display = true;
                    } else if button == 1 { // Launch volume control app
                        _ = Command::new(&self.config.volume_control_app).spawn().map_err(|e| eprintln!("Error spawning volume app: {}", e));
                        // This action itself doesn't require immediate refresh from this block
                    } else if button == 3 { // Toggle device name display
                        self.show_device_name = !self.show_device_name;
                        action_affects_display = true;
                    }

                    // Perform update if:
                    // 1. It's the very first update.
                    // 2. A pactl event occurred (button == 0, meaning a generic change).
                    // 3. A click action that affects the display was taken.
                    if first_update || button == 0 || action_affects_display {
                        match fetch_sink_status() {
                            Ok(lines) => {
                                match get_output(get_default_sink_node_name(), lines, self.show_device_name) {
                                    Ok((line_str, volume_percent)) => {
                                        // Update stdout if content changed or if it's the first update
                                        if line_str != self.previous_line || first_update {
                                            if writeln!(stdout, "{}", line_str).is_err() { self.active = false; break; }
                                            if io::stdout().flush().is_err() { self.active = false; break; }
                                            self.previous_line = line_str;
                                        }
                                        // Update wob if volume changed (and not the first update)
                                        if last_volume != volume_percent && !first_update {
                                            if let Some(wob_stdin_guard) = self.wob_stdin.lock().unwrap().as_mut() {
                                                let vol_str = format!("{}\n", volume_percent);
                                                if wob_stdin_guard.write_all(vol_str.as_bytes()).is_err() || wob_stdin_guard.flush().is_err() {
                                                    eprintln!("Error writing to wob, disabling wob output.");
                                                    *self.wob_stdin.lock().unwrap() = None; // Stop trying to use wob
                                                }
                                            }
                                        }
                                        last_volume = volume_percent;
                                        if first_update { first_update = false; }
                                    }
                                    Err(e) => eprintln!("Error getting output: {}", e),
                                }
                            }
                            Err(e) => eprintln!("Error fetching sink status: {}", e),
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => { /* Continue loop to check self.active */ }
                Err(mpsc::RecvTimeoutError::Disconnected) => { self.active = false; break; /* All senders dropped */ }
            }
        }
    }

    pub fn tx(&self) -> Sender<u8> {
        self.event_tx.clone()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.active = false;
        // Dropping event_tx will signal sender threads.
        // Ensure the pactl listener thread is joined.
        if let Some(jh) = self.pactl_event_thread_handle.lock().unwrap().take() {
            _ = jh.join().map_err(|e| eprintln!("Error joining pactl_subscribe_listener thread: {:?}", e));
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
        let (status_line, volume) = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, true).unwrap();

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
        assert!(volume == 40);
    }

    #[test]
    fn no_device_name() {
        let response = include_str!("../tests/active.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let (status_line, volume) = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, false).unwrap();

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(!status_line.contains("Creative USB Headset"));
        assert!(volume == 40);
    }

    #[test]
    fn inactive_output() {
        let response = include_str!("../tests/inactive.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let (status_line, volume) = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, true).unwrap();

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
        assert_eq!(volume, 40);
    }
    #[test]
    fn empty_sink_list() {
        let lines: Vec<String> = Vec::new();
        let (status_line, volume) = get_output(None, lines, false).unwrap();
        assert!(status_line.is_empty());
        assert_eq!(volume, 0);
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
            full_text: "full text!".into(),
            ..Default::default()
        };

        let r = serde_json::to_string(&o).unwrap();
        assert!(!r.is_empty());
    }
}
