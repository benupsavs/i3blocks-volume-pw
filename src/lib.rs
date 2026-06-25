mod protocol;
use protocol::*;

use std::{error::Error, io::{self, Write}, process::{Command, Stdio, ChildStdin}, sync::{Arc, Mutex}, thread, rc::Rc, cell::{Cell, RefCell}, os::unix::io::RawFd};

use lazy_static::lazy_static;
use zbus::blocking::{Connection, Proxy};

use envconfig::Envconfig;
use regex::Regex;
use std::time::{Duration, Instant};
use std::collections::HashMap;

use libpulse_binding as pulse;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::context::subscribe::InterestMaskSet;
use pulse::context::introspect::SinkInfo;
use pulse::mainloop::standard::{Mainloop, IterateResult};
use pulse::mainloop::api::Mainloop as _; // trait providing new_io_event()
use pulse::mainloop::events::io::FlagSet as IoFlagSet;
use pulse::callbacks::ListResult;
use pulse::volume::{ChannelVolumes, Volume};
use pulse::def::SinkState;

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
    use std::io::Read;
    use std::sync::mpsc;
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

/// Build a [`Sink`] from a native PulseAudio/PipeWire `SinkInfo`.
fn sink_from_info(info: &SinkInfo) -> Sink {
    let mut sink = Sink::default();
    sink.sink_name = info.name.as_ref().map(|c| c.to_string()).unwrap_or_default();
    sink.got_sink_name = !sink.sink_name.is_empty();
    sink.mute = info.mute;
    sink.got_mute = true;
    sink.active = info.state == SinkState::Running;
    // Volume as a percentage of the normal (100%) reference level.
    sink.volume_percent =
        (info.volume.avg().0 as f64 / Volume::NORMAL.0 as f64 * 100.0).round() as u16;
    sink.got_volume = true;
    // Prefer node.nick, then device.alias for the display name (matches prior behaviour).
    if let Some(v) = info.proplist.get_str("node.nick").filter(|s| !s.is_empty()) {
        sink.device_name = v;
        sink.got_device_name = true;
    } else if let Some(v) = info.proplist.get_str("device.alias").filter(|s| !s.is_empty()) {
        sink.device_name = v;
        sink.got_device_name = true;
    }
    sink
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
    let chosen = choose_sink(&sinks, default_sink_node.as_deref());
    let s = match chosen {
        Some(s) => s,
        None => return Ok((String::new(), 0)), // No suitable sink
    };

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

/// Pick the sink to display: prefer a RUNNING sink, then the reported default sink,
/// then the first available sink. Returns `None` only when the list is empty.
fn choose_sink<'a>(sinks: &'a [Sink], default_sink_node: Option<&str>) -> Option<&'a Sink> {
    sinks.iter().find(|s| s.active)
        .or_else(|| default_sink_node.and_then(|d| sinks.iter().find(|s| s.sink_name == d)))
        .or_else(|| sinks.first())
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

/// Mutable display/runtime state shared between the PulseAudio callbacks and the
/// event loop. Everything lives on the single event-loop thread, so a plain
/// `Rc<RefCell<..>>` is sufficient (no locking).
struct State {
    show_device_name: bool,
    show_bt_battery: bool,
    previous_line: String,
    last_volume: u16,
    first_update: bool,
    /// Default sink name as last reported by the server (for selection fallback).
    default_sink: Option<String>,
    /// Currently displayed sink + its raw volume, used to apply click actions.
    cur_sink_name: Option<String>,
    cur_volume: ChannelVolumes,
    cur_mute: bool,
    wob_stdin: Option<ChildStdin>,
    /// MAC of the current Bluetooth sink (if any), read by the bt-poller thread.
    current_bluez_mac: Arc<Mutex<Option<String>>>,
}

/// PipeWire/PulseAudio volume control for an i3blocks blocklet.
///
/// Holds a single persistent client connection to the PulseAudio-compatible
/// server (pipewire-pulse) and reacts to native subscribe events. Because there
/// is exactly one long-lived client, it never generates the client-churn events
/// that a fork-per-update (`pactl`) design feeds back into itself.
pub struct Control {
    config: Config,
}

impl Control {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Run the blocklet event loop. Fully event-driven: the process blocks in
    /// `poll()` (zero CPU) until the PulseAudio socket, stdin (clicks), or the
    /// battery-poller wakeup pipe becomes readable. Returns when stdin closes or
    /// the connection dies.
    pub fn run(self) -> Result<(), Box<dyn Error>> {
        // Optional i3bar protocol header.
        if self.config.print_header {
            let header = Header { version: 1, click_events: Some(true), ..Default::default() };
            println!("{}", serde_json::to_string(&header)?);
        }

        // Optional `wob` overlay process.
        let wob_stdin = if self.config.use_wob {
            match Command::new("wob").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                Ok(mut child) => child.stdin.take(),
                Err(e) => { eprintln!("Failed to spawn wob: {}", e); None }
            }
        } else {
            None
        };

        let current_bluez_mac: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Self-pipe so the (blocking) Bluetooth battery thread can wake the event
        // loop: it writes a byte after warming the cache, the read end is polled as
        // an IO event source, and its callback triggers a redraw.
        let (bt_pipe_rd, bt_pipe_wr) = make_pipe()?;
        set_nonblocking(bt_pipe_rd);

        // Background Bluetooth battery poller: the D-Bus / bluetoothctl lookup may
        // block, so it runs off the event loop and signals via the pipe.
        if self.config.show_bt_battery {
            let mac_slot = current_bluez_mac.clone();
            thread::Builder::new().name("bt-poller".to_string()).spawn(move || {
                loop {
                    let mac = mac_slot.lock().unwrap().clone();
                    if let Some(mac) = mac {
                        if get_bt_battery(&mac).is_some() {
                            let _ = unsafe { libc::write(bt_pipe_wr, [1u8].as_ptr() as *const libc::c_void, 1) };
                        }
                    }
                    thread::sleep(Duration::from_secs(BT_POLL_INTERVAL_SECS));
                }
            })?;
        }

        let state = Rc::new(RefCell::new(State {
            show_device_name: self.config.show_device_name,
            show_bt_battery: self.config.show_bt_battery,
            previous_line: String::new(),
            last_volume: 0,
            first_update: true,
            default_sink: None,
            cur_sink_name: None,
            cur_volume: ChannelVolumes::default(),
            cur_mute: false,
            wob_stdin,
            current_bluez_mac,
        }));

        // --- Connect to the server ---
        let mut mainloop = Mainloop::new().ok_or("failed to create PulseAudio mainloop")?;
        let ctx = Rc::new(RefCell::new(
            Context::new(&mainloop, "i3blocks-volume-pw").ok_or("failed to create PulseAudio context")?
        ));
        ctx.borrow_mut().connect(None, ContextFlagSet::NOFLAGS, None)?;

        // Block until the context is ready (or fails).
        loop {
            match mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) =>
                    return Err("PulseAudio mainloop quit during connect".into()),
            }
            match ctx.borrow().get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated =>
                    return Err("PulseAudio connection failed".into()),
                _ => {}
            }
        }

        // --- Subscribe to sink and server changes ---
        // Only SINK | SERVER: nothing here reacts to client events, so our own
        // introspection queries can never re-trigger a refresh.
        {
            let ctx_sub = ctx.clone();
            let state_sub = state.clone();
            ctx.borrow_mut().set_subscribe_callback(Some(Box::new(move |_facility, _op, _idx| {
                request_redraw(&ctx_sub, &state_sub);
            })));
            ctx.borrow_mut().subscribe(InterestMaskSet::SINK | InterestMaskSet::SERVER, |_| {});
        }

        // Set to true by the stdin callback on EOF (parent closed); checked after
        // each blocking iteration to exit the loop.
        let quit = Rc::new(Cell::new(false));

        // --- stdin (clicks) as an IO event source ---
        set_nonblocking(0);
        let stdin_ev = {
            let ctx_c = ctx.clone();
            let state_c = state.clone();
            let quit_c = quit.clone();
            let app = self.config.volume_control_app.clone();
            let delta = self.config.audio_delta as i32;
            let mut acc: Vec<u8> = Vec::new();
            let mut buf = [0u8; 1024];
            mainloop.new_io_event(0, IoFlagSet::INPUT, Box::new(move |mut ev, _fd, _flags| {
                loop {
                    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                    if n > 0 {
                        acc.extend_from_slice(&buf[..n as usize]);
                    } else if n == 0 {
                        // EOF: parent gone. Stop listening and ask the loop to quit.
                        quit_c.set(true);
                        ev.enable(IoFlagSet::NULL);
                        return;
                    } else {
                        break; // EAGAIN
                    }
                }
                while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = acc.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&line);
                    handle_click(text.trim(), &ctx_c, &state_c, &app, delta);
                }
            }))
        };

        // --- battery-poller wakeup pipe as an IO event source ---
        let bt_ev = {
            let ctx_p = ctx.clone();
            let state_p = state.clone();
            let mut drain = [0u8; 64];
            mainloop.new_io_event(bt_pipe_rd, IoFlagSet::INPUT, Box::new(move |_ev, _fd, _flags| {
                while unsafe { libc::read(bt_pipe_rd, drain.as_mut_ptr() as *mut libc::c_void, drain.len()) } > 0 {}
                request_redraw(&ctx_p, &state_p);
            }))
        };

        // Render once at startup, then sleep until something actually happens.
        request_redraw(&ctx, &state);
        let result = loop {
            match mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) => break Ok(()),
            }
            if quit.get() {
                break Ok(());
            }
        };

        // Keep the event sources alive for the whole loop.
        drop(stdin_ev);
        drop(bt_ev);
        result
    }
}

/// Handle one line of i3bar click JSON. Lines that don't parse as a click are
/// treated as a generic refresh request (matches the prior behaviour). Volume and
/// mute actions are fire-and-forget; the resulting sink-change event drives the
/// redraw, while local-only changes (device-name toggle) redraw directly.
fn handle_click(text: &str, ctx: &Rc<RefCell<Context>>, state: &Rc<RefCell<State>>, volume_app: &str, delta: i32) {
    if text.is_empty() {
        return;
    }
    let button = match parse_click(text) {
        Ok(click) => click.button,
        Err(_) => { request_redraw(ctx, state); return; }
    };
    match button {
        1 => {
            if let Err(e) = Command::new(volume_app).spawn() {
                eprintln!("Error spawning volume app: {}", e);
            }
        }
        2 => set_mute_toggle(ctx, state),
        3 => {
            {
                let mut s = state.borrow_mut();
                s.show_device_name = !s.show_device_name;
            }
            request_redraw(ctx, state);
        }
        4 => adjust_volume(ctx, state, delta),
        5 => adjust_volume(ctx, state, -delta),
        _ => request_redraw(ctx, state),
    }
}

/// Create a pipe, returning (read_fd, write_fd).
fn make_pipe() -> Result<(RawFd, RawFd), Box<dyn Error>> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err("failed to create wakeup pipe".into());
    }
    Ok((fds[0], fds[1]))
}

/// Apply a relative volume change (in percent) to the current sink.
fn adjust_volume(ctx: &Rc<RefCell<Context>>, state: &Rc<RefCell<State>>, delta_pct: i32) {
    if delta_pct == 0 {
        return;
    }
    let (name, mut cv) = {
        let s = state.borrow();
        match &s.cur_sink_name {
            Some(n) => (n.clone(), s.cur_volume),
            None => return,
        }
    };
    let step = Volume((Volume::NORMAL.0 as f64 * (delta_pct.unsigned_abs() as f64 / 100.0)) as u32);
    if delta_pct > 0 {
        cv.increase(step);
    } else {
        cv.decrease(step);
    }
    // Fire-and-forget: the resulting sink change event triggers a redraw.
    ctx.borrow().introspect().set_sink_volume_by_name(&name, &cv, None);
}

/// Toggle mute on the current sink.
fn set_mute_toggle(ctx: &Rc<RefCell<Context>>, state: &Rc<RefCell<State>>) {
    let (name, mute) = {
        let s = state.borrow();
        match &s.cur_sink_name {
            Some(n) => (n.clone(), !s.cur_mute),
            None => return,
        }
    };
    ctx.borrow().introspect().set_sink_mute_by_name(&name, mute, None);
}

/// Query the server for the default sink, then the full sink list, and render the
/// chosen sink. All callbacks run on the event-loop thread.
fn request_redraw(ctx: &Rc<RefCell<Context>>, state: &Rc<RefCell<State>>) {
    let ctx_for_list = ctx.clone();
    let state_for_srv = state.clone();
    // First learn the default sink name, then list sinks (so selection is correct).
    ctx.borrow().introspect().get_server_info(move |info| {
        state_for_srv.borrow_mut().default_sink =
            info.default_sink_name.as_ref().map(|c| c.to_string());

        let scratch: Rc<RefCell<Vec<(Sink, ChannelVolumes)>>> = Rc::new(RefCell::new(Vec::new()));
        let state_for_end = state_for_srv.clone();
        let scratch_cb = scratch.clone();
        ctx_for_list.borrow().introspect().get_sink_info_list(move |res| match res {
            ListResult::Item(info) => {
                scratch_cb.borrow_mut().push((sink_from_info(info), info.volume));
            }
            ListResult::End => finalize_render(&state_for_end, &scratch_cb.borrow()),
            ListResult::Error => {}
        });
    });
}

/// Select the sink to show, update shared state, and print the i3bar line if it changed.
fn finalize_render(state: &Rc<RefCell<State>>, sinks: &[(Sink, ChannelVolumes)]) {
    let mut s = state.borrow_mut();

    let chosen = {
        let sink_views: Vec<&Sink> = sinks.iter().map(|(k, _)| k).collect();
        match choose_sink_idx(&sink_views, s.default_sink.as_deref()) {
            Some(i) => i,
            None => return,
        }
    };
    let (sink, volume) = &sinks[chosen];

    // Bluetooth battery (cached only; warmed off-loop by the poller thread).
    let mac = if sink.sink_name.starts_with("bluez_output.") {
        mac_from_sink_name(&sink.sink_name)
    } else {
        None
    };
    let bt_battery = if s.show_bt_battery {
        mac.as_deref().and_then(cached_bt_battery)
    } else {
        None
    };
    *s.current_bluez_mac.lock().unwrap() = mac;

    // Remember the current sink so click actions can act on it.
    s.cur_sink_name = Some(sink.sink_name.clone());
    s.cur_volume = *volume;
    s.cur_mute = sink.mute;

    let include_name = s.show_device_name;
    match render_sink_output(sink, include_name, bt_battery) {
        Ok((line, vol_pct)) => {
            if line != s.previous_line || s.first_update {
                let mut out = io::stdout().lock();
                if writeln!(out, "{}", line).is_err() || out.flush().is_err() {
                    return;
                }
                s.previous_line = line;
            }
            if s.last_volume != vol_pct && !s.first_update {
                if let Some(w) = s.wob_stdin.as_mut() {
                    if write!(w, "{}\n", vol_pct).is_err() || w.flush().is_err() {
                        eprintln!("Error writing to wob, disabling wob output.");
                        s.wob_stdin = None;
                    }
                }
            }
            s.last_volume = vol_pct;
            s.first_update = false;
        }
        Err(e) => eprintln!("Error rendering output: {}", e),
    }
}

/// Index-returning variant of [`choose_sink`] used by the native render path.
fn choose_sink_idx(sinks: &[&Sink], default_sink_node: Option<&str>) -> Option<usize> {
    sinks.iter().position(|s| s.active)
        .or_else(|| default_sink_node.and_then(|d| sinks.iter().position(|s| s.sink_name == d)))
        .or_else(|| if sinks.is_empty() { None } else { Some(0) })
}

/// Put a file descriptor into non-blocking mode (best-effort).
fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
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

    #[test]
    fn choose_sink_prefers_running() {
        let mut a = Sink::default(); a.sink_name = "a".into();
        let mut b = Sink::default(); b.sink_name = "b".into(); b.active = true;
        let sinks = vec![a, b];
        assert_eq!(choose_sink(&sinks, Some("a")).unwrap().sink_name, "b");
    }

    #[test]
    fn choose_sink_falls_back_to_default_then_first() {
        let mut a = Sink::default(); a.sink_name = "a".into();
        let mut b = Sink::default(); b.sink_name = "b".into();
        let sinks = vec![a, b];
        // default match wins over first
        assert_eq!(choose_sink(&sinks, Some("b")).unwrap().sink_name, "b");
        // no default -> first
        assert_eq!(choose_sink(&sinks, None).unwrap().sink_name, "a");
        // empty -> none
        assert!(choose_sink(&[], Some("x")).is_none());
    }
}
