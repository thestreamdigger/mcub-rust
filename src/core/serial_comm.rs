use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::core::config_manager::Config;
use crate::core::logger::Logger;
use crate::core::reconnection::Reconnection;
use crate::core::serial_queue::SerialQueue;
use crate::error::{McubError, Result};
use crate::{log_error, log_info, log_ok};

pub const SYNC_BYTE: u8 = 0xCA;

pub struct SerialComm {
    config: Arc<Config>,
    logger: Arc<Logger>,
    bridge_name: String,
    state: Mutex<SerialState>,
    reconnection: Mutex<Reconnection>,
    poll_timeout: Duration,
}

struct SerialState {
    fd: Option<OwnedFd>,
    device_path: String,
    queue: Option<Arc<SerialQueue>>,
}

impl SerialComm {
    pub fn new(config: Arc<Config>, logger: Arc<Logger>, bridge_name: &str) -> Self {
        let recon = Reconnection::new(
            Arc::clone(&logger),
            config.bridge.connection.max_reconnect_attempts,
            config.bridge.connection.reconnect_delay,
        );
        let poll_timeout = Duration::from_millis(config.performance.serial_poll_ms as u64);

        Self {
            config,
            logger,
            bridge_name: bridge_name.to_string(),
            state: Mutex::new(SerialState {
                fd: None,
                device_path: String::new(),
                queue: None,
            }),
            reconnection: Mutex::new(recon),
            poll_timeout,
        }
    }

    pub fn connect(&self) -> Result<()> {
        self.close();

        let device_path = self.resolve_device_path()?;
        let fd = open_serial(&device_path, self.config.bridge.connection.baudrate)?;

        std::thread::sleep(Duration::from_millis(500));

        let queue_timeout = self.config.performance.queue_timeout_ms as f64 / 1000.0;
        let queue = Arc::new(SerialQueue::new(
            fd.as_raw_fd(),
            Arc::clone(&self.logger),
            &self.bridge_name,
            queue_timeout,
            self.config.performance.max_queue_size as usize,
        ));
        queue.start();

        let mut state = self.state.lock().unwrap();
        state.device_path = device_path.clone();
        state.fd = Some(fd);
        state.queue = Some(queue);
        drop(state);

        log_ok!(self.logger, "Serial: {}", device_path);
        self.reconnection.lock().unwrap().reset();
        Ok(())
    }

    pub fn close(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(queue) = state.queue.take() {
            queue.stop();
        }
        state.fd = None;
    }

    pub fn send_message(&self, json_str: &str, priority: i32) -> bool {
        let state = self.state.lock().unwrap();
        match state.queue.as_ref() {
            Some(q) => q.send_json(json_str, priority),
            None => false,
        }
    }

    pub fn send_binary_spectrum(&self, raw_data: &[u8], header_byte: u8, priority: i32) -> bool {
        let state = self.state.lock().unwrap();
        match state.queue.as_ref() {
            Some(q) => q.send_binary_spectrum(raw_data, header_byte, SYNC_BYTE, priority),
            None => false,
        }
    }

    pub fn read_message(&self, timeout: Duration) -> Option<String> {
        let timeout = if timeout.is_zero() { self.poll_timeout } else { timeout };
        let state = self.state.lock().unwrap();
        let fd = state.fd.as_ref()?;
        readline(fd.as_fd().as_raw_fd(), timeout)
    }

    pub fn identify_device(&self) -> Option<String> {
        let state = self.state.lock().unwrap();
        let fd = state.fd.as_ref()?;
        let raw = fd.as_raw_fd();

        unsafe {
            let flag: libc::c_int = libc::TIOCM_DTR;
            libc::ioctl(raw, libc::TIOCMBIC, &flag);
            std::thread::sleep(Duration::from_millis(100));
            libc::ioctl(raw, libc::TIOCMBIS, &flag);
            std::thread::sleep(Duration::from_millis(2000));

            // Drain (consume buffered boot-time chatter), matching watcher flow
            let mut drain_buf = [0u8; 256];
            loop {
                let n = libc::read(raw, drain_buf.as_mut_ptr() as *mut _, drain_buf.len());
                if n <= 0 { break; }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        let cmd = b"{\"t\":\"id\",\"c\":\"identify\"}\n";
        let w = unsafe { libc::write(raw, cmd.as_ptr() as *const _, cmd.len()) };
        if w < 0 {
            return None;
        }

        if let Some(s) = readline(raw, Duration::from_millis(2000)) {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(200));
        readline(raw, Duration::from_millis(2000))
    }

    pub fn should_reconnect(&self) -> bool {
        self.reconnection.lock().unwrap().should_attempt("serial device")
    }

    pub fn get_priority(&self, level: &str) -> i32 {
        self.config.get_priority(level)
    }

    fn resolve_device_path(&self) -> Result<String> {
        if let Ok(env_path) = std::env::var("DEVICE_PATH") {
            if !env_path.is_empty() {
                log_info!(self.logger, "Device: {}", env_path);
                return Ok(env_path);
            }
        }
        let cfg_path = &self.config.bridge.device;
        if !cfg_path.is_empty() && Path::new(cfg_path).exists() {
            log_info!(self.logger, "Device: {}", cfg_path);
            return Ok(cfg_path.clone());
        }
        log_error!(self.logger, "No device");
        Err(McubError::DeviceNotFound)
    }
}

impl Drop for SerialComm {
    fn drop(&mut self) {
        self.close();
    }
}

fn open_serial(path: &str, baudrate: u32) -> Result<OwnedFd> {
    let c_path = std::ffi::CString::new(path).map_err(|_| McubError::Config("path".into()))?;
    let raw = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK) };
    if raw < 0 {
        return Err(McubError::Io(std::io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL, 0);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }

    let speed = match baudrate {
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        230400 => libc::B230400,
        460800 => libc::B460800,
        _ => libc::B115200,
    };

    unsafe {
        let mut tty: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd.as_raw_fd(), &mut tty) != 0 {
            return Err(McubError::Io(std::io::Error::last_os_error()));
        }
        libc::cfsetospeed(&mut tty, speed);
        libc::cfsetispeed(&mut tty, speed);
        libc::cfmakeraw(&mut tty);
        tty.c_cflag |= libc::CLOCAL | libc::CREAD;
        tty.c_cflag &= !(libc::CSTOPB | libc::CRTSCTS | libc::CSIZE);
        tty.c_cflag |= libc::CS8;
        tty.c_cc[libc::VMIN] = 0;
        tty.c_cc[libc::VTIME] = 1;
        libc::tcflush(fd.as_raw_fd(), libc::TCIOFLUSH);
        if libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, &tty) != 0 {
            return Err(McubError::Io(std::io::Error::last_os_error()));
        }
    }
    Ok(fd)
}

fn readline(fd: RawFd, timeout: Duration) -> Option<String> {
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let deadline = std::time::Instant::now() + timeout;
    let mut chunk = [0u8; 256];

    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.truncate(pos);
            break;
        }
        let remaining = match deadline.checked_duration_since(std::time::Instant::now()) {
            Some(r) => r,
            None => break,
        };
        let ms = (remaining.as_millis() as u16).max(1);
        let mut pfds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut pfds, PollTimeout::from(ms)) {
            Ok(n) if n > 0 => {}
            _ => break,
        }
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
        if n <= 0 { break; }
        for &b in &chunk[..n as usize] {
            if b == b'\r' { continue; }
            buf.push(b);
        }
        if buf.len() > 4096 { break; }
    }

    if buf.is_empty() {
        None
    } else {
        String::from_utf8(buf).ok()
    }
}
