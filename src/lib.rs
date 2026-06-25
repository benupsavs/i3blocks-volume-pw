mod protocol;
use protocol::*;

use std::{error::Error, io::{self, BufRead, BufReader, Read, Write}, process::{Command, Stdio, ChildStdin}, sync::{mpsc::{self, Receiver, Sender}, Mutex}, thread::{self, JoinHandle}};

use lazy_static::lazy_static;
use zbus::blocking::{Connection, Proxy};

use envconfig::Envconfig;
use regex::Regex;
use std::time::{Duration, Instant};
use std::collections::HashMap;

/// Character representing muted audio.
const CHAR_AUDIO_MUTED:  char = '\u{1F507}';
/// Character representing a low volume level.
const CHAR_AUDIO_LOW:    char = '\u{1F508}';
/// Character representing a medium volume level.
const CHAR_AUDIO_MEDIUM: char = '\u{1F509}';
/// Character representing a high volume level.
const CHAR_AUDIO_HIGH:   char = '\u{1F50A}';

/// Battery cache TTL and poll interval
const BT_BATTERY_TTL_SECS: u64 = 30;
const BT_POLL_INTERVAL_SECS: u64 = 31;

#[derive(Envconfig)]
pub struct Config {
    #[envconfig(from = "AUDIO_DELTA", default="5")]
    pub audio_delta: u8,
    #[envconfig(from = "VOLUME_CONTROL_APP", default="pavucontrol")]
    pub volume_control_app: String,
    #[envconfig(from = "SHOW_DEVICE_NAME", default="false")]
    pub show_device_name: bool,
    #[envconfig(from = "SHOW_BT_BATTERY", default="true")]
    pub show_bt_battery: bool,
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

/// Convert a `pactl`/PipeWire bluez output node name into a MAC address string.
/// Examples handled:
/// - `bluez_output.00_1A_7D_DA_71_13.a2dp-sink` -> `00:1A:7D:DA:71:13`
/// - `bluez_output.AA:BB:CC:DD:EE:FF.a2dp-sink` -> `AA:BB:CC:DD:EE:FF`
fn mac_from_sink_name(s: &str) -> Option<String> {
    let prefix = "bluez_output.";
    if !s.starts_with(prefix) {
        return None;
    }
    let rest = &s[prefix.len()..];
    // accept both forms: `bluez_output.<mac>.<profile>` and `bluez_output.<mac>`
    let mac_part = if let Some(dot) = rest.find('.') {
        &rest[..dot]
    } else {
        rest
    };

    // Accept underscore-separated (`00_1A_...`) or colon-separated (`00:1A:...`).
    let candidate = if mac_part.contains('_') {
        mac_part.replace('_', ":")
    } else if mac_part.contains(':') {
        mac_part.to_string()
    } else {
        return None;
    };

    // Validate the canonical MAC form XX:XX:XX:XX:XX:XX (hex pairs)
    let parts: Vec<&str> = candidate.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    for p in &parts {
        if p.len() != 2 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }

    Some(candidate.to_uppercase())
}

lazy_static! {
    static ref BT_BATTERY_CACHE: Mutex<HashMap<String, (Instant, u8)>> = Mutex::new(HashMap::new());
}

/// Build a list of candidate BlueZ device object paths for a given MAC.
/// Example for MAC `AA:BB:CC:DD:EE:FF` -> `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`,
/// `/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF`, ...
fn bluez_device_paths_for_mac(mac: &str, max_hci: usize) -> Vec<String> {
    let dev = mac.replace(':', "_").to_uppercase();
    (0..max_hci).map(|i| format!("/org/bluez/hci{}/dev_{}", i, dev)).collect()
}

/// Parse `bluetoothctl info <mac>` output for a battery percentage (best-effort).
fn parse_bluetoothctl_info_output(s: &str) -> Option<u8> {
    let re = Regex::new(r"Battery Percentage:\s*(\d+)%?").unwrap();
    if let Some(caps) = re.captures(s) {
        if let Ok(v) = caps[1].parse::<u8>() {
            return Some(v);
        }
    }
    None
}

/// Return a cached battery value if it's still fresh.
fn cached_bt_battery(mac: &str) -> Option<u8> {
    let key = mac.to_uppercase();
    let guard = BT_BATTERY_CACHE.lock().unwrap();
    if let Some((ts, v)) = guard.get(&key) {
        if Instant::now().duration_since(*ts) < Duration::from_secs(BT_BATTERY_TTL_SECS) {
            return Some(*v);
        }
    }
    None
}

/// Query BlueZ `org.bluez.Battery1` for a device MAC (best-effort).
/// Iterates available hci adapters (hci0..hciN) until a Battery1 property is found.
/// Falls back to `bluetoothctl info <mac>` if D-Bus lookups fail.
/// Returns `None` on any error or if Battery1 is not present on any adapter.
fn get_bt_battery(mac: &str) -> Option<u8> {
    // Check cache first
    if let Some(v) = cached_bt_battery(mac) {
        return Some(v);
    }

    let conn = Connection::system().ok()?;
    let key = mac.to_uppercase();

    // Try the first few adapters (80/20: most systems use hci0/hci1).
    for path in bluez_device_paths_for_mac(mac, 8) {
        if let Ok(proxy) = Proxy::new(&conn, "org.bluez", path.as_str(), "org.bluez.Battery1") {
            if let Ok(p) = proxy.get_property::<u8>("Percentage") {
                // update cache
                let mut guard = BT_BATTERY_CACHE.lock().unwrap();
                guard.insert(key.clone(), (Instant::now(), p));
                return Some(p);
            }
        }
    }

    // D-Bus failed; try CLI fallback (bluetoothctl info <mac>) with a timeout so a
    // hung BlueZ stack can't wedge the caller indefinitely.
    if let Some(out) = bluetoothctl_info_with_timeout(mac, Duration::from_secs(5)) {
        if let Some(v) = parse_bluetoothctl_info_output(&out) {
            let mut guard = BT_BATTERY_CACHE.lock().unwrap();
            guard.insert(key.clone(), (Instant::now(), v));
            return Some(v);
        }
    }

    None
}

/// Run `bluetoothctl info <mac>` but give up after `timeout`, returning its stdout
/// (or `None` on spawn failure / timeout). The child keeps running detached if it
/// overruns, but we stop waiting so the caller never blocks longer than `timeout`.
fn bluetoothctl_info_with_timeout(mac: &str, timeout: Duration) -> Option<String> {
    let mut child = Command::new("bluetoothctl")
        .arg("info")
        .arg(mac)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let (tx, rx) = mpsc::channel();
    let mut stdout = child.stdout.take()?;
    thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(timeout) {
        Ok(buf) => {
            let _ = child.wait();
            Some(buf)
        }
        Err(_) => {
            // Timed out: kill the child so it doesn't linger.
            let _ = child.kill();
            None
        }
    }
}

/// Find the MAC of the first Bluetooth sink present in `pactl list sinks` output.
fn bluez_mac_from_lines(lines: &[String]) -> Option<String> {
    for line in lines {
        if let Some(caps) = RE_SINK_NAME.captures(line.trim_end()) {
            let name = &caps[1];
            if name.starts_with("bluez_output.") {
                if let Some(mac) = mac_from_sink_name(name) {
                    return Some(mac);
                }
            }
        }
    }
    None
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
    battery: Option<u8>,
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
        self.battery = None;
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
pub fn get_output(default_sink_node: Option<String>, lines: Vec<String>, include_device_name: bool, include_bt_battery: bool) -> Result<(String, u16), Box<dyn Error>> {
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

    // Best-effort: read a cached BlueZ battery value if this looks like a Bluetooth
    // sink. The actual (potentially blocking) D-Bus/bluetoothctl lookup happens on
    // the bt-poller thread, never here on the event loop.
    let mut bt_battery: Option<u8> = None;
    if include_bt_battery && s.sink_name.starts_with("bluez_output.") {
        if let Some(mac) = mac_from_sink_name(&s.sink_name) {
            bt_battery = cached_bt_battery(&mac);
        }
    }

    // Delegate rendering to a pure helper so tests can mock the battery/formatting.
    render_sink_output(s, include_device_name, bt_battery)
}

/// Render JSON output for a single `Sink` (pure, test-friendly).
fn render_sink_output(s: &Sink, include_device_name: bool, bt_battery: Option<u8>) -> Result<(String, u16), Box<dyn Error>> {
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

    // Base text includes volume and optional BT battery indicator
    let mut base_text = format!("{} {}%", icon_char, s.volume_percent);
    if let Some(b) = bt_battery {
        base_text.push(' ');
        base_text.push_str(&format!("🔋{}%", b));
    }

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
    pub fn adjust_volume(&self, delta: i32) -> Result<(), Box<dyn Error>> {
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
                                            if line.contains("change") && !line.contains("on client") {
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
                }
            })
            .map_err(|e| Box::new(e) as Box<dyn Error>)?;

        // poller thread: only spawn when BT battery lookups are enabled in the config.
        // It performs the (potentially blocking) battery lookup here, off the event
        // loop, then signals a refresh so the freshly cached value gets displayed.
        if self.config.show_bt_battery {
            let tx_poll = self.event_tx.clone();
            thread::Builder::new().name("bt-poller".to_string()).spawn(move || {
                loop {
                    if let Ok(lines) = fetch_sink_status() {
                        if let Some(mac) = bluez_mac_from_lines(&lines) {
                            // Warms BT_BATTERY_CACHE; get_output only reads the cache.
                            let _ = get_bt_battery(&mac);
                        }
                    }
                    if tx_poll.send(0).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_secs(BT_POLL_INTERVAL_SECS));
                }
            }).expect("create poller thread");
        }

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
            // Block for the first event (with a 1s timeout so we can re-check active).
            let first = match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(b) => b,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => { self.active = false; break; }
            };

            // Coalesce the whole burst that's already queued: net the volume deltas,
            // collapse repeated mute / name toggles, and refresh display only once.
            let mut net_volume: i32 = 0;
            let mut mute_toggles: u32 = 0;
            let mut name_toggles: u32 = 0;
            let mut launch_app = false;
            let mut generic_change = false;

            let mut absorb = |button: u8| match button {
                0 => generic_change = true,                          // pactl/poller change
                1 => launch_app = true,                              // launch volume app
                2 => mute_toggles += 1,                              // mute toggle
                3 => name_toggles += 1,                              // toggle device name
                4 => net_volume += self.config.audio_delta as i32,   // volume up
                5 => net_volume -= self.config.audio_delta as i32,   // volume down
                _ => {}
            };
            absorb(first);
            while let Ok(b) = event_rx.try_recv() {
                absorb(b);
            }

            // Apply the netted actions, each at most once.
            let mut action_affects_display = false;
            if mute_toggles % 2 == 1 {
                _ = self.toggle_mute().map_err(|e| eprintln!("Error toggling mute: {}", e));
                action_affects_display = true;
            }
            if net_volume != 0 {
                _ = self.adjust_volume(net_volume).map_err(|e| eprintln!("Error adjusting volume: {}", e));
                action_affects_display = true;
            }
            if name_toggles % 2 == 1 {
                self.show_device_name = !self.show_device_name;
                action_affects_display = true;
            }
            if launch_app {
                _ = Command::new(&self.config.volume_control_app).spawn().map_err(|e| eprintln!("Error spawning volume app: {}", e));
            }

            // Perform update if:
            // 1. It's the very first update.
            // 2. A pactl/poller change event occurred.
            // 3. A click action that affects the display was taken.
            if first_update || generic_change || action_affects_display {
                match fetch_sink_status() {
                    Ok(lines) => {
                        match get_output(get_default_sink_node_name(), lines, self.show_device_name, self.config.show_bt_battery) {
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
    }

    pub fn tx(&self) -> Sender<u8> {
        self.event_tx.clone()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.active = false;
        // Do not block on joining background threads during drop (tests would hang).
        // Remove stored handle so the JoinHandle is dropped (thread keeps running until it exits).
        let _ = self.pactl_event_thread_handle.lock().unwrap().take();
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
        let (status_line, volume) = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, true, false).unwrap();

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
        assert!(volume == 40);
    }

    #[test]
    fn no_device_name() {
        let response = include_str!("../tests/active.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let (status_line, volume) = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, false, false).unwrap();

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(!status_line.contains("Creative USB Headset"));
        assert!(volume == 40);
    }

    #[test]
    fn inactive_output() {
        let response = include_str!("../tests/inactive.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let (status_line, volume) = get_output(Some("alsa_output.usb-Creative_Technology_Creative_USB_Headset-00.11.analog-stereo".to_string()), lines, true, false).unwrap();

        assert!(status_line.contains(&String::from_iter([CHAR_AUDIO_MEDIUM])));
        assert!(status_line.contains("40%"));
        assert!(status_line.contains("Creative USB Headset"));
        assert_eq!(volume, 40);
    }
    #[test]
    fn empty_sink_list() {
        let lines: Vec<String> = Vec::new();
        let (status_line, volume) = get_output(None, lines, false, false).unwrap();
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

    #[test]
    fn mac_from_sink_name_parses() {
        assert_eq!(mac_from_sink_name("bluez_output.00_1A_7D_DA_71_13.a2dp-sink"), Some("00:1A:7D:DA:71:13".to_string()));
    }

    #[test]
    fn mac_from_sink_name_colon_separated() {
        assert_eq!(mac_from_sink_name("bluez_output.AA:BB:CC:DD:EE:FF.a2dp-sink"), Some("AA:BB:CC:DD:EE:FF".to_string()));
        // also accept sink names without a trailing profile component
        assert_eq!(mac_from_sink_name("bluez_output.AA:BB:CC:DD:EE:FF"), Some("AA:BB:CC:DD:EE:FF".to_string()));
    }

    #[test]
    fn bluetooth_sink_parsing() {
        let response = include_str!("../tests/bluetooth.txt");
        let lines: Vec<String> = response.lines().map(|l| l.to_owned()).collect();
        let (status_line, volume) = get_output(Some("bluez_output.00_1A_7D_DA_71_13.a2dp-sink".to_string()), lines, true, false).unwrap();

        assert!(status_line.contains("55%"));
        assert!(status_line.contains("Sony WH-1000XM4"));
        assert!(volume == 55);
    }

    #[test]
    fn render_with_mocked_bt_battery() {
        let s = Sink{
            volume_percent: 60,
            device_name: "ACME Headphones".to_string(),
            sink_name: "bluez_output.AA:BB:CC:DD:EE:FF".to_string(),
            mute: false,
            ..Default::default()
        };

        let (json, vol) = render_sink_output(&s, true, Some(30)).unwrap();
        assert!(json.contains("60%"));
        assert!(json.contains("🔋30%"));
        assert!(json.contains("ACME Headphones"));
        assert_eq!(vol, 60);
    }

    #[test]
    fn cached_bt_battery_honors_ttl() {
        let mac = "aa:bb:cc:dd:ee:ff";
        {
            let mut guard = BT_BATTERY_CACHE.lock().unwrap();
            guard.insert(mac.to_uppercase(), (Instant::now(), 77));
        }
        assert_eq!(cached_bt_battery(mac), Some(77));

        // expired entry should not be returned
        {
            let mut guard = BT_BATTERY_CACHE.lock().unwrap();
            guard.insert(mac.to_uppercase(), (Instant::now() - Duration::from_secs(BT_BATTERY_TTL_SECS + 1), 88));
        }
        assert_eq!(cached_bt_battery(mac), None);
    }

    #[test]
    fn subscribe_creates_poller_thread() {
        let cfg = Config { audio_delta: 5, volume_control_app: "pavucontrol".to_string(), show_device_name: false, show_bt_battery: false, print_header: false, use_wob: false };
        let mut c = Control::new(cfg);
        c.subscribe().unwrap();
        // pactl listener thread handle should be present
        assert!(c.pactl_event_thread_handle.lock().unwrap().is_some());
        // sending on tx should succeed (receiver present)
        assert!(c.tx().send(0).is_ok());
        // drop the receiver so background threads can exit, then drop control
        c.event_rx = None;
        drop(c);
    }

    #[test]
    fn mac_from_sink_name_lowercase() {
        assert_eq!(mac_from_sink_name("bluez_output.00_1a_7d_da_71_13.a2dp-sink"), Some("00:1A:7D:DA:71:13".to_string()));
    }

    #[test]
    fn mac_from_sink_name_non_underscored() {
        // unsupported format should return None
        assert_eq!(mac_from_sink_name("bluez_output.001A7DDA7113.a2dp-sink"), None);
    }

    #[test]
    fn bluez_device_paths_helper() {
        let mac = "aa:bb:cc:dd:ee:ff";
        let paths = bluez_device_paths_for_mac(mac, 4);
        assert_eq!(paths[0], "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(paths[1], "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(paths.len(), 4);
    }

    #[test]
    fn parse_bluetoothctl_info_output_from_fixture() {
        let s = include_str!("../tests/bluetoothctl_info.txt");
        assert_eq!(parse_bluetoothctl_info_output(s), Some(30));
    }

    #[test]
    fn parse_bluetoothctl_info_output_none() {
        let s = "Device AA:BB:CC:DD:EE:FF\n\tName: Some Device\n\tConnected: yes\n";
        assert_eq!(parse_bluetoothctl_info_output(s), None);
    }

    /// Live debugging test (ignored by default).
    ///
    /// Run locally with:
    ///   cargo test live_bt_battery_debug -- --ignored --nocapture
    ///
    /// This will print detected bluez sinks, the derived MAC and the result of the
    /// BlueZ Battery1 lookup so you can see where the lookup is failing on your machine.
    #[test]
    #[ignore]
    fn live_bt_battery_debug() {
        let lines = fetch_sink_status().unwrap();
        let mut found: Option<String> = None;
        for line in &lines {
            if line.trim_start().starts_with("Name: ") {
                let name = line.trim()[6..].trim().to_string();
                if name.starts_with("bluez_output.") {
                    found = Some(name);
                    break;
                }
            }
        }

        if found.is_none() {
            eprintln!("No bluez_output sink found in pactl output; skipping live test.");
            return;
        }

        let sink_name = found.unwrap();
        eprintln!("Found bluez sink name: {}", sink_name);

        let mac = mac_from_sink_name(&sink_name);
        eprintln!("mac_from_sink_name -> {:?}", mac);
        if mac.is_none() {
            eprintln!("mac_from_sink_name failed to parse {}; skipping live debug.", sink_name);
            return;
        }
        let mac = mac.unwrap();

        eprintln!("(live) get_bt_battery -> {:?}", get_bt_battery(&mac));

        let (status_line, volume) = get_output(get_default_sink_node_name(), fetch_sink_status().unwrap(), true, true).unwrap();
        eprintln!("get_output -> {}", status_line);
        eprintln!("volume -> {}", volume);
    }
}
