use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mcub::VERSION;
use mcub::core::config_manager::Config;
use mcub::core::device_identifier::{self, DeviceFormat, DeviceId, DeviceType};
use mcub::core::logger::Logger;
use mcub::core::signal_handler;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use udev::EventType;

const IDENTIFY_RETRIES: u32 = 3;
const MPD_CHECK_RETRIES: u32 = 30;
const MPD_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const DEVICE_SETTLE: Duration = Duration::from_millis(500);
const UDEV_COLLECT_WINDOW: Duration = Duration::from_millis(1000);
const BRIDGE_STOP_WAIT: Duration = Duration::from_millis(500);
const COOLDOWN_FAILURE_THRESHOLD: u32 = 3;
const COOLDOWN_INITIAL_SKIP: u32 = 3;
const COOLDOWN_MAX_SKIP: u32 = 30;

#[derive(Default, Clone)]
struct Cooldown {
    failures: u32,
    skip_cycles: u32,
    skip_remaining: u32,
}

struct Active {
    device: String,
    bridge_type: DeviceType,
    device_format: DeviceFormat,
    spectrum_bars: Option<u32>,
    #[allow(dead_code)]
    has_sysinfo: bool,
}

struct Watcher {
    config: Arc<Config>,
    logger: Arc<Logger>,
    project_root: PathBuf,
    device_check_interval: u32,
    baudrate: u32,
    bridge: Option<Child>,
    bridge_started_at: Option<Instant>,
    active: Option<Active>,
    cooldowns: HashMap<String, Cooldown>,
}

impl Watcher {
    fn new() -> Result<Self, String> {
        let project_root = resolve_project_root();
        let settings = project_root.join("shared/settings.json");
        if !settings.exists() {
            return Err(format!(
                "mcub-watcher-rust: settings.json not found at {}\n  set MCUB_BASE_DIR env var to project root",
                settings.display()
            ));
        }

        let config = Arc::new(
            Config::load(Some(&project_root)).map_err(|e| format!("config: {e}"))?
        );
        let logger = Arc::new(Logger::new(Some(&config), "WATCHER", Some(&project_root)));

        Ok(Self {
            device_check_interval: config.watcher.device_check_interval,
            baudrate: config.bridge.connection.baudrate,
            config,
            logger,
            project_root,
            bridge: None,
            bridge_started_at: None,
            active: None,
            cooldowns: HashMap::new(),
        })
    }

    fn cooldown_should_skip(&mut self, path: &str) -> bool {
        if let Some(c) = self.cooldowns.get_mut(path) {
            if c.skip_remaining > 0 {
                c.skip_remaining -= 1;
                return true;
            }
        }
        false
    }

    fn cooldown_record_failure(&mut self, path: &str) {
        let c = self.cooldowns.entry(path.to_string()).or_default();
        c.failures += 1;
        if c.failures >= COOLDOWN_FAILURE_THRESHOLD {
            c.skip_cycles = if c.skip_cycles < COOLDOWN_INITIAL_SKIP {
                COOLDOWN_INITIAL_SKIP
            } else {
                (c.skip_cycles * 2).min(COOLDOWN_MAX_SKIP)
            };
            c.skip_remaining = c.skip_cycles;
            self.logger.debug(&format!("{}: cooldown {} cycles", path, c.skip_cycles));
        }
    }

    fn cooldown_record_success(&mut self, path: &str) {
        if let Some(c) = self.cooldowns.get_mut(path) {
            *c = Cooldown::default();
        }
    }

    fn cooldown_remove(&mut self, path: &str) {
        self.cooldowns.remove(path);
    }

    fn identify(&self, device_path: &str) -> Option<DeviceId> {
        let logger = Arc::clone(&self.logger);
        let result = device_identifier::identify(
            device_path,
            self.baudrate,
            Duration::from_secs(2),
            |msg| logger.debug(msg),
        );
        match result {
            Ok(id) => {
                self.logger.debug(&format!("{}: {}", device_path, id.device_type.as_str()));
                Some(id)
            }
            Err(_) => {
                self.logger.debug(&format!("{}: no response", device_path));
                None
            }
        }
    }

    fn handle_new_device(&mut self, device_path: &str) -> bool {
        let path = device_path.to_string();
        self.logger.info(&format!("New device: {path}"));

        if self.active.is_some() || self.bridge.is_some() {
            self.logger.debug("Stopping bridge");
            self.cleanup_bridges();
            std::thread::sleep(Duration::from_millis(300));
        }

        let mut id: Option<DeviceId> = None;
        for attempt in 0..IDENTIFY_RETRIES {
            id = self.identify(&path);
            if id.is_some() { break; }
            self.logger.debug(&format!("Identify retry {}/{}", attempt + 1, IDENTIFY_RETRIES));
            std::thread::sleep(Duration::from_secs(1));
        }

        let Some(id) = id else {
            self.cooldown_record_failure(&path);
            return false;
        };
        self.cooldown_record_success(&path);

        let bars_info = id.spectrum_bars.map(|b| format!(" (bars={b})")).unwrap_or_default();
        let fmt_info = if id.format == DeviceFormat::Binary { " (format=binary)" } else { "" };
        self.logger.info(&format!("Identified: {}{}{}", id.device_type.as_str(), fmt_info, bars_info));

        if self.start_bridge(&id, &path) {
            self.active = Some(Active {
                device: path,
                bridge_type: id.device_type,
                device_format: id.format,
                spectrum_bars: id.spectrum_bars,
                has_sysinfo: id.has_sysinfo,
            });
            true
        } else {
            self.logger.error("Bridge start failed");
            false
        }
    }

    fn start_bridge(&mut self, id: &DeviceId, device_path: &str) -> bool {
        if id.device_type == DeviceType::Cava {
            cleanup_cava_process(&self.logger);
            std::thread::sleep(Duration::from_secs(1));
        }

        let Some(bridge_bin) = self.find_bridge_binary() else {
            self.logger.error("mcub-bridge-rust not found");
            return false;
        };

        let bridge_type = id.device_type.as_str();
        let mut cmd = Command::new(&bridge_bin);
        cmd.arg(bridge_type);
        cmd.env("DEVICE_PATH", device_path);
        cmd.env("MCUB_DEVICE_FORMAT", id.format.as_str());
        if let Some(bars) = id.spectrum_bars {
            cmd.env("MCUB_SPECTRUM_BARS", bars.to_string());
        }
        if id.has_sysinfo {
            cmd.env("MCUB_HAS_SYSINFO", "1");
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.logger.error(&format!("Fork failed: {e}"));
                return false;
            }
        };

        std::thread::sleep(Duration::from_secs(1));

        match child.try_wait() {
            Ok(None) => {
                let fmt_info = if id.format == DeviceFormat::Binary { " (binary)" } else { "" };
                self.logger.ok(&format!("Bridge: {}{}", bridge_type, fmt_info));
                self.bridge = Some(child);
                self.bridge_started_at = Some(Instant::now());
                true
            }
            _ => {
                self.logger.error(&format!("{} failed", bridge_type));
                false
            }
        }
    }

    fn find_bridge_binary(&self) -> Option<PathBuf> {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let p = dir.join("mcub-bridge-rust");
                if is_executable(&p) { return Some(p); }
            }
        }
        let candidates = [
            PathBuf::from("/usr/local/bin/mcub-bridge-rust"),
            PathBuf::from("./mcub-bridge-rust"),
            self.project_root.join("target/release/mcub-bridge-rust"),
            self.project_root.join("target/debug/mcub-bridge-rust"),
        ];
        candidates.into_iter().find(|p| is_executable(p))
    }

    fn check_active_bridge_health(&mut self) {
        if let Some(child) = self.bridge.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                self.logger.info("Bridge died, restart");
                self.bridge = None;
                if let Some(active) = self.active.as_ref() {
                    let dev = active.device.clone();
                    self.handle_new_device(&dev);
                }
                return;
            }
        }

        if let Some(active) = self.active.as_ref() {
            let in_grace = self.bridge_started_at
                .map(|t| t.elapsed() < Duration::from_secs(8))
                .unwrap_or(false);
            if active.bridge_type == DeviceType::Cava && !in_grace && !is_cava_running() {
                self.logger.info("CAVA died, restart");
                let dev = active.device.clone();
                let id = DeviceId {
                    device_type: DeviceType::Cava,
                    format: active.device_format,
                    spectrum_bars: active.spectrum_bars,
                    has_sysinfo: false,
                };
                self.cleanup_bridges();
                if self.start_bridge(&id, &dev) {
                    self.logger.ok("CAVA restarted");
                } else {
                    self.logger.error("CAVA restart failed");
                }
            }
        }
    }

    fn cleanup_bridges(&mut self) {
        let mut killed_any = false;

        if let Some(mut child) = self.bridge.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    let _ = child.kill();
                    for _ in 0..30 {
                        std::thread::sleep(Duration::from_millis(100));
                        if let Ok(Some(_)) = child.try_wait() { break; }
                    }
                    let _ = child.wait();
                    self.logger.info("Bridge stopped");
                    killed_any = true;
                }
            }
        }

        for ty in ["mpd", "cava", "hybrid", "sysinfo"] {
            let _ = Command::new("pkill")
                .args(["-f", &format!("mcub-bridge-rust {ty}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            self.logger.info(&format!("{ty} stopped"));
            killed_any = true;
        }

        if is_cava_running() {
            cleanup_cava_process(&self.logger);
        }

        if killed_any {
            std::thread::sleep(BRIDGE_STOP_WAIT);
        }
    }

    fn run(&mut self) {
        self.logger.info("Started");
        self.logger.info("Scanning");

        let devices = find_devices(&self.logger);
        for d in &devices {
            if self.handle_new_device(d) { break; }
        }

        let socket = build_udev_monitor();
        if socket.is_none() {
            self.logger.warning("No udev, polling mode");
            self.run_polling();
            return;
        }
        let socket = socket.unwrap();
        let mon_fd = socket.as_raw_fd();
        self.logger.info("Watching");

        let mut last_rescan: Option<Instant> = None;

        while !signal_handler::received() {
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(mon_fd) };
            let mut pfds = [PollFd::new(borrowed, PollFlags::POLLIN)];
            let n = poll(&mut pfds, PollTimeout::from(1000u16)).unwrap_or(0);

            if n <= 0 {
                if self.active.is_some() {
                    self.check_active_bridge_health();
                } else {
                    let now = Instant::now();
                    let due = last_rescan
                        .map(|t| now.duration_since(t).as_secs() >= self.device_check_interval as u64)
                        .unwrap_or(true);
                    if due {
                        last_rescan = Some(now);
                        let devs = find_devices(&self.logger);
                        for d in &devs {
                            if self.handle_new_device(d) { break; }
                        }
                    }
                }
                continue;
            }

            let mut pending: Vec<String> = Vec::new();
            let mut removed: Vec<String> = Vec::new();
            for event in socket.iter() {
                let devnode = event.devnode().and_then(|p| p.to_str()).map(String::from);
                let Some(devnode) = devnode else { continue; };
                if !devnode.contains("ttyACM") { continue; }
                match event.event_type() {
                    EventType::Add => {
                        self.logger.info(&format!("Connected: {devnode}"));
                        if !pending.contains(&devnode) { pending.push(devnode); }
                    }
                    EventType::Remove => removed.push(devnode),
                    _ => {}
                }
            }

            if !pending.is_empty() {
                std::thread::sleep(DEVICE_SETTLE);
                let deadline = Instant::now() + UDEV_COLLECT_WINDOW;
                while Instant::now() < deadline && pending.len() < 8 {
                    let mut pfds = [PollFd::new(borrowed, PollFlags::POLLIN)];
                    if poll(&mut pfds, PollTimeout::from(100u16)).unwrap_or(0) <= 0 { break; }
                    for event in socket.iter() {
                        if event.event_type() != EventType::Add { continue; }
                        let Some(dn) = event.devnode().and_then(|p| p.to_str()).map(String::from) else { continue; };
                        if !dn.contains("ttyACM") { continue; }
                        self.logger.info(&format!("Connected: {dn}"));
                        if !pending.contains(&dn) { pending.push(dn); }
                    }
                }
                pending.sort_by(|a, b| b.cmp(a));
                self.logger.debug(&format!("Processing: {} devices", pending.len()));
                for d in &pending {
                    self.cooldown_remove(d);
                    if self.handle_new_device(d) { break; }
                }
            }

            for dn in &removed {
                self.logger.info(&format!("Disconnected: {dn}"));
                self.cooldown_remove(dn);
                if self.active.as_ref().map(|a| a.device == *dn).unwrap_or(false) {
                    self.logger.info("Device removed, cleanup");
                    self.cleanup_bridges();
                    self.active = None;
                }
            }
        }

        self.cleanup_bridges();
    }

    fn run_polling(&mut self) {
        self.logger.info("Polling mode");
        while !signal_handler::received() {
            let devices = find_devices(&self.logger);

            if devices.is_empty() {
                if self.active.is_some() {
                    self.logger.info("No devices, cleanup");
                    self.cleanup_bridges();
                    self.active = None;
                }
                std::thread::sleep(Duration::from_secs(self.device_check_interval as u64));
                continue;
            }

            let mut found = false;
            for d in &devices {
                if self.active.as_ref().map(|a| a.device == *d).unwrap_or(false) {
                    self.check_active_bridge_health();
                    found = true;
                    break;
                }
                if self.cooldown_should_skip(d) { continue; }
                if let Some(_id) = self.identify(d) {
                    self.cooldown_record_success(d);
                    self.handle_new_device(d);
                    found = true;
                    break;
                }
                self.cooldown_record_failure(d);
            }

            if !found && self.active.is_some() {
                self.logger.info("No response, cleanup");
                self.cleanup_bridges();
                self.active = None;
            }

            std::thread::sleep(Duration::from_secs(self.device_check_interval as u64));
        }
        self.cleanup_bridges();
    }
}

fn resolve_project_root() -> PathBuf {
    if let Ok(env) = env::var("MCUB_BASE_DIR") {
        if !env.is_empty() { return PathBuf::from(env); }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent().and_then(|d| d.parent()) {
            return p.to_path_buf();
        }
    }
    PathBuf::from(".")
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn is_bridge_running(bridge_type: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", &format!("mcub-bridge-rust {bridge_type}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn is_cava_running() -> bool {
    Command::new("pgrep")
        .args(["-f", "/usr/bin/cava"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn cleanup_cava_process(logger: &Logger) {
    let _ = Command::new("pkill")
        .args(["-f", "/usr/bin/cava"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::thread::sleep(Duration::from_millis(200));
    let _ = Command::new("pkill")
        .args(["-9", "-f", "/usr/bin/cava"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = fs::remove_file("/tmp/cava_config");
    logger.info("CAVA cleanup done");
}

fn find_devices(logger: &Logger) -> Vec<String> {
    let mut devices: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();

    if let Ok(entries) = fs::read_dir("/dev/serial/by-id") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(resolved) = fs::canonicalize(&path) {
                let p = resolved.to_string_lossy().to_lowercase();
                if p.contains("acm") || p.contains("ttyacm") {
                    let s = resolved.to_string_lossy().to_string();
                    if seen.insert(s.clone()) { devices.push(s); }
                }
            }
        }
    }

    if devices.is_empty() {
        if let Ok(paths) = glob::glob("/dev/ttyACM*") {
            for path in paths.flatten() {
                let s = path.to_string_lossy().to_string();
                if seen.insert(s.clone()) { devices.push(s); }
            }
        }
    }

    devices.retain(|d| {
        nix::sys::stat::stat(d.as_str())
            .is_ok()
            && fs::metadata(d).map(|m| !m.permissions().readonly()).unwrap_or(true)
    });

    devices.sort_by(|a, b| b.cmp(a));
    logger.debug(&format!("Valid: {} devices", devices.len()));
    devices
}

fn build_udev_monitor() -> Option<udev::MonitorSocket> {
    udev::MonitorBuilder::new()
        .ok()?
        .match_subsystem("tty")
        .ok()?
        .listen()
        .ok()
}

fn wait_for_mpd(logger: &Logger) -> bool {
    logger.info("MPD waiting");
    for i in 0..MPD_CHECK_RETRIES {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 6600);
        if let Ok(stream) = TcpStream::connect_timeout(&addr.into(), Duration::from_secs(1)) {
            drop(stream);
            logger.ok("MPD ready");
            return true;
        }
        if i < MPD_CHECK_RETRIES - 1 {
            std::thread::sleep(MPD_CHECK_INTERVAL);
        }
    }
    logger.error("MPD timeout");
    false
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mut no_wait_mpd = false;
    let mut status_mode = false;
    let mut cleanup_mode = false;

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--status" => status_mode = true,
            "--cleanup" => cleanup_mode = true,
            "--no-wait-mpd" => no_wait_mpd = true,
            "--version" => {
                let _ = writeln!(std::io::stdout(), "Watcher {VERSION}");
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("Usage: {} [--status] [--cleanup] [--no-wait-mpd] [--version]", args[0]);
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }

    let mut watcher = match Watcher::new() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    signal_handler::setup(|| {});

    if status_mode {
        let devices = find_devices(&watcher.logger);
        watcher.logger.info(&format!("MCUB WATCHER v{VERSION}"));
        let active_dev = watcher.active.as_ref().map(|a| a.device.as_str()).unwrap_or("None");
        let active_br = watcher.active.as_ref().map(|a| a.bridge_type.as_str()).unwrap_or("None");
        watcher.logger.info(&format!("Active device: {active_dev}"));
        watcher.logger.info(&format!("Active bridge: {active_br}"));
        watcher.logger.info(&format!("Connected devices: {}", devices.len()));
        watcher.logger.info(&format!("MPD bridge running: {}", is_bridge_running("mpd") as u8));
        watcher.logger.info(&format!("CAVA bridge running: {}", is_bridge_running("cava") as u8));
        return ExitCode::SUCCESS;
    }

    if cleanup_mode {
        watcher.cleanup_bridges();
        watcher.logger.info("Cleanup done");
        return ExitCode::SUCCESS;
    }

    if !no_wait_mpd {
        if !wait_for_mpd(&watcher.logger) {
            watcher.logger.error("MPD failed");
            return ExitCode::from(1);
        }
    } else {
        watcher.logger.info("MPD wait disabled");
    }

    let _ = Arc::clone(&watcher.config);
    watcher.run();
    ExitCode::SUCCESS
}
