use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::fcntl::{open, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::stat::Mode;
use nix::sys::termios::{
    self, BaudRate, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices,
};
use serde::Deserialize;

use crate::error::{McubError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Mpd,
    Cava,
    Hybrid,
    Sysinfo,
}

impl DeviceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mpd => "mpd",
            Self::Cava => "cava",
            Self::Hybrid => "hybrid",
            Self::Sysinfo => "sysinfo",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceFormat {
    Json,
    Binary,
}

impl DeviceFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Binary => "binary",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceId {
    pub device_type: DeviceType,
    pub format: DeviceFormat,
    pub spectrum_bars: Option<u32>,
    pub has_sysinfo: bool,
}

#[derive(Debug, Deserialize)]
struct IdEnvelope {
    t: String,
    d: IdData,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct IdData {
    modes: Vec<String>,
    binary: bool,
    bars: Option<u32>,
    name: Option<String>,
}

pub fn identify<F>(device_path: &str, baudrate: u32, timeout: Duration, mut debug: F) -> Result<DeviceId>
where
    F: FnMut(&str),
{
    let fd = match open_serial(device_path, baudrate, timeout) {
        Ok(fd) => fd,
        Err(e) => {
            debug(&format!("{device_path}: serial error: {e}"));
            return Err(e);
        }
    };

    toggle_dtr(&fd);
    std::thread::sleep(Duration::from_secs(2));
    drain(&fd);

    let cmd = b"{\"t\":\"id\",\"c\":\"identify\"}\n";
    if let Err(e) = write_all(&fd, cmd) {
        debug(&format!("{device_path}: write error: {e}"));
        return Err(e);
    }

    let response = match readline(&fd, timeout) {
        Some(s) if !s.is_empty() => s,
        _ => {
            debug(&format!("{device_path}: no response (timeout)"));
            return Err(McubError::Timeout);
        }
    };
    debug(&format!("{device_path}: response={}", truncate(&response, 200)));

    let env: IdEnvelope = serde_json::from_str(&response).map_err(|_| {
        debug(&format!("{device_path}: JSON decode error"));
        McubError::Config("invalid id JSON".into())
    })?;

    if env.t != "id" {
        debug(&format!("{device_path}: invalid response format"));
        return Err(McubError::Config("invalid id envelope".into()));
    }

    let format = if env.d.binary { DeviceFormat::Binary } else { DeviceFormat::Json };
    let spectrum_bars = env.d.bars;

    let mut has_mpd = false;
    let mut has_cava = false;
    let mut has_sysinfo = false;

    if !env.d.modes.is_empty() {
        for m in &env.d.modes {
            match m.as_str() {
                "mpd" => has_mpd = true,
                "cava" => has_cava = true,
                "sysinfo" => has_sysinfo = true,
                _ => {}
            }
        }
    } else if let Some(name) = &env.d.name {
        let lower = name.to_ascii_lowercase();
        if lower.contains("cava") { has_cava = true; }
        if lower.contains("mpd") { has_mpd = true; }
        if lower.contains("sysinfo") { has_sysinfo = true; }
    }

    let device_type = match (has_mpd, has_cava, has_sysinfo) {
        (true, true, _) => DeviceType::Hybrid,
        (true, false, true) | (false, true, true) => DeviceType::Hybrid,
        (true, false, false) => DeviceType::Mpd,
        (false, true, false) => DeviceType::Cava,
        (false, false, true) => DeviceType::Sysinfo,
        _ => return Err(McubError::DeviceNotFound),
    };

    let modes_str = [
        has_mpd.then_some("mpd"),
        has_cava.then_some("cava"),
        has_sysinfo.then_some("sysinfo"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(",");
    debug(&format!(
        "{device_path}: modes=[{modes_str}], format={}, bars={}",
        format.as_str(),
        spectrum_bars.unwrap_or(0)
    ));

    Ok(DeviceId { device_type, format, spectrum_bars, has_sysinfo })
}

fn open_serial(path: &str, baudrate: u32, timeout: Duration) -> Result<OwnedFd> {
    let raw = open(path, OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK, Mode::empty())?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    let flags = nix::fcntl::fcntl(fd.as_raw_fd(), FcntlArg::F_GETFL)?;
    let new_flags = OFlag::from_bits_truncate(flags) & !OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(fd.as_raw_fd(), FcntlArg::F_SETFL(new_flags))?;

    let mut tty = termios::tcgetattr(fd.as_fd())?;
    let speed = match baudrate {
        9600 => BaudRate::B9600,
        _ => BaudRate::B115200,
    };
    termios::cfsetospeed(&mut tty, speed)?;
    termios::cfsetispeed(&mut tty, speed)?;
    termios::cfmakeraw(&mut tty);
    tty.control_flags |= ControlFlags::CLOCAL | ControlFlags::CREAD;
    tty.control_flags &= !(ControlFlags::CSTOPB | ControlFlags::CRTSCTS | ControlFlags::CSIZE);
    tty.control_flags |= ControlFlags::CS8;
    tty.input_flags &= !(InputFlags::IXON | InputFlags::IXOFF | InputFlags::IXANY);
    tty.output_flags &= !OutputFlags::OPOST;
    tty.local_flags &= !(LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ICANON | LocalFlags::ISIG);

    let vtime = ((timeout.as_secs_f64() * 10.0) as u8).clamp(1, 255);
    tty.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    tty.control_chars[SpecialCharacterIndices::VTIME as usize] = vtime;

    termios::tcflush(fd.as_fd(), termios::FlushArg::TCIOFLUSH)?;
    termios::tcsetattr(fd.as_fd(), SetArg::TCSANOW, &tty)?;
    Ok(fd)
}

fn toggle_dtr(fd: &OwnedFd) {
    let dtr = libc::TIOCM_DTR;
    unsafe {
        libc::ioctl(fd.as_raw_fd(), libc::TIOCMBIC, &dtr);
    }
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        libc::ioctl(fd.as_raw_fd(), libc::TIOCMBIS, &dtr);
    }
}

fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 256];
    loop {
        let n = unsafe {
            libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len())
        };
        if n <= 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn write_all(fd: &OwnedFd, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let n = unsafe {
            libc::write(fd.as_raw_fd(), data.as_ptr() as *const _, data.len())
        };
        if n <= 0 {
            return Err(McubError::Io(std::io::Error::last_os_error()));
        }
        data = &data[n as usize..];
    }
    Ok(())
}

fn readline(fd: &OwnedFd, timeout: Duration) -> Option<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let deadline = Instant::now() + timeout;
    let mut chunk = [0u8; 256];
    let borrowed = fd.as_fd();

    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.truncate(pos);
            break;
        }
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(r) => r,
            None => break,
        };
        let ms: u16 = (remaining.as_millis() as u16).max(100);
        let mut pfds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut pfds, PollTimeout::from(ms)) {
            Ok(n) if n > 0 => {}
            _ => break,
        }
        let n = unsafe {
            libc::read(fd.as_raw_fd(), chunk.as_mut_ptr() as *mut _, chunk.len())
        };
        if n <= 0 {
            break;
        }
        for &b in &chunk[..n as usize] {
            if b == b'\r' { continue; }
            buf.push(b);
        }
        if buf.len() > 4096 { break; }
    }

    buf.retain(|&b| b != b'\r');
    if buf.is_empty() {
        None
    } else {
        String::from_utf8(buf).ok()
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let end = s.char_indices().take_while(|(i, _)| *i < max).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
    &s[..end]
}
