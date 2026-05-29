use std::env;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::core::action_runner;
use crate::core::cava_manager::{CavaManager, ReadResult, check_loopback};
use crate::core::config_manager::{Config, get_cava_env_config};
use crate::core::logger::Logger;
use crate::core::serial_comm::SerialComm;
use crate::core::signal_handler;
use crate::modules::mpd_bridge::MpdBridge;
use crate::modules::sysinfo_bridge::SysinfoCollector;
use crate::{log_debug, log_error, log_info, log_ok, log_warning};

const STATS_INTERVAL: Duration = Duration::from_secs(60);
const HEALTH_CHECK: Duration = Duration::from_millis(500);
const MPD_RECONNECT: Duration = Duration::from_secs(5);
const IDLE_SLEEP: Duration = Duration::from_micros(5000);

pub struct HybridBridge {
    inner: Arc<Inner>,
}

struct Inner {
    serial: Arc<SerialComm>,
    mpd: Option<Arc<MpdBridge>>,
    cava: Mutex<Option<CavaManager>>,
    sysinfo: Mutex<Option<SysinfoCollector>>,
    stats: Mutex<Stats>,
    cava_skip: Mutex<CavaSkip>,
    cava_deferred: Mutex<bool>,
    supports: Mutex<Supports>,
    config: Arc<Config>,
    logger: Arc<Logger>,
    device_format: String,
    cava_format: String,
    spectrum_bars: Option<u32>,
    mpd_update_interval: f64,
    sysinfo_interval: f64,
    last_mpd_update: Mutex<Option<Instant>>,
}

#[derive(Default)]
struct Stats {
    cava_frames: u32,
    mpd_updates: u32,
    sysinfo_updates: u32,
    bytes_sent: usize,
    commands: u32,
    mpd_latency_total: f64,
    mpd_latency_max: f64,
    last_report: Option<Instant>,
    start: Option<Instant>,
}

#[derive(Default)]
struct CavaSkip {
    count: u32,
    remaining: u32,
}

#[derive(Default)]
struct Supports {
    mpd: bool,
    cava: bool,
    sysinfo: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IdEnvelope {
    d: IdData,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IdData {
    modes: Vec<String>,
    name: Option<String>,
}

#[derive(Serialize)]
struct SpectrumEnvelope<'a> {
    t: &'a str,
    d: &'a [i32],
}

#[derive(Deserialize)]
struct CmdEnvelope {
    t: String,
    c: Option<CmdContent>,
}

#[derive(Deserialize)]
struct CmdContent {
    action: String,
    #[serde(default)]
    parameters: Option<serde_json::Value>,
}

const SUPPORTED_COMMANDS: &[&str] = &[
    "play_pause", "next", "previous", "stop", "set_volume",
    "volume_up", "volume_down", "repeat", "single", "consume", "random", "exec",
];

impl HybridBridge {
    pub fn new(config: Arc<Config>, logger: Arc<Logger>) -> Self {
        let serial = Arc::new(SerialComm::new(Arc::clone(&config), Arc::clone(&logger), "Hybrid"));
        let env = get_cava_env_config();
        let spectrum_bars = env.spectrum_bars.or(Some(config.cava.defaults.spectrum_bars));
        let sysinfo_interval = if config.sysinfo.update_interval < 0.5 {
            2.0
        } else {
            config.sysinfo.update_interval
        };
        let mpd_update_interval = config.mpd.update_interval;

        action_runner::init(config.actions.clone(), Arc::clone(&logger));
        let now = Instant::now();

        Self {
            inner: Arc::new(Inner {
                serial,
                mpd: None,
                cava: Mutex::new(None),
                sysinfo: Mutex::new(None),
                stats: Mutex::new(Stats {
                    last_report: Some(now),
                    start: Some(now),
                    ..Stats::default()
                }),
                cava_skip: Mutex::new(CavaSkip::default()),
                cava_deferred: Mutex::new(false),
                supports: Mutex::new(Supports::default()),
                config: Arc::clone(&config),
                logger: Arc::clone(&logger),
                device_format: env.device_format,
                cava_format: env.cava_format,
                spectrum_bars,
                mpd_update_interval,
                sysinfo_interval,
                last_mpd_update: Mutex::new(None),
            }),
        }
    }

    pub fn run(mut self) -> i32 {
        if self.inner.serial.connect().is_err() {
            log_error!(self.inner.logger, "Device failed");
            return 1;
        }

        let id_response = match self.inner.serial.identify_device() {
            Some(s) => s,
            None => {
                log_error!(self.inner.logger, "Device identify failed");
                return 1;
            }
        };

        let detected = self.extract_supported_modes(&id_response);
        if !detected {
            log_warning!(self.inner.logger, "No modes detected");
        }

        {
            let s = self.inner.supports.lock().unwrap();
            let modes = [
                s.mpd.then_some("mpd"),
                s.cava.then_some("cava"),
                s.sysinfo.then_some("sysinfo"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            log_info!(self.inner.logger, "Modes: [{}]", modes);
        }

        // Init optional sub-components
        let supports_mpd = self.inner.supports.lock().unwrap().mpd;
        let supports_cava = self.inner.supports.lock().unwrap().cava;
        let supports_sysinfo = self.inner.supports.lock().unwrap().sysinfo;

        let mut handles: Vec<(&'static str, JoinHandle<()>)> = Vec::new();

        if supports_mpd {
            log_info!(self.inner.logger, "MPD starting");
            let mpd = Arc::new(MpdBridge::new(
                Arc::clone(&self.inner.config),
                Arc::clone(&self.inner.logger),
            ));
            if !mpd.connect() {
                log_warning!(self.inner.logger, "MPD not ready");
            }
            Arc::get_mut(&mut self.inner).expect("unique").mpd = Some(mpd);

            let inner = Arc::clone(&self.inner);
            let h = thread::Builder::new()
                .name("MPDCheckerThread".into())
                .spawn(move || mpd_checker_loop(inner))
                .unwrap();
            log_debug!(self.inner.logger, "Thread: MPDCheckerThread");
            handles.push(("MPDCheckerThread", h));
        }

        if supports_cava {
            let mut cava = CavaManager::new(
                Arc::clone(&self.inner.logger),
                Arc::clone(&self.inner.config),
                Some(&self.inner.cava_format),
                self.inner.spectrum_bars,
            );
            if self.inner.device_format == "binary" {
                log_info!(self.inner.logger, "Binary mode: 8bit, sync=0xCA");
            }
            if !cava.start(0) {
                log_info!(self.inner.logger, "CAVA deferred");
                *self.inner.cava_deferred.lock().unwrap() = true;
            }
            *self.inner.cava.lock().unwrap() = Some(cava);

            if !*self.inner.cava_deferred.lock().unwrap() {
                let inner = Arc::clone(&self.inner);
                let h = thread::Builder::new()
                    .name("CAVAReaderThread".into())
                    .spawn(move || cava_reader_loop(inner))
                    .unwrap();
                log_debug!(self.inner.logger, "Thread: CAVAReaderThread");
                handles.push(("CAVAReaderThread", h));
            }
        }

        if supports_sysinfo {
            let mut collector = SysinfoCollector::new();
            collector.warmup();
            *self.inner.sysinfo.lock().unwrap() = Some(collector);
        }

        {
            let inner = Arc::clone(&self.inner);
            let h = thread::Builder::new()
                .name("CommandProcessorThread".into())
                .spawn(move || command_processor_loop(inner))
                .unwrap();
            log_debug!(self.inner.logger, "Thread: CommandProcessorThread");
            handles.push(("CommandProcessorThread", h));
        }

        if supports_sysinfo {
            let inner = Arc::clone(&self.inner);
            let h = thread::Builder::new()
                .name("SysInfoThread".into())
                .spawn(move || sysinfo_checker_loop(inner))
                .unwrap();
            log_debug!(self.inner.logger, "Thread: SysInfoThread");
            handles.push(("SysInfoThread", h));
        }

        // Main loop: monitor threads + deferred CAVA + stats
        while !signal_handler::received() {
            // Detect dead threads
            let mut died = None;
            handles.retain(|(name, h)| {
                if h.is_finished() {
                    died = Some(*name);
                    false
                } else {
                    true
                }
            });
            if let Some(name) = died {
                log_error!(self.inner.logger, "Thread died: {}", name);
                break;
            }

            // Try deferred CAVA start
            let deferred = *self.inner.cava_deferred.lock().unwrap();
            if deferred {
                let started = {
                    let mut cava_guard = self.inner.cava.lock().unwrap();
                    if let Some(cava) = cava_guard.as_mut() {
                        check_loopback() && cava.start(0)
                    } else {
                        false
                    }
                };
                if started {
                    *self.inner.cava_deferred.lock().unwrap() = false;
                    let inner = Arc::clone(&self.inner);
                    let h = thread::Builder::new()
                        .name("CAVAReaderThread".into())
                        .spawn(move || cava_reader_loop(inner))
                        .unwrap();
                    log_ok!(self.inner.logger, "CAVA started (deferred)");
                    handles.push(("CAVAReaderThread", h));
                }
            }

            self.report_stats();
            std::thread::sleep(HEALTH_CHECK);
        }

        self.cleanup(handles);
        0
    }

    fn extract_supported_modes(&self, id_json: &str) -> bool {
        let Ok(env) = serde_json::from_str::<IdEnvelope>(id_json) else { return false; };
        let mut s = self.inner.supports.lock().unwrap();

        if !env.d.modes.is_empty() {
            for m in &env.d.modes {
                match m.as_str() {
                    "mpd" => s.mpd = true,
                    "cava" => s.cava = true,
                    "sysinfo" => s.sysinfo = true,
                    _ => {}
                }
            }
            if env.d.modes.iter().any(|_| true) {
                drop(s);
                self.apply_env_overrides();
                return true;
            }
        }
        if let Some(name) = &env.d.name {
            let lower = name.to_ascii_lowercase();
            if lower.contains("cava") { s.cava = true; }
            if lower.contains("mpd") { s.mpd = true; }
            if lower.contains("sysinfo") { s.sysinfo = true; }
        }
        drop(s);
        self.apply_env_overrides();

        let s = self.inner.supports.lock().unwrap();
        s.mpd || s.cava || s.sysinfo
    }

    fn apply_env_overrides(&self) {
        if env::var("MCUB_HAS_SYSINFO").as_deref() == Ok("1") {
            self.inner.supports.lock().unwrap().sysinfo = true;
        }
    }

    fn report_stats(&self) {
        let now = Instant::now();
        let mut stats = self.inner.stats.lock().unwrap();
        let last = stats.last_report.unwrap_or(now);
        let elapsed = now.duration_since(last);
        if elapsed < STATS_INTERVAL { return; }

        let elapsed_s = elapsed.as_secs_f64();
        let cava_fps = stats.cava_frames as f64 / elapsed_s;
        let drops = if stats.cava_frames > 0 {
            let expected = (elapsed_s * self.inner.config.cava.framerate as f64 * 0.95) as i32;
            (expected - stats.cava_frames as i32).max(0)
        } else {
            0
        };
        let mpd_avg = if stats.mpd_updates > 0 {
            (stats.mpd_latency_total / stats.mpd_updates as f64) * 1000.0
        } else {
            0.0
        };
        let mpd_peak = stats.mpd_latency_max * 1000.0;
        let uptime = stats.start.map(|s| now.duration_since(s).as_secs()).unwrap_or(0);
        let qs = self.inner.serial.queue_stats().unwrap_or_default();

        log_info!(
            self.inner.logger,
            "Stats: CAVA={} ({:.1}/s, drops={}), MPD={} (avg={:.1}ms, peak={:.1}ms), sys={}, cmds={}, sent={:.1}KB, queue(peak={}, wr={:.1}/{:.1}ms), up={}s",
            stats.cava_frames, cava_fps, drops,
            stats.mpd_updates, mpd_avg, mpd_peak,
            stats.sysinfo_updates,
            stats.commands,
            stats.bytes_sent as f64 / 1024.0,
            qs.peak_depth, qs.write_avg_ms, qs.write_max_ms,
            uptime
        );

        stats.cava_frames = 0;
        stats.mpd_updates = 0;
        stats.sysinfo_updates = 0;
        stats.bytes_sent = 0;
        stats.commands = 0;
        stats.mpd_latency_total = 0.0;
        stats.mpd_latency_max = 0.0;
        stats.last_report = Some(now);
    }

    fn cleanup(&self, handles: Vec<(&'static str, JoinHandle<()>)>) {
        log_info!(self.inner.logger, "Cleanup");
        if let Some(mpd) = self.inner.mpd.as_ref() {
            mpd.disconnect();
        }
        for (name, h) in handles {
            // Best-effort wait; threads should observe signal_handler::received() shortly
            log_debug!(self.inner.logger, "Joining {}", name);
            let _ = h.join();
        }
        if let Some(mpd) = self.inner.mpd.as_ref() {
            mpd.cleanup();
        }
        if let Some(mut cava) = self.inner.cava.lock().unwrap().take() {
            cava.cleanup();
        }
        self.inner.serial.close();
    }
}

fn mpd_checker_loop(inner: Arc<Inner>) {
    signal_handler::block_in_thread();
    let Some(mpd) = inner.mpd.as_ref() else { return; };

    let mut send_initial = true;
    let mut last_reconnect: Option<Instant> = None;
    let mut last_state: Option<String> = None;
    let mut last_title: Option<String> = None;

    while !signal_handler::received() {
        let now = Instant::now();
        let connected = mpd.is_connected();

        if !connected {
            let allow = last_reconnect
                .map(|t| now.duration_since(t) >= MPD_RECONNECT)
                .unwrap_or(true);
            if allow {
                last_reconnect = Some(now);
                if mpd.connect() {
                    log_ok!(inner.logger, "MPD reconnected");
                    send_initial = true;
                } else {
                    sleep_check(Duration::from_millis(1000));
                    continue;
                }
            } else {
                sleep_check(Duration::from_millis(500));
                continue;
            }
        }

        let due = {
            let last = inner.last_mpd_update.lock().unwrap();
            match *last {
                Some(t) => now.duration_since(t).as_secs_f64() >= inner.mpd_update_interval,
                None => true,
            }
        };

        if due || send_initial {
            send_initial = false;
            let t0 = Instant::now();
            let json = mpd.build_state_json();
            let dt = t0.elapsed().as_secs_f64();
            let Some(json) = json else {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };

            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(d) = v.get("d") {
                    let cur_state = d.get("state").and_then(|s| s.as_str()).map(String::from);
                    let cur_title = d.get("title").and_then(|s| s.as_str()).map(String::from);
                    if let (Some(prev), Some(cur)) = (last_state.as_ref(), cur_state.as_ref()) {
                        if prev != cur {
                            log_debug!(inner.logger, "MPD: {}->{}", prev, cur);
                        }
                    }
                    last_state = cur_state;
                    if let (Some(prev), Some(cur)) = (last_title.as_ref(), cur_title.as_ref()) {
                        if prev != cur && !cur.is_empty() {
                            log_debug!(inner.logger, "MPD: track=\"{:.30}\"", cur);
                        }
                    }
                    last_title = cur_title;
                }
            }

            *inner.last_mpd_update.lock().unwrap() = Some(now);
            let priority = inner.serial.get_priority("high");
            inner.serial.send_message(&json, priority);

            let mut stats = inner.stats.lock().unwrap();
            stats.mpd_updates += 1;
            stats.mpd_latency_total += dt;
            if dt > stats.mpd_latency_max { stats.mpd_latency_max = dt; }
            drop(stats);

            sleep_check(Duration::from_secs_f64(inner.mpd_update_interval));
        } else {
            let last = inner.last_mpd_update.lock().unwrap().unwrap_or(now);
            let elapsed = now.duration_since(last).as_secs_f64();
            let remaining = (inner.mpd_update_interval - elapsed).max(0.05);
            sleep_check(Duration::from_secs_f64(remaining));
        }
    }
}

fn cava_reader_loop(inner: Arc<Inner>) {
    signal_handler::block_in_thread();
    let frame_interval = Duration::from_secs_f64(1.0 / inner.config.cava.framerate as f64);

    while !signal_handler::received() {
        let result = {
            let mut cava_guard = inner.cava.lock().unwrap();
            match cava_guard.as_mut() {
                Some(c) => c.read_data(64, frame_interval),
                None => ReadResult::None,
            }
        };

        match result {
            ReadResult::Exited => {
                log_error!(inner.logger, "CAVA died in hybrid");
                return;
            }
            ReadResult::Frame(values) => {
                let mut skip = inner.cava_skip.lock().unwrap();
                if skip.remaining > 0 {
                    skip.remaining -= 1;
                    continue;
                }
                drop(skip);

                let priority = inner.serial.get_priority("critical");
                let (ok, sent) = if inner.device_format == "binary" {
                    let bytes: Vec<u8> = values.iter().take(64).map(|&v| v as u8).collect();
                    let header = {
                        let cava_guard = inner.cava.lock().unwrap();
                        cava_guard.as_ref().map(|c| c.binary_header()).unwrap_or(0)
                    };
                    let sent = bytes.len() + 2;
                    (inner.serial.send_binary_spectrum(&bytes, header, priority), sent)
                } else {
                    let json = serde_json::to_string(&SpectrumEnvelope { t: "s", d: &values }).unwrap_or_default();
                    let sent = json.len();
                    (inner.serial.send_message(&json, priority), sent)
                };

                let mut skip = inner.cava_skip.lock().unwrap();
                if !ok {
                    skip.count = if skip.count < 1 { 1 } else { (skip.count * 2).min(8) };
                    skip.remaining = skip.count;
                } else {
                    skip.count = 0;
                }
                drop(skip);

                let mut stats = inner.stats.lock().unwrap();
                stats.cava_frames += 1;
                stats.bytes_sent += sent;
            }
            ReadResult::None => {
                std::thread::sleep(IDLE_SLEEP);
            }
        }
    }
}

fn command_processor_loop(inner: Arc<Inner>) {
    signal_handler::block_in_thread();
    while !signal_handler::received() {
        let line = inner.serial.read_message(Duration::from_millis(50));
        let Some(line) = line else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let Ok(env) = serde_json::from_str::<CmdEnvelope>(&line) else { continue; };
        if env.t != "cmd" { continue; }
        let Some(content) = env.c else { continue; };
        let action = content.action.as_str();
        let params = content.parameters.as_ref();

        inner.stats.lock().unwrap().commands += 1;
        log_debug!(inner.logger, "Cmd: {}", action);

        if !SUPPORTED_COMMANDS.contains(&action) { continue; }

        if action == "exec" {
            if let Some(p) = params {
                if let Some(name) = p.get("name").and_then(|v| v.as_str()) {
                    action_runner::dispatch(name);
                }
            }
            continue;
        }

        let mpd_supported = inner.supports.lock().unwrap().mpd;
        if mpd_supported {
            if let Some(mpd) = inner.mpd.as_ref() {
                mpd.handle_command(action, params);
            }
        } else {
            log_warning!(inner.logger, "Cmd '{}': MPD not supported", action);
        }
    }
}

fn sysinfo_checker_loop(inner: Arc<Inner>) {
    signal_handler::block_in_thread();
    let interval = inner.sysinfo_interval;

    while !signal_handler::received() {
        let json = {
            let mut guard = inner.sysinfo.lock().unwrap();
            guard.as_mut().map(|s| s.build_json())
        };
        if let Some(json) = json {
            let priority = inner.serial.get_priority("normal");
            if inner.serial.send_message(&json, priority) {
                let mut stats = inner.stats.lock().unwrap();
                stats.sysinfo_updates += 1;
                stats.bytes_sent += json.len();
            }
        }
        sleep_check(Duration::from_secs_f64(interval));
    }
}

fn sleep_check(total: Duration) {
    let step = Duration::from_millis(100);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if signal_handler::received() { return; }
        let chunk = remaining.min(step);
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}
