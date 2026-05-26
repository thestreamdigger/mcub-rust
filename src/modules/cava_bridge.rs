use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::core::cava_manager::{CavaManager, ReadResult};
use crate::core::config_manager::{Config, get_cava_env_config};
use crate::core::logger::Logger;
use crate::core::serial_comm::SerialComm;
use crate::core::signal_handler;
use crate::{log_debug, log_error, log_info, log_warning};

const STATS_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_SLEEP: Duration = Duration::from_micros(5000);
const READ_TIMEOUT: Duration = Duration::from_micros(5000);

#[derive(Serialize)]
struct SpectrumEnvelope<'a> {
    t: &'a str,
    d: &'a [i32],
}

pub struct CavaBridge {
    serial: Arc<SerialComm>,
    cava: CavaManager,
    logger: Arc<Logger>,
    device_format: String,
    stats_cava_frames: u32,
    stats_bytes_sent: usize,
    stats_last_report: Instant,
    skip_count: u32,
    skip_remaining: u32,
}

impl CavaBridge {
    pub fn new(config: Arc<Config>, logger: Arc<Logger>) -> Self {
        let env = get_cava_env_config();
        let serial = Arc::new(SerialComm::new(Arc::clone(&config), Arc::clone(&logger), "CAVA"));
        let bars = env.spectrum_bars;
        let cava = CavaManager::new(Arc::clone(&logger), Arc::clone(&config), Some(&env.cava_format), bars);

        if env.device_format == "binary" {
            log_info!(logger, "Binary mode: 8bit, sync=0xCA");
        }

        Self {
            serial,
            cava,
            logger,
            device_format: env.device_format,
            stats_cava_frames: 0,
            stats_bytes_sent: 0,
            stats_last_report: Instant::now(),
            skip_count: 0,
            skip_remaining: 0,
        }
    }

    pub fn run(&mut self) -> i32 {
        if self.serial.connect().is_err() {
            log_error!(self.logger, "No device");
            return 1;
        }
        if self.serial.identify_device().is_none() {
            log_warning!(self.logger, "Device identify failed");
        }

        if !self.cava.start(120) {
            log_error!(self.logger, "CAVA failed");
            return 1;
        }

        log_info!(self.logger, "Main loop");

        while !signal_handler::received() {
            self.report_stats();

            match self.cava.read_data(64, READ_TIMEOUT) {
                ReadResult::Exited => {
                    log_error!(self.logger, "CAVA died");
                    break;
                }
                ReadResult::Frame(values) => {
                    if self.skip_remaining > 0 {
                        self.skip_remaining -= 1;
                    } else if !self.process_and_send(&values) {
                        self.skip_count = if self.skip_count < 1 { 1 } else { (self.skip_count * 2).min(8) };
                        self.skip_remaining = self.skip_count;
                    } else {
                        self.skip_count = 0;
                    }
                }
                ReadResult::None => {
                    std::thread::sleep(IDLE_SLEEP);
                }
            }
        }

        self.cleanup();
        0
    }

    fn process_and_send(&mut self, data: &[i32]) -> bool {
        self.stats_cava_frames += 1;
        let priority = self.serial.get_priority("critical");

        if self.device_format == "binary" {
            let bytes: Vec<u8> = data.iter().take(64).map(|&v| v as u8).collect();
            let header = self.cava.binary_header();
            self.stats_bytes_sent += bytes.len() + 2;
            self.serial.send_binary_spectrum(&bytes, header, priority)
        } else {
            let json = serde_json::to_string(&SpectrumEnvelope { t: "s", d: data }).unwrap_or_default();
            self.stats_bytes_sent += json.len();
            self.serial.send_message(&json, priority)
        }
    }

    fn report_stats(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.stats_last_report);
        if elapsed < STATS_INTERVAL { return; }
        let secs = elapsed.as_secs_f64();
        let fps = self.stats_cava_frames as f64 / secs;
        log_debug!(
            self.logger,
            "Stats: CAVA={} ({:.1}/s), sent={:.1}KB, uptime={}s",
            self.stats_cava_frames,
            fps,
            self.stats_bytes_sent as f64 / 1024.0,
            secs as u32
        );
        self.stats_cava_frames = 0;
        self.stats_bytes_sent = 0;
        self.stats_last_report = now;
    }

    pub fn cleanup(&mut self) {
        log_info!(self.logger, "Cleanup");
        self.serial.close();
        self.cava.cleanup();
    }
}
