//! Versioned graph types, validation, and deterministic compilation.

pub mod graph;
pub mod render;
pub mod state;

pub use graph::{
    CompiledGraph, ContextSpec, Edge, EdgeCondition, EdgeKind, FailurePolicy, GateDefault, Graph,
    GraphError, GraphMetadata, GraphPolicies, GraphSpec, IssueSeverity, LoopCondition, Node,
    NodeKind, NodeStatus, OutputFormat, OutputSpec, PromptSpec, RetryPolicy, RunBudgets,
    ValidationIssue, WorkspaceSpec,
};
pub use state::{
    ArtifactRef, CheckResult, FinalStatus, ModelUsage, NodeOutcome, Provenance, RunEvent,
    RunEventKind, RunSummary,
};
