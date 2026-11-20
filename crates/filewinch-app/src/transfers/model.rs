//! The transfers panel's model: one row per engine job, fed by engine events.

use filewinch_core::{Error, RPath};

use super::engine::{ConflictAsk, Event, JobId, JobState, Op, Progress};

/// Which way a job moves data, for its icon.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    /// From This Mac to a server.
    Upload,
    /// From a server to This Mac.
    Download,
    /// Between two servers, or within one side (a duplicate, a move).
    #[default]
    Across,
    Delete,
}

impl Direction {
    /// `src_local`/`dst_local`: whether each side is This Mac (`dst_local` is `None` without a
    /// destination).
    pub fn of(op: &Op, src_local: bool, dst_local: Option<bool>) -> Direction {
        match (op, src_local, dst_local) {
            (Op::Delete, _, _) | (_, _, None) => Direction::Delete,
            (_, true, Some(false)) => Direction::Upload,
            (_, false, Some(true)) => Direction::Download,
            _ => Direction::Across,
        }
    }
}

/// Why the window started a job, so it can follow up when the job ends.
#[derive(Clone, Debug)]
pub enum Purpose {
    /// A transfer the person asked for: refresh the folders it touched.
    Plain,
    /// Downloading files to hand to a drag out to Finder.
    Staging { files: Vec<(u64, RPath, std::path::PathBuf)> },
    /// Downloading a file to open it in its app (`watch`: then watch it for saves).
    Open { edit: u64 },
    /// Uploading a saved external edit.
    EditUpload { edit: u64 },
}

/// The folders a job touches, by session.
#[derive(Clone, Debug)]
pub struct JobMeta {
    pub purpose: Purpose,
    pub touched: Vec<(u64, RPath)>,
}

pub struct Row {
    pub id: JobId,
    pub label: String,
    pub direction: Direction,
    pub state: JobState,
    pub progress: Progress,
    pub failures: Vec<(RPath, Error)>,
    pub expanded: bool,
    pub question: Option<ConflictAsk>,
    pub apply_to_all: bool,
    /// For a download to this computer: the first item, to show in the file manager once done.
    pub reveal: Option<std::path::PathBuf>,
}

impl Row {
    pub fn fraction(&self) -> Option<f32> {
        let total = self.progress.bytes_total?;
        if total == 0 {
            return Some(if matches!(self.state, JobState::Done) { 1.0 } else { 0.0 });
        }
        Some((self.progress.bytes_done as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }

    pub fn state_label(&self) -> String {
        match &self.state {
            JobState::Queued => "Waiting".into(),
            JobState::Scanning => "Counting items…".into(),
            JobState::Running => "Transferring".into(),
            JobState::Paused => "Paused".into(),
            JobState::WaitingForAnswer => "Needs your answer".into(),
            JobState::Retrying { attempt, .. } => format!("Connection lost, retrying (attempt {attempt})"),
            JobState::Done => "Done".into(),
            JobState::Failed(e) => e.message.clone(),
            JobState::Cancelled => "Cancelled".into(),
        }
    }
}

#[derive(Default)]
pub struct Transfers {
    pub rows: Vec<Row>,
}

impl Transfers {
    /// Applies an engine event. Returns `(job, succeeded)` when a job just finished.
    pub fn apply(&mut self, ev: &Event) -> Option<(JobId, bool)> {
        match ev {
            Event::Added(id) => {
                if self.row(*id).is_none() {
                    self.rows.push(Row {
                        id: *id,
                        label: String::new(),
                        direction: Direction::default(),
                        state: JobState::Queued,
                        progress: Progress::default(),
                        failures: Vec::new(),
                        expanded: false,
                        question: None,
                        apply_to_all: false,
                        reveal: None,
                    });
                }
                None
            }
            Event::State(id, st) => {
                let row = self.row_mut(*id)?;
                let was_finished = row.state.is_finished();
                row.state = st.clone();
                if !matches!(st, JobState::WaitingForAnswer) {
                    row.question = None;
                }
                if matches!(st, JobState::Failed(_)) {
                    row.expanded = true;
                }
                (st.is_finished() && !was_finished).then_some((*id, matches!(st, JobState::Done)))
            }
            Event::Progress(id, p) => {
                self.row_mut(*id)?.progress = p.clone();
                None
            }
            Event::Conflict { job, ask } => {
                let row = self.row_mut(*job)?;
                row.question = Some(ask.clone());
                row.apply_to_all = false;
                None
            }
            Event::ItemFailed { job, path, error } => {
                self.row_mut(*job)?.failures.push((path.clone(), error.clone()));
                None
            }
        }
    }

    /// Fills in labels the engine knows (events don't carry them).
    pub fn set_label(&mut self, id: JobId, label: &str) {
        if let Some(r) = self.row_mut(id) {
            r.label = label.to_owned();
        }
    }

    pub fn set_direction(&mut self, id: JobId, direction: Direction) {
        if let Some(r) = self.row_mut(id) {
            r.direction = direction;
        }
    }

    pub fn row(&self, id: JobId) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }

    pub fn row_mut(&mut self, id: JobId) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }

    pub fn has_question(&self) -> bool {
        self.rows.iter().any(|r| r.question.is_some())
    }

    pub fn active(&self) -> usize {
        self.rows.iter().filter(|r| !r.state.is_finished()).count()
    }

    /// Removes finished jobs that succeeded or were cancelled; failed ones stay until retried
    /// or cleared one by one.
    pub fn clear_completed(&mut self) -> Vec<JobId> {
        let gone: Vec<JobId> =
            self.rows.iter().filter(|r| matches!(r.state, JobState::Done | JobState::Cancelled)).map(|r| r.id).collect();
        self.rows.retain(|r| !gone.contains(&r.id));
        gone
    }

    /// One line for the toolbar: "2 transfers, 4.1 MB/s".
    pub fn summary(&self) -> Option<String> {
        let active: Vec<&Row> = self.rows.iter().filter(|r| !r.state.is_finished()).collect();
        if active.is_empty() {
            return None;
        }
        let speed: f64 = active.iter().map(|r| r.progress.bytes_per_sec).sum();
        let n = active.len();
        let what = if n == 1 { "1 transfer".to_owned() } else { format!("{n} transfers") };
        Some(if speed >= 1.0 { format!("{what}, {}", crate::fmt::speed(speed)) } else { what })
    }
}
