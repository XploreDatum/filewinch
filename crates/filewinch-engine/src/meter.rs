//! Per-job progress counters, throttled sampling and smoothed speed.

use crate::Progress;
use filewinch_core::RPath;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Minimum gap between progress events for one job (about ten a second).
const EMIT_EVERY: Duration = Duration::from_millis(100);
/// Time constant of the speed's exponential smoothing.
const SMOOTHING: f64 = 2.0;

#[derive(Default)]
pub(crate) struct Meter {
    m: Mutex<MeterState>,
}

#[derive(Default)]
struct MeterState {
    bytes_done: u64,
    files_done: u64,
    bytes_total: u64,
    files_total: u64,
    scanned: bool,
    current: Option<RPath>,
    speed: f64,
    last_sample: Option<(Instant, u64)>,
    last_emit: Option<Instant>,
}

impl Meter {
    fn lock(&self) -> std::sync::MutexGuard<'_, MeterState> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_total(&self, files: u64, bytes: u64) {
        let mut s = self.lock();
        s.files_total += files;
        s.bytes_total += bytes;
    }

    pub fn scan_done(&self) {
        self.lock().scanned = true;
    }

    pub fn set_current(&self, p: &RPath) {
        self.lock().current = Some(p.clone());
    }

    pub fn add_bytes(&self, n: u64) {
        self.lock().bytes_done += n;
    }

    /// Takes back bytes counted for an attempt that has to restart.
    pub fn sub_bytes(&self, n: u64) {
        let mut s = self.lock();
        s.bytes_done = s.bytes_done.saturating_sub(n);
    }

    pub fn file_done(&self) {
        self.lock().files_done += 1;
    }

    pub fn progress(&self) -> Progress {
        Self::build(&self.lock())
    }

    fn build(s: &MeterState) -> Progress {
        let bytes_total = s.scanned.then_some(s.bytes_total.max(s.bytes_done));
        let eta = match bytes_total {
            Some(t) if s.speed > 1.0 => {
                Some(Duration::from_secs_f64((t - s.bytes_done) as f64 / s.speed))
            }
            _ => None,
        };
        Progress {
            bytes_done: s.bytes_done,
            bytes_total,
            files_done: s.files_done,
            files_total: s.scanned.then_some(s.files_total.max(s.files_done)),
            current: s.current.clone(),
            bytes_per_sec: s.speed,
            eta,
        }
    }

    fn sample(s: &mut MeterState, now: Instant) {
        match s.last_sample {
            Some((t, b)) => {
                let dt = now.duration_since(t).as_secs_f64();
                if dt > 0.0 {
                    let inst = s.bytes_done.saturating_sub(b) as f64 / dt;
                    let alpha = 1.0 - (-dt / SMOOTHING).exp();
                    s.speed = if s.speed == 0.0 { inst } else { s.speed + alpha * (inst - s.speed) };
                }
            }
            None => s.speed = 0.0,
        }
        s.last_sample = Some((now, s.bytes_done));
    }

    /// Returns a progress snapshot if enough time has passed since the last one.
    pub fn maybe_sample(&self) -> Option<Progress> {
        let mut s = self.lock();
        let now = Instant::now();
        if s.last_emit.is_some_and(|t| now.duration_since(t) < EMIT_EVERY) {
            return None;
        }
        Self::sample(&mut s, now);
        s.last_emit = Some(now);
        Some(Self::build(&s))
    }

    /// A final snapshot regardless of throttling.
    pub fn force_sample(&self) -> Option<Progress> {
        let mut s = self.lock();
        let now = Instant::now();
        Self::sample(&mut s, now);
        s.last_emit = Some(now);
        Some(Self::build(&s))
    }
}
