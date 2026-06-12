use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{McubError, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
    pub timestamp: TimestampConfig,
    pub file: FileLogConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TimestampConfig {
    pub enable: bool,
    pub format: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FileLogConfig {
    pub enable: bool,
    pub dir: String,
    pub bridge_log: String,
    pub watcher_log: String,
    pub max_size: u64,
    pub backup_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatcherConfig {
    pub device_check_interval: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub baudrate: u32,
    pub timeout: f64,
    pub reconnect_delay: f64,
    pub max_reconnect_attempts: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BridgeConfig {
    pub device: String,
    pub connection: ConnectionConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CavaInputConfig {
    pub method: String,
    pub source: String,
    pub channels: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CavaOutputConfig {
    pub method: String,
    pub raw_target: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CavaDefaultsConfig {
    pub data_format: String,
    pub spectrum_bars: u32,
    pub ascii_range: u32,
    pub autosens: u32,
    pub sensitivity: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CavaSmoothingConfig {
    pub noise_reduction: u32,
    pub monstercat: u32,
    pub waves: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CavaConfig {
    pub framerate: u32,
    pub input: CavaInputConfig,
    pub output: CavaOutputConfig,
    pub defaults: CavaDefaultsConfig,
    pub smoothing: CavaSmoothingConfig,
    #[serde(deserialize_with = "deserialize_eq_map", default = "default_eq")]
    pub eq: [f64; 8],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MpdReconnectConfig {
    pub delay: f64,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MpdConfig {
    pub host: String,
    pub port: u16,
    pub update_interval: f64,
    pub reconnect: MpdReconnectConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SysinfoConfig {
    pub update_interval: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    pub serial_poll_ms: u32,
    pub max_queue_size: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PriorityConfig {
    pub critical: i32,
    pub high: i32,
    pub normal: i32,
    pub low: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionEntry {
    pub command: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub logging: LoggingConfig,
    pub watcher: WatcherConfig,
    pub bridge: BridgeConfig,
    pub cava: CavaConfig,
    pub mpd: MpdConfig,
    pub sysinfo: SysinfoConfig,
    pub performance: PerformanceConfig,
    pub priority_levels: PriorityConfig,
    pub actions: HashMap<String, ActionEntry>,
    #[serde(skip)]
    pub config_path: PathBuf,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "INFO".into(),
            format: "[{timestamp}] [{component}] [{level}] {message}".into(),
            timestamp: TimestampConfig::default(),
            file: FileLogConfig::default(),
        }
    }
}

impl Default for TimestampConfig {
    fn default() -> Self {
        Self {
            enable: true,
            format: "%Y-%m-%d %H:%M:%S".into(),
        }
    }
}

impl Default for FileLogConfig {
    fn default() -> Self {
        Self {
            enable: true,
            dir: "../logs".into(),
            bridge_log: "bridge.log".into(),
            watcher_log: "watcher.log".into(),
            max_size: 1_048_576,
            backup_count: 5,
        }
    }
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self { device_check_interval: 5 }
    }
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            baudrate: 115_200,
            timeout: 0.1,
            reconnect_delay: 0.1,
            max_reconnect_attempts: 10,
        }
    }
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            device: "/dev/ttyACM1".into(),
            connection: ConnectionConfig::default(),
        }
    }
}

impl Default for CavaInputConfig {
    fn default() -> Self {
        Self {
            method: "alsa".into(),
            source: "hw:Loopback,1,0".into(),
            channels: "stereo".into(),
        }
    }
}

impl Default for CavaOutputConfig {
    fn default() -> Self {
        Self {
            method: "raw".into(),
            raw_target: "/dev/stdout".into(),
        }
    }
}

impl Default for CavaDefaultsConfig {
    fn default() -> Self {
        Self {
            data_format: "json".into(),
            spectrum_bars: 16,
            ascii_range: 8,
            autosens: 1,
            sensitivity: 100,
        }
    }
}

fn default_eq() -> [f64; 8] {
    [1.0; 8]
}

impl Default for CavaConfig {
    fn default() -> Self {
        Self {
            framerate: 60,
            input: CavaInputConfig::default(),
            output: CavaOutputConfig::default(),
            defaults: CavaDefaultsConfig::default(),
            smoothing: CavaSmoothingConfig::default(),
            eq: default_eq(),
        }
    }
}

impl Default for MpdReconnectConfig {
    fn default() -> Self {
        Self { delay: 5.0, max_attempts: 30 }
    }
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 6600,
            update_interval: 1.0,
            reconnect: MpdReconnectConfig::default(),
        }
    }
}

impl Default for SysinfoConfig {
    fn default() -> Self {
        Self { update_interval: 2.0 }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            serial_poll_ms: 5,
            max_queue_size: 60,
        }
    }
}

impl Default for PriorityConfig {
    fn default() -> Self {
        Self { critical: -1, high: 0, normal: 1, low: 2 }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            logging: LoggingConfig::default(),
            watcher: WatcherConfig::default(),
            bridge: BridgeConfig::default(),
            cava: CavaConfig::default(),
            mpd: MpdConfig::default(),
            sysinfo: SysinfoConfig::default(),
            performance: PerformanceConfig::default(),
            priority_levels: PriorityConfig::default(),
            actions: HashMap::new(),
            config_path: PathBuf::new(),
        }
    }
}

fn deserialize_eq_map<'de, D>(de: D) -> std::result::Result<[f64; 8], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let map: HashMap<String, f64> = HashMap::deserialize(de)?;
    let mut eq = [1.0_f64; 8];
    for (k, v) in map {
        if let Ok(i) = k.parse::<usize>() {
            if (1..=8).contains(&i) {
                eq[i - 1] = v;
            }
        }
    }
    Ok(eq)
}

impl Config {
    pub fn load(base_dir: Option<&Path>) -> Result<Self> {
        let path = match base_dir {
            Some(d) => d.join("shared/settings.json"),
            None => PathBuf::from("shared/settings.json"),
        };

        let mut config = match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str::<Config>(&s)
                .map_err(|e| McubError::Config(format!("{}: {}", path.display(), e)))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e.into()),
        };
        config.config_path = path;
        Ok(config)
    }

    pub fn get_priority(&self, level: &str) -> i32 {
        match level {
            "critical" => self.priority_levels.critical,
            "high" => self.priority_levels.high,
            "normal" => self.priority_levels.normal,
            "low" => self.priority_levels.low,
            _ => self.priority_levels.normal,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CavaEnvConfig {
    pub device_format: String,
    pub spectrum_bars: Option<u32>,
    pub cava_format: String,
}

pub fn get_cava_env_config() -> CavaEnvConfig {
    let device_format = env::var("MCUB_DEVICE_FORMAT").unwrap_or_else(|_| "json".into());
    let spectrum_bars = env::var("MCUB_SPECTRUM_BARS")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok());
    let cava_format = if device_format == "binary" {
        "binary".into()
    } else {
        "ascii".into()
    };
    CavaEnvConfig { device_format, spectrum_bars, cava_format }
}
