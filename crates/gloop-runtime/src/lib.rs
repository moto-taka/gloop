//! In-process deterministic scheduler with bounded retries and loops.

pub mod artifacts;
pub mod executor;
pub mod journal;
pub mod replay;
pub mod worktree;

pub use artifacts::{ArtifactError, ArtifactStore, AttemptArtifacts, RunPaths};
pub use executor::{
    DefaultHumanGate, GateDecision, GateRequest, HumanGate, NodeFailureClass,
    ProcessCancellationScope, ProgressEvent, ProviderInvocation, ProviderInvoker, RunError,
    RunOptions, Runtime, SUMMARY_SCHEMA_VERSION, node_failure_class,
};
pub use journal::{Journal, JournalError, JournalRead, JournalRow, read_events, read_journal};
pub use replay::{
    ReplayError, ReplayReport, RunInspection, inspect_run, replay_events, replay_journal,
};
pub use worktree::{
    GitWorktreeManager, WorktreeError, WorktreeManifest, WorktreeRecord, WorktreeWorkspace,
};
