use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::core::config_manager::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogLevel {
    Debug = 10,
    Info = 20,
    Ok = 25,
    Warning = 30,
    Error = 40,
    Critical = 50,
}

impl LogLevel {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "DEBUG" => Self::Debug,
            "INFO" => Self::Info,
            "OK" => Self::Ok,
            "WARNING" => Self::Warning,
            "ERROR" => Self::Error,
            "CRITICAL" => Self::Critical,
            _ => Self::Info,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Ok => "OK",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }
}

struct FileSink {
    path: PathBuf,
    file: File,
    current_size: u64,
    max_size: u64,
    backup_count: u32,
}

impl FileSink {
    fn write_line(&mut self, line: &str) {
        if let Ok(n) = self.file.write(line.as_bytes()) {
            let _ = self.file.write_all(b"\n");
            let _ = self.file.flush();
            self.current_size += n as u64 + 1;
            if self.max_size > 0 && self.current_size >= self.max_size {
                self.rotate();
            }
        }
    }

    fn rotate(&mut self) {
        for i in (1..=self.backup_count).rev() {
            let old = if i > 1 {
                self.path.with_extension(format!(
                    "{}.{}",
                    self.path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or(""),
                    i - 1
                ))
            } else {
                self.path.clone()
            };
            let new = self.path.with_extension(format!(
                "{}.{}",
                self.path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
                i
            ));
            let _ = fs::rename(&old, &new);
        }
        if let Ok(f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            self.file = f;
            self.current_size = 0;
        }
    }
}

struct LoggerState {
    file_sink: Option<FileSink>,
}

pub struct Logger {
    level: LogLevel,
    component: String,
    format: String,
    timestamp_enable: bool,
    timestamp_format: String,
    state: Mutex<LoggerState>,
}

impl Logger {
    pub fn new(config: Option<&Config>, component: &str, base_dir: Option<&Path>) -> Self {
        let component = if component.is_empty() {
            "BRIDGE".to_string()
        } else {
            component.to_string()
        };

        let Some(config) = config else {
            return Self {
                level: LogLevel::Info,
                component,
                format: "[{timestamp}] [{component}] [{level}] {message}".into(),
                timestamp_enable: true,
                timestamp_format: "%Y-%m-%d %H:%M:%S".into(),
                state: Mutex::new(LoggerState { file_sink: None }),
            };
        };

        let logging = &config.logging;
        let level = LogLevel::from_str(&logging.level);

        let file_sink = if logging.file.enable {
            let dir = resolve_log_dir(&logging.file.dir, base_dir);
            let _ = fs::create_dir_all(&dir);
            let filename = if component.eq_ignore_ascii_case("WATCHER") {
                &logging.file.watcher_log
            } else {
                &logging.file.bridge_log
            };
            let path = dir.join(filename);
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
                .map(|mut f| {
                    let size = f.seek(SeekFrom::End(0)).unwrap_or(0);
                    FileSink {
                        path,
                        file: f,
                        current_size: size,
                        max_size: logging.file.max_size,
                        backup_count: logging.file.backup_count,
                    }
                })
        } else {
            None
        };

        Self {
            level,
            component,
            format: logging.format.clone(),
            timestamp_enable: logging.timestamp.enable,
            timestamp_format: logging.timestamp.format.clone(),
            state: Mutex::new(LoggerState { file_sink }),
        }
    }

    pub fn debug(&self, msg: &str) { self.log(LogLevel::Debug, msg); }
    pub fn info(&self, msg: &str) { self.log(LogLevel::Info, msg); }
    pub fn ok(&self, msg: &str) { self.log(LogLevel::Ok, msg); }
    pub fn warning(&self, msg: &str) { self.log(LogLevel::Warning, msg); }
    pub fn error(&self, msg: &str) { self.log(LogLevel::Error, msg); }
    pub fn critical(&self, msg: &str) { self.log(LogLevel::Critical, msg); }

    fn log(&self, level: LogLevel, message: &str) {
        if level < self.level {
            return;
        }

        let timestamp = if self.timestamp_enable {
            chrono::Local::now().format(&self.timestamp_format).to_string()
        } else {
            String::new()
        };

        let output = self.render(level, message, &timestamp);

        let mut state = self.state.lock().expect("logger mutex poisoned");
        eprintln!("{output}");
        if let Some(sink) = state.file_sink.as_mut() {
            sink.write_line(&output);
        }
    }

    fn render(&self, level: LogLevel, message: &str, timestamp: &str) -> String {
        let mut out = String::with_capacity(self.format.len() + message.len() + 32);
        let bytes = self.format.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let rest = &self.format[i..];
            if !self.timestamp_enable && rest.starts_with("[{timestamp}]") {
                i += "[{timestamp}]".len();
                if self.format.as_bytes().get(i) == Some(&b' ') {
                    i += 1;
                }
                continue;
            }
            if rest.starts_with("{timestamp}") {
                if self.timestamp_enable {
                    out.push_str(timestamp);
                }
                i += "{timestamp}".len();
            } else if rest.starts_with("{component}") {
                out.push_str(&self.component);
                i += "{component}".len();
            } else if rest.starts_with("{level}") {
                out.push_str(level.as_str());
                i += "{level}".len();
            } else if rest.starts_with("{message}") {
                out.push_str(message);
                i += "{message}".len();
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        out
    }
}

fn resolve_log_dir(dir: &str, base_dir: Option<&Path>) -> PathBuf {
    if dir.starts_with('/') {
        return PathBuf::from(dir);
    }
    let Some(base) = base_dir else {
        return PathBuf::from(dir);
    };
    let mut stripped = dir;
    while let Some(rest) = stripped.strip_prefix("../") {
        stripped = rest;
    }
    if let Some(rest) = stripped.strip_prefix("./") {
        stripped = rest;
    }
    base.join(stripped)
}

#[macro_export]
macro_rules! log_debug {
    ($logger:expr, $($arg:tt)*) => { $logger.debug(&format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_info {
    ($logger:expr, $($arg:tt)*) => { $logger.info(&format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_ok {
    ($logger:expr, $($arg:tt)*) => { $logger.ok(&format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_warning {
    ($logger:expr, $($arg:tt)*) => { $logger.warning(&format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_error {
    ($logger:expr, $($arg:tt)*) => { $logger.error(&format!($($arg)*)) };
}
