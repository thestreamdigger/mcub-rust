use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::statvfs::statvfs;
use serde::Serialize;

use crate::core::config_manager::Config;
use crate::core::logger::Logger;
use crate::core::serial_comm::SerialComm;
use crate::core::signal_handler;
use crate::{log_error, log_info, log_warning};

const STATS_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct CpuPrev {
    user: i64,
    nice: i64,
    system: i64,
    idle: i64,
    iowait: i64,
    irq: i64,
    softirq: i64,
    valid: bool,
}

pub struct SysinfoBridge {
    serial: Arc<SerialComm>,
    logger: Arc<Logger>,
    update_interval: f64,
    collector: SysinfoCollector,
    stats_updates: u32,
    stats_bytes_sent: usize,
    stats_last_report: Instant,
}

pub struct SysinfoCollector {
    cpu_prev: CpuPrev,
}

impl Default for SysinfoCollector {
    fn default() -> Self {
        Self { cpu_prev: CpuPrev::default() }
    }
}

impl SysinfoCollector {
    pub fn new() -> Self { Self::default() }

    pub fn warmup(&mut self) {
        self.collect_cpu();
    }

    pub fn build_json(&mut self) -> String {
        let mut d = SysinfoData::default();
        d.cpu = self.collect_cpu();
        d.temp = collect_temp().map(|t| (t * 10.0).trunc() / 10.0);
        d.mem = collect_mem();
        d.disk = collect_disk();
        d.load = collect_load().map(|l| format!("{:.2}", l));
        d.up = collect_uptime();
        d.ip = collect_ip();
        d.freq = collect_freq();
        d.time = Some(chrono::Local::now().format("%H:%M").to_string());
        serde_json::to_string(&Envelope { t: "sys", d }).unwrap_or_default()
    }

    fn collect_cpu(&mut self) -> Option<i32> {
        let s = fs::read_to_string("/proc/stat").ok()?;
        let first = s.lines().next()?;
        let nums: Vec<i64> = first
            .split_whitespace()
            .skip(1)
            .take(7)
            .filter_map(|t| t.parse().ok())
            .collect();
        if nums.len() < 7 { return None; }
        let (user, nice, system, idle, iowait, irq, softirq) =
            (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5], nums[6]);

        if !self.cpu_prev.valid {
            self.cpu_prev = CpuPrev { user, nice, system, idle, iowait, irq, softirq, valid: true };
            return None;
        }

        let d_user = user - self.cpu_prev.user;
        let d_nice = nice - self.cpu_prev.nice;
        let d_system = system - self.cpu_prev.system;
        let d_idle = idle - self.cpu_prev.idle;
        let d_iowait = iowait - self.cpu_prev.iowait;
        let d_irq = irq - self.cpu_prev.irq;
        let d_softirq = softirq - self.cpu_prev.softirq;

        self.cpu_prev = CpuPrev { user, nice, system, idle, iowait, irq, softirq, valid: true };

        let busy = d_user + d_nice + d_system + d_irq + d_softirq;
        let total = busy + d_idle + d_iowait;
        if total == 0 { return Some(0); }
        Some((100 * busy / total) as i32)
    }
}

#[derive(Serialize)]
struct Envelope<'a> {
    t: &'a str,
    d: SysinfoData,
}

#[derive(Serialize, Default)]
struct SysinfoData {
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mem: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    load: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    up: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freq: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<String>,
}

impl SysinfoBridge {
    pub fn new(config: Arc<Config>, logger: Arc<Logger>) -> Self {
        let interval = if config.sysinfo.update_interval < 0.5 {
            2.0
        } else {
            config.sysinfo.update_interval
        };
        let serial = Arc::new(SerialComm::new(config, Arc::clone(&logger), "SysInfo"));
        Self {
            serial,
            logger,
            update_interval: interval,
            collector: SysinfoCollector::new(),
            stats_updates: 0,
            stats_bytes_sent: 0,
            stats_last_report: Instant::now(),
        }
    }

    pub fn run(&mut self) -> i32 {
        if self.serial.connect().is_err() {
            log_error!(self.logger, "Device failed");
            return 1;
        }
        if self.serial.identify_device().is_none() {
            log_warning!(self.logger, "Device identify failed");
        }

        self.collector.warmup();
        std::thread::sleep(Duration::from_millis(100));

        log_info!(self.logger, "Main loop ({:.1}s interval)", self.update_interval);

        while !signal_handler::received() {
            self.report_stats();

            let json = self.collector.build_json();
            let priority = self.serial.get_priority("normal");
            self.serial.send_message(&json, priority);
            self.stats_updates += 1;
            self.stats_bytes_sent += json.len();

            let steps = (self.update_interval * 10.0) as u32;
            for _ in 0..steps {
                if signal_handler::received() { break; }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        self.cleanup();
        0
    }

    fn report_stats(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.stats_last_report) < STATS_INTERVAL { return; }
        log_info!(
            self.logger,
            "Stats: sysinfo={}, sent={:.1}KB",
            self.stats_updates,
            self.stats_bytes_sent as f64 / 1024.0
        );
        self.stats_updates = 0;
        self.stats_bytes_sent = 0;
        self.stats_last_report = now;
    }

    pub fn cleanup(&self) {
        log_info!(self.logger, "Cleanup");
        self.serial.close();
    }
}

fn collect_temp() -> Option<f64> {
    let s = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    s.trim().parse::<i64>().ok().map(|raw| raw as f64 / 1000.0)
}

fn collect_mem() -> Option<i32> {
    let s = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total: i64 = 0;
    let mut available: i64 = 0;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest.split_whitespace().next()?.parse().ok()?;
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = rest.split_whitespace().next()?.parse().ok()?;
        }
        if total > 0 && available > 0 { break; }
    }
    if total <= 0 { return None; }
    Some((100 * (total - available) / total) as i32)
}

fn collect_disk() -> Option<i32> {
    let st = statvfs("/").ok()?;
    if st.blocks() == 0 { return None; }
    let used = st.blocks() - st.blocks_available();
    Some((100 * used as u64 / st.blocks() as u64) as i32)
}

fn collect_load() -> Option<f64> {
    let s = fs::read_to_string("/proc/loadavg").ok()?;
    s.split_whitespace().next()?.parse().ok()
}

fn collect_uptime() -> Option<i64> {
    let s = fs::read_to_string("/proc/uptime").ok()?;
    s.split_whitespace().next()?.parse::<f64>().ok().map(|v| v as i64)
}

fn collect_freq() -> Option<i32> {
    let s = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq").ok()?;
    s.trim().parse::<i32>().ok().map(|khz| khz / 1000)
}

fn collect_ip() -> Option<String> {
    let addrs = nix::ifaddrs::getifaddrs().ok()?;
    let mut fallback: Option<String> = None;
    let mut wlan: Option<String> = None;

    for ifa in addrs {
        let Some(sock) = ifa.address else { continue };
        let Some(ipv4) = sock.as_sockaddr_in() else { continue };
        let flags = ifa.flags;
        if flags.contains(nix::net::if_::InterfaceFlags::IFF_LOOPBACK) { continue; }

        let ip = ipv4.ip().to_string();
        match ifa.interface_name.as_str() {
            "eth0" | "end0" => return Some(ip),
            "wlan0" if wlan.is_none() => wlan = Some(ip),
            _ if fallback.is_none() => fallback = Some(ip),
            _ => {}
        }
    }
    wlan.or(fallback)
}
