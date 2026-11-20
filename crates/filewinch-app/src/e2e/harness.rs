//! The real transfer engine with its events on a channel, and bounded waits on them: a job
//! that hangs fails the test with what the engine was doing, never blocks it forever.

use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use filewinch_core::{Error, RPath};

use crate::transfers::engine::{ConflictAnswer, ConflictAsk, Engine, EngineConfig, Event, Job, JobId, JobState};

/// Long enough for a 20 MB round trip on loopback with room for a slow CI machine.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Harness {
    pub engine: Engine,
    rx: Receiver<Event>,
}

/// How a job ended.
#[derive(Debug)]
pub struct Outcome {
    pub state: JobState,
    pub failures: Vec<(RPath, Error)>,
}

impl Outcome {
    #[track_caller]
    pub fn assert_done(&self, what: &str) {
        assert!(matches!(self.state, JobState::Done), "{what}: ended {:?}, failures {:?}", self.state, self.failures);
    }
}

/// What the next wait stopped on.
#[allow(clippy::large_enum_variant)] // one short-lived value per wait
pub enum Stop {
    Finished(Outcome),
    Conflict(ConflictAsk),
}

impl Harness {
    pub fn new() -> Harness {
        Harness::with_config(EngineConfig { retry_delay: Duration::from_millis(100), ..EngineConfig::default() })
    }

    pub fn with_config(cfg: EngineConfig) -> Harness {
        let (tx, rx) = channel();
        let engine = Engine::new(cfg, move |ev| {
            let _ = tx.send(ev);
        });
        Harness { engine, rx }
    }

    /// Enqueues `job` and waits for it to finish; a conflict question fails the test.
    #[track_caller]
    pub fn run(&self, job: Job) -> Outcome {
        let label = job.label.clone();
        let id = self.engine.enqueue(job);
        match self.wait(id, &label) {
            Stop::Finished(o) => o,
            Stop::Conflict(ask) => panic!("{label}: unexpected conflict question for {:?}", ask.dst.path),
        }
    }

    /// Enqueues `job` and waits for its conflict question.
    #[track_caller]
    pub fn run_until_conflict(&self, job: Job) -> (JobId, ConflictAsk) {
        let label = job.label.clone();
        let id = self.engine.enqueue(job);
        match self.wait(id, &label) {
            Stop::Conflict(ask) => (id, ask),
            Stop::Finished(o) => panic!("{label}: expected a conflict question, the job ended {:?}", o.state),
        }
    }

    /// Answers a conflict and waits for the job to finish.
    #[track_caller]
    pub fn answer(&self, id: JobId, answer: ConflictAnswer) -> Outcome {
        self.engine.answer(id, answer);
        match self.wait(id, "answered job") {
            Stop::Finished(o) => o,
            Stop::Conflict(ask) => panic!("job {id}: asked again about {:?} after an answer", ask.dst.path),
        }
    }

    /// Waits until job `id` finishes or asks a question, within [`JOB_TIMEOUT`].
    #[track_caller]
    pub fn wait(&self, id: JobId, label: &str) -> Stop {
        self.wait_for(id, label, JOB_TIMEOUT)
    }

    #[track_caller]
    pub fn wait_for(&self, id: JobId, label: &str, timeout: Duration) -> Stop {
        let deadline = Instant::now() + timeout;
        let mut failures = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Event::State(j, state)) if j == id && state.is_finished() => {
                    return Stop::Finished(Outcome { state, failures });
                }
                Ok(Event::Conflict { job, ask }) if job == id => return Stop::Conflict(ask),
                Ok(Event::ItemFailed { job, path, error }) if job == id => failures.push((path, error)),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!(
                    "{label}: job {id} didn't finish within {timeout:?}; engine: {:?}",
                    self.engine.snapshot().into_iter().map(|(i, l, s, p)| (i, l, s, p.bytes_done)).collect::<Vec<_>>()
                ),
                Err(RecvTimeoutError::Disconnected) => panic!("{label}: the engine stopped sending events"),
            }
        }
    }

    /// Waits for the first progress event of job `id` that has moved bytes.
    #[track_caller]
    pub fn wait_bytes(&self, id: JobId, label: &str) {
        let deadline = Instant::now() + JOB_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Event::Progress(j, p)) if j == id && p.bytes_done > 0 => return,
                Ok(Event::State(j, s)) if j == id && s.is_finished() => panic!("{label}: ended {s:?} before moving any bytes"),
                Ok(_) => {}
                Err(e) => panic!("{label}: no progress within {JOB_TIMEOUT:?} ({e:?})"),
            }
        }
    }
}
