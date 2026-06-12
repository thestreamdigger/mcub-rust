use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::core::config_manager::Config;
use crate::core::logger::Logger;
use crate::{log_debug, log_error, log_info, log_ok, log_warning};

const PROCESS_SETTLE: Duration = Duration::from_millis(100);
const CONFIG_PATH: &str = "/tmp/cava_config";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CavaFormat {
    Ascii,
    Binary,
}

impl CavaFormat {
    pub fn as_str(self) -> &'static str {
        match self { Self::Ascii => "ascii", Self::Binary => "binary" }
    }

    pub fn from_str(s: &str) -> Self {
        if s == "binary" { Self::Binary } else { Self::Ascii }
    }
}

pub struct CavaManager {
    logger: Arc<Logger>,
    config: Arc<Config>,
    child: Option<Child>,
    stdout_fd: Option<OwnedFd>,
    binary_buffer: Vec<u8>,
    spectrum_bars: usize,
    frame_size: usize,
    read_chunk_size: usize,
    cava_format: CavaFormat,
    binary_fd_configured: bool,
}

impl CavaManager {
    pub fn new(
        logger: Arc<Logger>,
        config: Arc<Config>,
        cava_format: Option<&str>,
        spectrum_bars: Option<u32>,
    ) -> Self {
        let format = match cava_format {
            Some(s) if !s.is_empty() => CavaFormat::from_str(s),
            _ => {
                if config.cava.defaults.data_format == "json" {
                    CavaFormat::Ascii
                } else {
                    CavaFormat::Binary
                }
            }
        };

        let bars = spectrum_bars
            .filter(|&n| n > 0)
            .unwrap_or(config.cava.defaults.spectrum_bars) as usize;

        Self {
            logger,
            config,
            child: None,
            stdout_fd: None,
            binary_buffer: Vec::new(),
            spectrum_bars: bars,
            frame_size: bars,
            read_chunk_size: bars * 8,
            cava_format: format,
            binary_fd_configured: false,
        }
    }

    pub fn kill_existing(&self) {
        let out = Command::new("pgrep")
            .args(["-f", "/usr/bin/cava -p /tmp/cava_config"])
            .output();
        let Ok(out) = out else { return };

        let mut found = false;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Ok(pid) = line.trim().parse::<i32>() {
                if pid > 0 {
                    let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
                    found = true;
                }
            }
        }
        if found {
            log_info!(self.logger, "CAVA running, stop");
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn write_config(&self) -> std::io::Result<()> {
        let cava = &self.config.cava;
        log_info!(
            self.logger,
            "CAVA config: {} bars @ {} FPS, format={}",
            self.spectrum_bars,
            cava.framerate,
            self.cava_format.as_str()
        );

        let mut f = fs::File::create(CONFIG_PATH)?;
        writeln!(f, "[general]")?;
        writeln!(f, "framerate = {}", cava.framerate)?;
        writeln!(f, "bars = {}", self.spectrum_bars)?;
        writeln!(f, "autosens = {}", cava.defaults.autosens)?;
        writeln!(f, "sensitivity = {}", cava.defaults.sensitivity)?;
        writeln!(f, "\n[input]")?;
        writeln!(f, "method = {}", cava.input.method)?;
        writeln!(f, "source = {}", cava.input.source)?;
        writeln!(f, "\n[output]")?;
        writeln!(f, "method = {}", cava.output.method)?;
        writeln!(f, "raw_target = {}", cava.output.raw_target)?;
        writeln!(f, "channels = {}", cava.input.channels)?;
        writeln!(f, "data_format = {}", self.cava_format.as_str())?;
        if self.cava_format == CavaFormat::Binary {
            writeln!(f, "bit_format = 8bit")?;
        } else {
            writeln!(f, "ascii_max_range = {}", cava.defaults.ascii_range)?;
        }
        // cava 0.10.x [smoothing]: noise_reduction, monstercat, waves only
        // (gravity removed upstream; unknown keys silently ignored)
        writeln!(f, "\n[smoothing]")?;
        writeln!(f, "noise_reduction = {}", cava.smoothing.noise_reduction)?;
        writeln!(f, "monstercat = {}", cava.smoothing.monstercat)?;
        writeln!(f, "waves = {}", cava.smoothing.waves)?;
        writeln!(f, "\n[eq]")?;
        for (i, v) in cava.eq.iter().enumerate() {
            writeln!(f, "{} = {:.1}", i + 1, v)?;
        }
        log_debug!(self.logger, "CAVA config written");
        Ok(())
    }

    fn wait_for_loopback(&self, max_wait_secs: u32) -> bool {
        for i in 0..=max_wait_secs {
            if check_loopback() {
                if i > 0 {
                    log_info!(self.logger, "Loopback ready after {}s", i);
                }
                return true;
            }
            if i < max_wait_secs {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        if max_wait_secs > 0 {
            log_warning!(self.logger, "Loopback unavailable");
        }
        false
    }

    pub fn start(&mut self, loopback_wait_secs: u32) -> bool {
        self.kill_existing();
        if let Err(e) = self.write_config() {
            log_error!(self.logger, "CAVA config failed: {}", e);
            return false;
        }

        self.binary_buffer.clear();
        self.binary_fd_configured = false;

        log_info!(self.logger, "CAVA starting");
        if !self.wait_for_loopback(loopback_wait_secs) {
            if loopback_wait_secs > 0 {
                log_error!(self.logger, "Loopback timeout ({}s)", loopback_wait_secs);
            }
            return false;
        }

        let mut child = match Command::new("/usr/bin/cava")
            .args(["-p", CONFIG_PATH])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log_error!(self.logger, "CAVA spawn failed: {}", e);
                return false;
            }
        };

        let stdout = child.stdout.take().expect("piped");
        std::thread::sleep(PROCESS_SETTLE);

        match child.try_wait() {
            Ok(None) => {
                log_ok!(self.logger, "CAVA ready");
                let raw: RawFd = stdout.as_raw_fd();
                std::mem::forget(stdout);
                self.stdout_fd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
                self.child = Some(child);
                true
            }
            Ok(Some(status)) => {
                log_error!(self.logger, "CAVA failed (exit={:?})", status.code());
                false
            }
            Err(e) => {
                log_error!(self.logger, "CAVA wait error: {}", e);
                false
            }
        }
    }

    pub fn binary_header(&self) -> u8 {
        (self.spectrum_bars as u8) & 0x7F
    }

    pub fn read_data(&mut self, max_values: usize, timeout: Duration) -> ReadResult {
        if self.child.is_none() || self.stdout_fd.is_none() {
            return ReadResult::None;
        }

        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => return ReadResult::Exited,
                Ok(None) => {}
                Err(_) => return ReadResult::Exited,
            }
        }

        match self.cava_format {
            CavaFormat::Binary => self.read_binary(max_values, timeout),
            CavaFormat::Ascii => self.read_ascii(max_values, timeout),
        }
    }

    fn read_ascii(&mut self, max_values: usize, timeout: Duration) -> ReadResult {
        let fd = self.stdout_fd.as_ref().unwrap();
        let raw = fd.as_raw_fd();
        let ms = timeout.as_millis().max(1) as u16;

        let mut pfds = [PollFd::new(fd.as_fd_ref(), PollFlags::POLLIN)];
        match poll(&mut pfds, PollTimeout::from(ms)) {
            Ok(n) if n > 0 => {}
            _ => return ReadResult::None,
        }

        let mut line = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        while line.len() < 1023 {
            let n = unsafe { libc::read(raw, byte.as_mut_ptr() as *mut _, 1) };
            if n <= 0 { break; }
            if byte[0] == b'\n' { break; }
            line.push(byte[0]);
        }
        if line.is_empty() {
            return ReadResult::None;
        }

        let s = match std::str::from_utf8(&line) {
            Ok(s) => s,
            Err(_) => return ReadResult::None,
        };
        let values: Vec<i32> = s
            .split(';')
            .filter_map(|t| t.parse::<i32>().ok())
            .take(max_values)
            .collect();

        if values.len() != self.spectrum_bars {
            return ReadResult::None;
        }
        ReadResult::Frame(values)
    }

    fn read_binary(&mut self, max_values: usize, timeout: Duration) -> ReadResult {
        let fd = self.stdout_fd.as_ref().unwrap();
        let raw = fd.as_raw_fd();

        if !self.binary_fd_configured {
            let flags = unsafe { libc::fcntl(raw, libc::F_GETFL, 0) };
            unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            self.binary_fd_configured = true;
            log_debug!(self.logger, "Binary read: non-blocking configured");
        }

        // Block in poll until data arrives (or timeout) instead of caller-side
        // sleep-polling: 5ms sleep quantization cannot sustain 60fps cadence
        if self.binary_buffer.len() < self.frame_size {
            let ms = timeout.as_millis().max(1) as u16;
            let mut pfds = [PollFd::new(fd.as_fd_ref(), PollFlags::POLLIN)];
            match poll(&mut pfds, PollTimeout::from(ms)) {
                Ok(n) if n > 0 => {}
                _ => return ReadResult::None,
            }
        }

        let mut chunk = vec![0u8; self.read_chunk_size];
        let n = unsafe { libc::read(raw, chunk.as_mut_ptr() as *mut _, chunk.len()) };
        if n > 0 {
            self.binary_buffer.extend_from_slice(&chunk[..n as usize]);
        }

        if self.binary_buffer.len() < self.frame_size {
            return ReadResult::None;
        }

        // Skip to latest frame
        let num_frames = self.binary_buffer.len() / self.frame_size;
        let skip = (num_frames - 1) * self.frame_size;
        if skip > 0 {
            self.binary_buffer.drain(..skip);
        }

        let count = self.frame_size.min(max_values);
        let values: Vec<i32> = self.binary_buffer[..count].iter().map(|&b| b as i32).collect();
        self.binary_buffer.drain(..self.frame_size);
        ReadResult::Frame(values)
    }

    pub fn cleanup(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = Pid::from_raw(child.id() as i32);
            let _ = kill(pid, Signal::SIGTERM);
            std::thread::sleep(Duration::from_secs(1));
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => {
                    let _ = kill(pid, Signal::SIGKILL);
                    let _ = child.wait();
                    log_warning!(self.logger, "CAVA killed (timeout)");
                }
                _ => {
                    let _ = child.wait();
                    log_info!(self.logger, "CAVA stopped");
                }
            }
        }
        self.stdout_fd = None;
        self.binary_buffer.clear();
        if PathBuf::from(CONFIG_PATH).exists() {
            let _ = fs::remove_file(CONFIG_PATH);
            log_debug!(self.logger, "CAVA config removed");
        }
    }
}

impl Drop for CavaManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub enum ReadResult {
    None,
    Frame(Vec<i32>),
    Exited,
}

pub fn check_loopback() -> bool {
    let out = Command::new("arecord").arg("-l").output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("Loopback") || String::from_utf8_lossy(&o.stderr).contains("Loopback")
        }
        Err(_) => false,
    }
}

trait AsFdRef {
    fn as_fd_ref(&self) -> std::os::fd::BorrowedFd<'_>;
}

impl AsFdRef for OwnedFd {
    fn as_fd_ref(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.as_fd()
    }
}
