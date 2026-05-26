// Minimal MPD client. Replaces the `mpd = "0.1"` crate which has 220x worse
// latency than libmpdclient due to no TCP_NODELAY + per-line allocations.
// Protocol reference: https://mpd.readthedocs.io/en/latest/protocol.html

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::error::{McubError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Play,
    Pause,
    Stop,
}

impl State {
    fn from_str(s: &str) -> Self {
        match s {
            "play" => Self::Play,
            "pause" => Self::Pause,
            _ => Self::Stop,
        }
    }
}

#[derive(Debug, Default)]
pub struct Status {
    pub state: State,
    pub volume: i32,
    pub elapsed_ms: u64,
    pub duration_s: u32,
    pub song_pos: Option<i32>,
    pub song_id: Option<i32>,
    pub queue_len: i32,
    pub repeat: bool,
    pub random: bool,
    pub single: bool,
    pub consume: bool,
}

impl Default for State {
    fn default() -> Self { Self::Stop }
}

#[derive(Debug, Default)]
pub struct Song {
    pub file: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub date: Option<String>,
    pub track: Option<String>,
    pub duration_s: Option<u32>,
}

pub struct MpdClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl MpdClient {
    pub fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{host}:{port}");
        let socket = TcpStream::connect(&addr)?;
        socket.set_nodelay(true)?; // critical: kills Nagle (40ms+ latency on small commands)
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        socket.set_write_timeout(Some(Duration::from_secs(3)))?;

        let writer = socket.try_clone()?;
        let mut reader = BufReader::new(socket);

        let mut banner = String::new();
        reader.read_line(&mut banner)?;
        if !banner.starts_with("OK MPD") {
            return Err(McubError::Mpd(format!("bad banner: {}", banner.trim())));
        }

        Ok(Self { writer, reader })
    }

    fn send(&mut self, cmd: &str) -> Result<()> {
        self.writer.write_all(cmd.as_bytes())?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    fn read_pairs(&mut self) -> Result<Vec<(String, String)>> {
        let mut pairs = Vec::with_capacity(24);
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Err(McubError::Mpd("connection closed".into()));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed == "OK" {
                return Ok(pairs);
            }
            if let Some(rest) = trimmed.strip_prefix("ACK ") {
                return Err(McubError::Mpd(rest.to_string()));
            }
            if let Some((k, v)) = trimmed.split_once(": ") {
                pairs.push((k.to_string(), v.to_string()));
            }
        }
    }

    fn run(&mut self, cmd: &str) -> Result<Vec<(String, String)>> {
        self.send(cmd)?;
        self.read_pairs()
    }

    fn exec(&mut self, cmd: &str) -> Result<()> {
        self.run(cmd).map(|_| ())
    }

    pub fn status(&mut self) -> Result<Status> {
        let pairs = self.run("status")?;
        let mut s = Status::default();
        for (k, v) in pairs {
            match k.as_str() {
                "state" => s.state = State::from_str(&v),
                "volume" => s.volume = v.parse().unwrap_or(0),
                "elapsed" => {
                    s.elapsed_ms = (v.parse::<f64>().unwrap_or(0.0) * 1000.0) as u64;
                }
                "duration" => {
                    s.duration_s = v.parse::<f64>().unwrap_or(0.0) as u32;
                }
                "time" => {
                    // fallback: "elapsed:total" if duration absent
                    if s.duration_s == 0 {
                        if let Some((_, t)) = v.split_once(':') {
                            s.duration_s = t.parse().unwrap_or(0);
                        }
                    }
                }
                "song" => s.song_pos = v.parse().ok(),
                "songid" => s.song_id = v.parse().ok(),
                "playlistlength" => s.queue_len = v.parse().unwrap_or(0),
                "repeat" => s.repeat = v == "1",
                "random" => s.random = v == "1",
                "single" => s.single = v == "1",
                "consume" => s.consume = v == "1",
                _ => {}
            }
        }
        Ok(s)
    }

    pub fn currentsong(&mut self) -> Result<Option<Song>> {
        let pairs = self.run("currentsong")?;
        if pairs.is_empty() {
            return Ok(None);
        }
        Ok(Some(song_from_pairs(&pairs)))
    }

    pub fn playlistinfo(&mut self) -> Result<Vec<Song>> {
        let pairs = self.run("playlistinfo")?;
        let mut songs = Vec::new();
        let mut current: HashMap<String, String> = HashMap::new();
        for (k, v) in pairs {
            if k == "file" && !current.is_empty() {
                songs.push(song_from_map(&current));
                current.clear();
            }
            current.insert(k, v);
        }
        if !current.is_empty() {
            songs.push(song_from_map(&current));
        }
        Ok(songs)
    }

    pub fn play(&mut self) -> Result<()> { self.exec("play") }
    pub fn stop(&mut self) -> Result<()> { self.exec("stop") }
    pub fn next(&mut self) -> Result<()> { self.exec("next") }
    pub fn previous(&mut self) -> Result<()> { self.exec("previous") }
    pub fn toggle_pause(&mut self) -> Result<()> { self.exec("pause") }
    pub fn play_pos(&mut self, pos: u32) -> Result<()> {
        self.exec(&format!("play {pos}"))
    }
    pub fn seek_current(&mut self, seconds: f32) -> Result<()> {
        self.exec(&format!("seekcur {seconds}"))
    }
    pub fn set_volume(&mut self, vol: i32) -> Result<()> {
        let v = vol.clamp(0, 100);
        self.exec(&format!("setvol {v}"))
    }
    pub fn set_repeat(&mut self, on: bool) -> Result<()> {
        self.exec(&format!("repeat {}", if on { 1 } else { 0 }))
    }
    pub fn set_random(&mut self, on: bool) -> Result<()> {
        self.exec(&format!("random {}", if on { 1 } else { 0 }))
    }
    pub fn set_single(&mut self, on: bool) -> Result<()> {
        self.exec(&format!("single {}", if on { 1 } else { 0 }))
    }
    pub fn set_consume(&mut self, on: bool) -> Result<()> {
        self.exec(&format!("consume {}", if on { 1 } else { 0 }))
    }
}

fn song_from_pairs(pairs: &[(String, String)]) -> Song {
    let mut s = Song::default();
    for (k, v) in pairs {
        apply_song_field(&mut s, k, v);
    }
    s
}

fn song_from_map(map: &HashMap<String, String>) -> Song {
    let mut s = Song::default();
    for (k, v) in map {
        apply_song_field(&mut s, k, v);
    }
    s
}

fn apply_song_field(s: &mut Song, k: &str, v: &str) {
    match k {
        "file" => s.file = v.to_string(),
        "Title" => s.title = Some(v.to_string()),
        "Artist" => s.artist = Some(v.to_string()),
        "Album" => s.album = Some(v.to_string()),
        "Genre" => s.genre = Some(v.to_string()),
        "Date" => s.date = Some(v.to_string()),
        "Track" => s.track = Some(v.to_string()),
        "duration" => {
            s.duration_s = v.parse::<f64>().ok().map(|d| d as u32);
        }
        "Time" => {
            if s.duration_s.is_none() {
                s.duration_s = v.parse().ok();
            }
        }
        _ => {}
    }
}
