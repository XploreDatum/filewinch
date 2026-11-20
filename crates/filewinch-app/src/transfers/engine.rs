//! The one place the app names the transfer engine. Everything else uses `transfers::engine::*`,
//! so swapping engines (or a stand-in while the engine crate is broken) is this file alone.

pub use filewinch_engine::{
    ActivePhase, CancelToken, CompareBy, CompareProgress, ConflictAnswer, ConflictAsk, ConflictChoice, ConflictPolicy,
    Engine, EngineConfig, Event, ItemCounts, ItemState, ItemView, Job, JobId, JobState, MAX_FILES_PER_JOB_LIVE, Op,
    Progress, SyncActionKind, SyncDirection, SyncOptions, SyncPlan, applied_mode, apply, compare_with,
};
// Built by hand only in the demo scenes (the dev-tools feature).
#[cfg(any(test, feature = "dev-tools"))]
pub use filewinch_engine::{SyncAction, SyncConflict};
