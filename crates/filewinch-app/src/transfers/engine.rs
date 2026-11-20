//! The one place the app names the transfer engine. Everything else uses `transfers::engine::*`,
//! so swapping engines (or a stand-in while the engine crate is broken) is this file alone.

pub use filewinch_engine::{
    CancelToken, CompareBy, CompareProgress, ConflictAnswer, ConflictAsk, ConflictChoice, ConflictPolicy, Engine,
    EngineConfig, Event, Job, JobId, JobState, Op, Progress, SyncAction, SyncActionKind, SyncConflict, SyncDirection,
    SyncOptions, SyncPlan, apply, compare_with,
};
