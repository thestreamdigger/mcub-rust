use std::sync::Arc;
use std::time::Instant;

use crate::core::logger::Logger;
use crate::log_info;

const BACKOFF_MULTIPLIER: f64 = 2.0;
const DEFAULT_MAX_DELAY: f64 = 60.0;
const QUIET_THRESHOLD: u32 = 5;

pub struct Reconnection {
    logger: Arc<Logger>,
    max_attempts: u32,
    attempts: u32,
    base_delay: f64,
    current_delay: f64,
    max_delay: f64,
    last_attempt: Option<Instant>,
}

impl Reconnection {
    pub fn new(logger: Arc<Logger>, max_attempts: u32, delay: f64) -> Self {
        Self {
            logger,
            max_attempts,
            attempts: 0,
            base_delay: delay,
            current_delay: delay,
            max_delay: DEFAULT_MAX_DELAY,
            last_attempt: None,
        }
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub fn should_attempt(&mut self, resource_name: &str) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_attempt {
            if now.duration_since(last).as_secs_f64() < self.current_delay {
                return false;
            }
        }

        self.attempts += 1;
        self.last_attempt = Some(now);

        if self.attempts <= QUIET_THRESHOLD {
            log_info!(self.logger, "{}: retry {}", resource_name, self.attempts);
        } else if self.attempts == QUIET_THRESHOLD + 1 {
            log_info!(
                self.logger,
                "{}: retry suppressed (interval={:.0}s)",
                resource_name,
                self.current_delay
            );
        }

        self.current_delay = (self.current_delay * BACKOFF_MULTIPLIER).min(self.max_delay);
        true
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
        self.current_delay = self.base_delay;
        self.last_attempt = None;
    }
}
