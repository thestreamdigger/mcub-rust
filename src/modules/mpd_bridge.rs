use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::core::action_runner;
use crate::core::config_manager::Config;
use crate::core::logger::Logger;
use crate::core::mpd_client::{MpdClient, State as MpdState};
use crate::core::reconnection::Reconnection;
use crate::core::serial_comm::SerialComm;
use crate::core::signal_handler;
use crate::error::Result as McubResult;
use crate::{log_debug, log_error, log_info, log_ok, log_warning};

pub struct MpdBridge {
    serial: Arc<SerialComm>,
    mpd: Mutex<Option<MpdClient>>,
    reconnection: Mutex<Reconnection>,
    config: Arc<Config>,
    logger: Arc<Logger>,
    last_update: Mutex<Option<Instant>>,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    playlist_time: u32,
    playlist_length: i32,
    elapsed_ms: u64,
    state: Option<MpdState>,
    last_stop: Option<Instant>,
    stop_count: u32,
    mpd_connected: bool,
    mpd_error_logged: bool,
}

#[derive(Serialize)]
struct StateEnvelope<'a> {
    t: &'a str,
    d: StateData,
}

#[derive(Serialize, Default)]
struct StateData {
    state: String,
    volume: String,
    elapsed: String,
    total: String,
    song_id: String,
    track_number: String,
    playlist_position: String,
    playlist_length: String,
    playlist_total_time: String,
    title: String,
    artist: String,
    album: String,
    genre: String,
    year: String,
    file_type: String,
    repeat: String,
    random: String,
    single: String,
    consume: String,
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

impl MpdBridge {
    pub fn new(config: Arc<Config>, logger: Arc<Logger>) -> Self {
        let serial = Arc::new(SerialComm::new(Arc::clone(&config), Arc::clone(&logger), "MPD"));
        let reconnection = Reconnection::new(
            Arc::clone(&logger),
            config.mpd.reconnect.max_attempts,
            config.mpd.reconnect.delay,
        );
        action_runner::init(config.actions.clone(), Arc::clone(&logger));

        Self {
            serial,
            mpd: Mutex::new(None),
            reconnection: Mutex::new(reconnection),
            config,
            logger,
            last_update: Mutex::new(None),
            cache: Mutex::new(Cache { playlist_length: -1, ..Cache::default() }),
        }
    }

    pub fn connect(&self) -> bool {
        let mut guard = self.mpd.lock().unwrap();
        *guard = None;
        match MpdClient::connect(&self.config.mpd.host, self.config.mpd.port) {
            Ok(client) => {
                *guard = Some(client);
                let mut cache = self.cache.lock().unwrap();
                cache.mpd_connected = true;
                cache.mpd_error_logged = false;
                log_ok!(self.logger, "MPD: {}:{}", self.config.mpd.host, self.config.mpd.port);
                true
            }
            Err(e) => {
                log_error!(self.logger, "MPD failed: {}", e);
                false
            }
        }
    }

    pub fn disconnect(&self) {
        let mut guard = self.mpd.lock().unwrap();
        if guard.take().is_some() {
            log_info!(self.logger, "MPD closed");
        }
    }

    pub fn attempt_reconnection(&self) -> bool {
        if !self.reconnection.lock().unwrap().should_attempt("MPD") {
            return false;
        }
        if self.connect() {
            log_ok!(self.logger, "MPD reconnected");
            self.reconnection.lock().unwrap().reset();
            true
        } else {
            log_warning!(self.logger, "MPD reconnect failed");
            false
        }
    }

    pub fn run(&self) -> i32 {
        if self.serial.connect().is_err() {
            log_error!(self.logger, "Device failed");
            return 1;
        }
        if !self.connect() {
            log_error!(self.logger, "MPD failed");
            return 1;
        }
        if self.serial.identify_device().is_none() {
            log_warning!(self.logger, "Device identify failed");
        }

        log_info!(self.logger, "Main loop");

        while !signal_handler::received() {
            self.send_status();
            self.process_device_commands();
            // state-JSON path flips mpd_connected on error; no per-iteration
            // status probe (was ~100 req/s against MPD)
            if !self.is_connected() {
                self.attempt_reconnection();
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        self.cleanup();
        0
    }

    fn send_status(&self) -> bool {
        let now = Instant::now();
        {
            let mut last = self.last_update.lock().unwrap();
            if let Some(last_t) = *last {
                if now.duration_since(last_t).as_secs_f64() < self.config.mpd.update_interval {
                    return true;
                }
            }
            *last = Some(now);
        }

        let Some(json) = self.build_state_json() else { return false; };
        let priority = self.serial.get_priority("high");
        self.serial.send_message(&json, priority)
    }

    pub fn is_connected(&self) -> bool {
        self.cache.lock().unwrap().mpd_connected
    }

    pub fn build_state_json(&self) -> Option<String> {
        let mut guard = self.mpd.lock().unwrap();
        let client = guard.as_mut()?;

        let status = match client.status() {
            Ok(s) => s,
            Err(e) => {
                let mut cache = self.cache.lock().unwrap();
                if cache.mpd_connected && !cache.mpd_error_logged {
                    log_error!(self.logger, "MPD lost: {}", e);
                    cache.mpd_error_logged = true;
                }
                cache.mpd_connected = false;
                return None;
            }
        };

        let state_str = match status.state {
            MpdState::Play => "P",
            MpdState::Pause => "U",
            MpdState::Stop => "S",
        };

        {
            let mut cache = self.cache.lock().unwrap();
            cache.elapsed_ms = status.elapsed_ms;
            cache.state = Some(status.state);
        }

        let elapsed = (status.elapsed_ms / 1000) as u32;
        let total = status.duration_s;
        let pl_length = status.queue_len;
        let song_pos = status.song_pos.unwrap_or(-1);
        let song_id = status.song_id.unwrap_or(-1);

        let playlist_total = self.calculate_playlist_time(client, pl_length);

        let mut data = StateData {
            state: state_str.into(),
            volume: status.volume.to_string(),
            elapsed: format_mmss(elapsed),
            total: format_mmss(total),
            song_id: if song_id >= 0 { song_id.to_string() } else { "0".into() },
            track_number: "0".into(),
            playlist_position: ((song_pos + 1).max(0)).to_string(),
            playlist_length: pl_length.to_string(),
            playlist_total_time: format_hhmmss(playlist_total),
            title: "Unknown".into(),
            artist: "Unknown".into(),
            album: "Unknown".into(),
            genre: "Unknown".into(),
            year: "Unknown".into(),
            file_type: "Unknown".into(),
            repeat: bool_str(status.repeat),
            random: bool_str(status.random),
            single: bool_str(status.single),
            consume: bool_str(status.consume),
        };

        if status.state != MpdState::Stop {
            if let Ok(Some(song)) = client.currentsong() {
                if let Some(t) = &song.title { data.title = t.clone(); }
                if let Some(a) = &song.artist { data.artist = a.clone(); }
                if let Some(al) = &song.album { data.album = al.clone(); }
                if let Some(g) = &song.genre { data.genre = g.clone(); }
                if let Some(d) = &song.date { data.year = d.clone(); }
                if let Some(tr) = &song.track { data.track_number = clean_track(tr); }

                if let Some(dot) = song.file.rfind('.') {
                    let ext = &song.file[dot + 1..];
                    if !ext.is_empty() {
                        data.file_type = ext.to_ascii_uppercase();
                    }
                }
                if data.title == "Unknown" {
                    let fname = song.file.rsplit('/').next().unwrap_or(&song.file);
                    let stem = match fname.rfind('.') {
                        Some(i) => &fname[..i],
                        None => fname,
                    };
                    data.title = stem.into();
                }
            }
        }

        serde_json::to_string(&StateEnvelope { t: "m", d: data }).ok()
    }

    fn calculate_playlist_time(&self, client: &mut MpdClient, playlist_length: i32) -> u32 {
        {
            let cache = self.cache.lock().unwrap();
            if cache.playlist_length == playlist_length && playlist_length > 0 {
                return cache.playlist_time;
            }
        }

        let Ok(songs) = client.playlistinfo() else {
            let cache = self.cache.lock().unwrap();
            return cache.playlist_time;
        };
        let total: u32 = songs.iter().filter_map(|s| s.duration_s).sum();

        let mut cache = self.cache.lock().unwrap();
        cache.playlist_time = total;
        cache.playlist_length = playlist_length;
        log_debug!(self.logger, "Playlist: {}s, {} songs", total, playlist_length);
        total
    }

    fn process_device_commands(&self) {
        let Some(line) = self.serial.read_message(Duration::ZERO) else { return; };
        let Ok(env) = serde_json::from_str::<CmdEnvelope>(&line) else { return; };
        if env.t != "cmd" { return; }
        let Some(content) = env.c else { return; };

        let action = content.action.as_str();
        let params = content.parameters.as_ref();
        self.handle_command(action, params);
    }

    pub fn handle_command(&self, action: &str, params: Option<&serde_json::Value>) {
        if action == "exec" {
            if let Some(p) = params {
                if let Some(name) = p.get("name").and_then(|v| v.as_str()) {
                    action_runner::dispatch(name);
                }
            }
            return;
        }

        let mut guard = self.mpd.lock().unwrap();
        let Some(client) = guard.as_mut() else {
            log_error!(self.logger, "MPD not connected");
            return;
        };
        log_debug!(self.logger, "Cmd: {}", action);

        let result: McubResult<()> = match action {
            "play_pause" => match client.status() {
                Ok(st) if st.state == MpdState::Stop => client.play(),
                Ok(st) => client.pause(st.state == MpdState::Play),
                Err(e) => Err(e),
            },
            "next" => client.next(),
            "previous" => {
                let cache = self.cache.lock().unwrap();
                let elapsed_ms = cache.elapsed_ms;
                let stopped = cache.state == Some(MpdState::Stop);
                drop(cache);
                if !stopped && elapsed_ms > 2000 {
                    client.seek_current(0.0)
                } else {
                    client.previous()
                }
            }
            "stop" => self.handle_stop(client),
            "volume_up" => self.adjust_volume(client, 10),
            "volume_down" => self.adjust_volume(client, -10),
            "set_volume" => {
                if let Some(p) = params {
                    self.handle_set_volume(client, p)
                } else {
                    Ok(())
                }
            }
            "repeat" => self.toggle_flag(client, action),
            "single" => self.toggle_flag(client, action),
            "consume" => self.toggle_flag(client, action),
            "random" => self.toggle_flag(client, action),
            other => {
                log_warning!(self.logger, "Unknown cmd: {}", other);
                Ok(())
            }
        };

        if let Err(e) = result {
            log_error!(self.logger, "Cmd '{}' err: {}", action, e);
        }
    }

    fn handle_stop(&self, client: &mut MpdClient) -> McubResult<()> {
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap();
        let recent = cache
            .last_stop
            .map(|t| now.duration_since(t) <= Duration::from_secs(3))
            .unwrap_or(false);
        if cache.stop_count >= 1 && recent {
            cache.stop_count = 0;
            drop(cache);
            client.play_pos(0)?;
            client.stop()
        } else {
            cache.stop_count = 1;
            cache.last_stop = Some(now);
            drop(cache);
            client.stop()
        }
    }

    fn adjust_volume(&self, client: &mut MpdClient, delta: i32) -> McubResult<()> {
        let st = client.status()?;
        let cur = st.volume;
        let new = (cur + delta).clamp(0, 100);
        log_debug!(self.logger, "Vol: {}->{}", cur, new);
        client.set_volume(new)
    }

    fn handle_set_volume(&self, client: &mut MpdClient, params: &serde_json::Value) -> McubResult<()> {
        let Some(vol_item) = params.get("volume") else { return Ok(()); };
        let vol: i32 = match vol_item {
            serde_json::Value::String(s) => s.parse().unwrap_or(0),
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) as i32,
            _ => return Ok(()),
        };
        if vol < 0 {
            let st = client.status()?;
            let new = (st.volume + vol).max(0);
            client.set_volume(new)
        } else {
            client.set_volume(vol.min(100))
        }
    }

    fn toggle_flag(&self, client: &mut MpdClient, action: &str) -> McubResult<()> {
        let st = client.status()?;
        let cur = match action {
            "repeat" => st.repeat,
            "random" => st.random,
            "single" => st.single,
            "consume" => st.consume,
            _ => return Ok(()),
        };
        let new = !cur;
        match action {
            "repeat" => client.set_repeat(new),
            "random" => client.set_random(new),
            "single" => client.set_single(new),
            "consume" => client.set_consume(new),
            _ => Ok(()),
        }
    }

    pub fn cleanup(&self) {
        log_info!(self.logger, "Cleanup");
        self.serial.close();
        self.disconnect();
    }
}

impl Drop for MpdBridge {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn format_mmss(seconds: u32) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn format_hhmmss(seconds: u32) -> String {
    format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60, seconds % 60)
}

fn bool_str(b: bool) -> String {
    if b { "1".into() } else { "0".into() }
}

fn clean_track(s: &str) -> String {
    let primary = s.split('/').next().unwrap_or(s);
    let digits: String = primary.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { "0".into() } else { digits }
}
