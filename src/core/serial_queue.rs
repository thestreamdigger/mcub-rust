use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::os::fd::RawFd;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::core::logger::Logger;
use crate::{log_debug, log_error, log_warning};

#[derive(Debug, Eq, PartialEq)]
struct QueueItem {
    priority: i32,
    sequence: u64,
    data: Vec<u8>,
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is max-heap; reverse so lower priority pops first.
        // Tie-break by sequence (older first), also reversed.
        match other.priority.cmp(&self.priority) {
            Ordering::Equal => other.sequence.cmp(&self.sequence),
            ord => ord,
        }
    }
}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct State {
    heap: BinaryHeap<QueueItem>,
    sequence_counter: u64,
    running: bool,
    device_disconnected: bool,
    stats_peak_depth: usize,
}

#[derive(Default)]
struct ErrorState {
    consecutive_errors: u32,
    stats_write_total: f64,
    stats_write_max: f64,
    stats_writes: usize,
}

#[derive(Default)]
pub struct QueueStats {
    pub peak_depth: usize,
    pub write_avg_ms: f64,
    pub write_max_ms: f64,
    pub writes: usize,
}

struct Inner {
    state: Mutex<State>,
    cond: Condvar,
    error_state: Mutex<ErrorState>,
    serial_fd: RawFd,
    logger: Arc<Logger>,
    bridge_name: String,
    max_queue_size: usize,
    max_consecutive_errors: u32,
}

pub struct SerialQueue {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SerialQueue {
    pub fn new(
        serial_fd: RawFd,
        logger: Arc<Logger>,
        bridge_name: &str,
        _queue_timeout: f64,
        max_queue_size: usize,
    ) -> Self {
        log_debug!(logger, "Queue init: {bridge_name}");
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    heap: BinaryHeap::with_capacity(64),
                    sequence_counter: 0,
                    running: false,
                    device_disconnected: false,
                    stats_peak_depth: 0,
                }),
                cond: Condvar::new(),
                error_state: Mutex::new(ErrorState::default()),
                serial_fd,
                logger,
                bridge_name: bridge_name.to_string(),
                max_queue_size,
                max_consecutive_errors: 3,
            }),
            worker: Mutex::new(None),
        }
    }

    pub fn start(&self) -> bool {
        let mut state = self.inner.state.lock().unwrap();
        if state.running {
            log_warning!(self.inner.logger, "Queue already running");
            return true;
        }
        state.running = true;
        state.device_disconnected = false;
        drop(state);

        let inner = Arc::clone(&self.inner);
        let handle = thread::spawn(move || worker_loop(inner));
        *self.worker.lock().unwrap() = Some(handle);
        log_debug!(self.inner.logger, "Queue started: {}", self.inner.bridge_name);
        true
    }

    pub fn stop(&self) {
        {
            let mut state = self.inner.state.lock().unwrap();
            if !state.running {
                return;
            }
            state.running = false;
            state.device_disconnected = true;
        }
        self.inner.cond.notify_all();

        let handle = self.worker.lock().unwrap().take();
        if let Some(handle) = handle {
            // Best-effort timed join via a watcher pattern. std::thread has no timed_join;
            // we trust the worker to observe running=false and exit promptly.
            let _ = handle.join();
        }
        log_debug!(self.inner.logger, "Queue stopped: {}", self.inner.bridge_name);
    }

    pub fn send(&self, data: &[u8], priority: i32) -> bool {
        let state_lock = self.inner.state.lock().unwrap();
        if !state_lock.running {
            log_warning!(self.inner.logger, "Queue not running");
            return false;
        }
        drop(state_lock);

        // Critical priority bypasses queue (synchronous write)
        if priority < 0 {
            return write_to_device(&self.inner, data);
        }

        let mut state = self.inner.state.lock().unwrap();
        if state.heap.len() >= self.inner.max_queue_size {
            log_warning!(self.inner.logger, "Queue full, drop");
            return false;
        }
        state.sequence_counter += 1;
        let seq = state.sequence_counter;
        state.heap.push(QueueItem {
            priority,
            sequence: seq,
            data: data.to_vec(),
        });
        let depth = state.heap.len();
        if depth > state.stats_peak_depth {
            state.stats_peak_depth = depth;
        }
        drop(state);

        self.inner.cond.notify_one();
        true
    }

    pub fn send_json(&self, json_str: &str, priority: i32) -> bool {
        let mut buf = Vec::with_capacity(json_str.len() + 1);
        buf.extend_from_slice(json_str.as_bytes());
        buf.push(b'\n');
        self.send(&buf, priority)
    }

    pub fn send_binary_spectrum(
        &self,
        raw_data: &[u8],
        header_byte: u8,
        sync_byte: u8,
        priority: i32,
    ) -> bool {
        let mut frame = Vec::with_capacity(2 + raw_data.len());
        frame.push(sync_byte);
        frame.push(header_byte);
        frame.extend_from_slice(raw_data);
        self.send(&frame, priority)
    }

    pub fn stats(&self) -> QueueStats {
        let mut err = self.inner.error_state.lock().unwrap();
        let mut state = self.inner.state.lock().unwrap();
        let stats = QueueStats {
            peak_depth: state.stats_peak_depth,
            write_avg_ms: if err.stats_writes > 0 {
                (err.stats_write_total / err.stats_writes as f64) * 1000.0
            } else {
                0.0
            },
            write_max_ms: err.stats_write_max * 1000.0,
            writes: err.stats_writes,
        };
        state.stats_peak_depth = 0;
        err.stats_write_total = 0.0;
        err.stats_write_max = 0.0;
        err.stats_writes = 0;
        stats
    }
}

impl Drop for SerialQueue {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker_loop(inner: Arc<Inner>) {
    log_debug!(inner.logger, "Queue loop: {}", inner.bridge_name);

    loop {
        let item = {
            let mut state = inner.state.lock().unwrap();
            while state.running && !state.device_disconnected && state.heap.is_empty() {
                let (s, _) = inner
                    .cond
                    .wait_timeout(state, Duration::from_millis(10))
                    .unwrap();
                state = s;
            }
            if !state.running || state.device_disconnected {
                break;
            }
            state.heap.pop()
        };

        if let Some(item) = item {
            write_to_device(&inner, &item.data);
        }
    }

    log_debug!(inner.logger, "Queue loop ended: {}", inner.bridge_name);
}

fn write_to_device(inner: &Inner, data: &[u8]) -> bool {
    {
        let state = inner.state.lock().unwrap();
        if state.device_disconnected || !state.running {
            return false;
        }
    }
    if inner.serial_fd < 0 {
        return false;
    }

    let t0 = Instant::now();
    let n = unsafe { libc::write(inner.serial_fd, data.as_ptr() as *const _, data.len()) };
    let dt = t0.elapsed().as_secs_f64();

    if n < 0 {
        let mut err = inner.error_state.lock().unwrap();
        err.consecutive_errors += 1;
        let disconnected = err.consecutive_errors >= inner.max_consecutive_errors;
        drop(err);

        if disconnected {
            let mut state = inner.state.lock().unwrap();
            if !state.device_disconnected {
                log_error!(inner.logger, "Device disconnected");
                state.device_disconnected = true;
            }
        }
        return false;
    }

    let mut err = inner.error_state.lock().unwrap();
    err.consecutive_errors = 0;
    err.stats_write_total += dt;
    if dt > err.stats_write_max {
        err.stats_write_max = dt;
    }
    err.stats_writes += 1;
    true
}
