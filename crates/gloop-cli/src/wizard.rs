//! Interactive and template-driven graph builders for gloop.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use dialoguer::{Confirm, Editor, Input, MultiSelect, Select, theme::ColorfulTheme};
use gloop_core::graph::{
    MAX_CONTEXT_BYTES, MAX_DURATION_SECONDS, MAX_FAN_OUT, MAX_LOOP_ITERATIONS, MAX_OUTPUT_BYTES,
    MAX_PARALLELISM, MAX_RETRY_ATTEMPTS,
};
use gloop_core::{
    ContextSpec, Edge, EdgeCondition, EdgeKind, FailurePolicy, Graph, IssueSeverity, LoopCondition,
    Node, NodeKind, NodeStatus, OutputFormat, OutputSpec, PromptSpec, RetryPolicy, RunBudgets,
    WorkspaceSpec,
};
use serde_json::Value;

use crate::atomic_write::{write_text_atomic_sync, write_text_no_replace_sync};
use crate::templates::{DEFAULT_TEMPLATE_GOAL, template_path, validate_init_template_name};

const INTERACTIVE_NESTING_LIMIT: usize = 8;

/// A configured provider profile available for interactive selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChoice {
    pub name: String,
    pub kind: String,
    pub source: ProfileSource,
    pub enabled: bool,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    Builtin,
    User,
    Project,
}

impl ProfileSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

pub fn profile_choice_label(choice: &ProfileChoice) -> String {
    format!(
        "{}  ({}, {})",
        choice.name,
        choice.kind,
        choice.source.label()
    )
}

/// Interactive editor state; mutations are applied through pure helpers.
#[derive(Debug, Clone)]
pub struct EditorState {
    pub graph: Graph,
    pub depth: usize,
}

impl EditorState {
    pub fn new(name: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            graph: Graph::new(name, goal, Vec::new()),
            depth: 0,
        }
    }

    pub fn from_graph(graph: Graph, depth: usize) -> Self {
        Self { graph, depth }
    }
}

/// Result of a nested interactive editor session.
#[derive(Debug, Clone, PartialEq)]
pub enum NestedEditorOutcome {
    Saved(Box<Graph>),
    Cancelled,
}

/// Optional persistence target for the interactive editor save action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorPersistTarget {
    None,
    ProjectTemplate { repo: PathBuf, force: bool },
    GraphFile { path: PathBuf, force: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartTemplate {
    Empty,
    Builtin(GraphTemplate),
}

pub fn seed_editor_from_template(
    name: &str,
    goal: &str,
    template: GraphTemplate,
    knobs: TemplateKnobs,
) -> EditorState {
    let graph = template_graph(
        name,
        goal,
        template,
        knobs.request,
        Some(knobs.provider_profiles),
        knobs.loop_cap,
    );
    EditorState::from_graph(graph, 0)
}

pub fn editor_summary_header(state: &EditorState) -> (String, String) {
    let node_count = state.graph.spec.nodes.len();
    let edge_count = state.graph.spec.edges.len();
    let header = format!(
        "Graph \"{}\": {node_count} nodes, {edge_count} edges",
        state.graph.metadata.name
    );
    let node_line = state
        .graph
        .spec
        .nodes
        .iter()
        .map(format_node_summary)
        .collect::<Vec<_>>()
        .join(" -> ");
    (header, node_line)
}

pub fn format_node_summary(node: &Node) -> String {
    let kind = match &node.kind {
        NodeKind::Agent { profile, .. } => profile
            .as_deref()
            .map_or_else(|| "agent".to_owned(), |value| format!("agent:{value}")),
        NodeKind::Reduce { profile, .. } => profile
            .as_deref()
            .map_or_else(|| "reduce".to_owned(), |value| format!("reduce:{value}")),
        NodeKind::Synthesize { profile, .. } => profile.as_deref().map_or_else(
            || "synthesize".to_owned(),
            |value| format!("synthesize:{value}"),
        ),
        NodeKind::Command { .. } => "command".to_owned(),
        NodeKind::Verify { .. } => "verify".to_owned(),
        NodeKind::Gate { .. } => "gate".to_owned(),
        NodeKind::Loop { .. } => "loop".to_owned(),
        NodeKind::Subgraph { .. } => "subgraph".to_owned(),
    };
    format!("{}({kind})", node.id)
}

pub fn add_node_to_editor(
    state: &EditorState,
    node: Node,
    dependency_ids: &[&str],
    selected_indices: &[usize],
    dependency_drafts: &[DependencyDraft],
) -> Result<EditorState> {
    let mut draft = state.graph.clone();
    draft.spec.edges.extend(build_dependency_edges(
        dependency_ids,
        &node.id,
        selected_indices,
        dependency_drafts,
    )?);
    ensure_workspace_inheritance_edge(&node, &mut draft.spec.edges)?;
    draft.spec.nodes.push(node);
    validate_graph_errors(&draft)?;
    Ok(EditorState {
        graph: draft,
        depth: state.depth,
    })
}

pub fn workspace_inheritance_dependents(state: &EditorState, source_id: &str) -> Vec<String> {
    state
        .graph
        .spec
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                &node.workspace,
                WorkspaceSpec::Inherit { node } if node == source_id
            )
        })
        .map(|node| node.id.clone())
        .collect()
}

fn workspace_inherit_source(node: &Node) -> Option<&str> {
    match &node.workspace {
        WorkspaceSpec::Inherit { node } => Some(node.as_str()),
        _ => None,
    }
}

fn is_auto_workspace_inheritance_edge(edge: &Edge, source: &str, target: &str) -> bool {
    edge.from == source && edge.to == target && edge.kind == EdgeKind::Data && edge.when.is_none()
}

pub fn remove_auto_workspace_inheritance_edge(edges: &mut Vec<Edge>, source: &str, target: &str) {
    edges.retain(|edge| !is_auto_workspace_inheritance_edge(edge, source, target));
}

pub fn reconcile_workspace_inheritance_edges(old: &Node, new: &Node, edges: &mut Vec<Edge>) {
    let old_source = workspace_inherit_source(old);
    let new_source = workspace_inherit_source(new);

    if old_source == new_source {
        if new_source.is_some() {
            let _ = ensure_workspace_inheritance_edge(new, edges);
        }
        return;
    }

    if let Some(source) = old_source {
        remove_auto_workspace_inheritance_edge(edges, source, &old.id);
    }
    if new_source.is_some() {
        let _ = ensure_workspace_inheritance_edge(new, edges);
    }
}

pub fn remove_node_from_editor(state: &EditorState, node_id: &str) -> Result<EditorState> {
    if !state.graph.spec.nodes.iter().any(|node| node.id == node_id) {
        return Err(anyhow!("node '{node_id}' was not found"));
    }
    let dependents = workspace_inheritance_dependents(state, node_id);
    if !dependents.is_empty() {
        return Err(anyhow!(
            "cannot remove '{node_id}' while nodes inherit its workspace: {}",
            dependents.join(", ")
        ));
    }
    let mut graph = state.graph.clone();
    graph.spec.nodes.retain(|node| node.id != node_id);
    graph
        .spec
        .edges
        .retain(|edge| edge.from != node_id && edge.to != node_id);
    Ok(EditorState {
        graph,
        depth: state.depth,
    })
}

pub fn replace_node_in_editor(
    state: &EditorState,
    node_id: &str,
    node: Node,
) -> Result<EditorState> {
    if node.id != node_id {
        return Err(anyhow!("replacement node id must match '{node_id}'"));
    }
    let Some(index) = state
        .graph
        .spec
        .nodes
        .iter()
        .position(|candidate| candidate.id == node_id)
    else {
        return Err(anyhow!("node '{node_id}' was not found"));
    };
    let old = state.graph.spec.nodes[index].clone();
    let mut graph = state.graph.clone();
    reconcile_workspace_inheritance_edges(&old, &node, &mut graph.spec.edges);
    graph.spec.nodes[index] = node;
    validate_graph_errors(&graph)?;
    Ok(EditorState {
        graph,
        depth: state.depth,
    })
}

pub fn add_edge_to_editor(state: &EditorState, edge: Edge) -> Result<EditorState> {
    let mut graph = state.graph.clone();
    if graph.spec.edges.iter().any(|existing| {
        existing.from == edge.from && existing.to == edge.to && existing.kind == edge.kind
    }) {
        return Err(anyhow!(
            "edge from '{}' to '{}' with kind {:?} already exists",
            edge.from,
            edge.to,
            edge.kind
        ));
    }
    graph.spec.edges.push(edge);
    validate_graph_errors(&graph)?;
    Ok(EditorState {
        graph,
        depth: state.depth,
    })
}

pub fn remove_edge_from_editor(
    state: &EditorState,
    from: &str,
    to: &str,
    kind: EdgeKind,
) -> Result<EditorState> {
    let mut graph = state.graph.clone();
    let before = graph.spec.edges.len();
    graph
        .spec
        .edges
        .retain(|edge| !(edge.from == from && edge.to == to && edge.kind == kind));
    if graph.spec.edges.len() == before {
        return Err(anyhow!("matching edge was not found"));
    }
    validate_graph_errors(&graph)?;
    Ok(EditorState {
        graph,
        depth: state.depth,
    })
}

pub fn apply_editor_settings(state: &EditorState, settings: GraphSettings) -> EditorState {
    let mut graph = state.graph.clone();
    apply_graph_settings(&mut graph, settings);
    EditorState {
        graph,
        depth: state.depth,
    }
}

pub fn validation_errors(state: &EditorState) -> Vec<String> {
    state
        .graph
        .validate()
        .into_iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .map(|issue| format!("[{}] at {}: {}", issue.code, issue.path, issue.message))
        .collect()
}

pub fn validation_warnings(state: &EditorState) -> Vec<String> {
    state
        .graph
        .validate()
        .into_iter()
        .filter(|issue| issue.severity == IssueSeverity::Warning)
        .map(|issue| format!("[{}] at {}: {}", issue.code, issue.path, issue.message))
        .collect()
}

pub fn validate_for_save(state: &EditorState) -> Result<(), Vec<String>> {
    let mut errors = validation_errors(state);
    if state.graph.spec.nodes.is_empty() {
        errors.push("A graph must contain at least one node.".to_owned());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
fn map_profile_selection(
    profiles: &[ProfileChoice],
    selected_index: usize,
) -> Result<Option<String>> {
    if selected_index < profiles.len() {
        return Ok(Some(profiles[selected_index].name.clone()));
    }
    if selected_index == profiles.len() {
        return Ok(None);
    }
    if selected_index == profiles.len() + 1 {
        return Err(anyhow!("manual profile entry requires interactive input"));
    }
    Err(anyhow!("invalid profile selection index"))
}

pub fn profile_select_items(profiles: &[ProfileChoice]) -> Vec<String> {
    let mut items: Vec<String> = profiles.iter().map(profile_choice_label).collect();
    items.push("(use default routing)".to_owned());
    items.push("(type a profile name manually)".to_owned());
    items
}

pub fn enabled_profile_choices(profiles: &[ProfileChoice]) -> Vec<ProfileChoice> {
    profiles
        .iter()
        .filter(|choice| choice.enabled)
        .cloned()
        .collect()
}

pub fn profile_default_select_index(
    profiles: &[ProfileChoice],
    default_profile: Option<&str>,
) -> usize {
    match default_profile {
        None | Some("") => profiles.len(),
        Some(name) => profiles
            .iter()
            .position(|choice| choice.name == name)
            .unwrap_or(profiles.len() + 1),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualProfileEntry {
    DefaultRouting,
    Accepted(String),
    KnownDisabled,
}

pub fn classify_manual_profile_name(name: &str, profiles: &[ProfileChoice]) -> ManualProfileEntry {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return ManualProfileEntry::DefaultRouting;
    }
    if let Some(choice) = profiles.iter().find(|choice| choice.name == trimmed) {
        if choice.enabled {
            ManualProfileEntry::Accepted(trimmed.to_owned())
        } else {
            ManualProfileEntry::KnownDisabled
        }
    } else {
        ManualProfileEntry::Accepted(trimmed.to_owned())
    }
}

pub fn profile_model_default(
    selected_profile: Option<&str>,
    previous_profile: Option<&str>,
    previous_model: Option<&str>,
    profiles: &[ProfileChoice],
) -> String {
    let profile_changed = selected_profile != previous_profile;
    if profile_changed {
        return selected_profile
            .and_then(|name| {
                profiles
                    .iter()
                    .find(|choice| choice.name == name)
                    .and_then(|choice| choice.default_model.clone())
            })
            .unwrap_or_default();
    }

    previous_model
        .map(str::to_owned)
        .or_else(|| {
            selected_profile.and_then(|name| {
                profiles
                    .iter()
                    .find(|choice| choice.name == name)
                    .and_then(|choice| choice.default_model.clone())
            })
        })
        .unwrap_or_default()
}

pub fn preflight_template_destination(repo: &Path, name: &str, force: bool) -> Result<(), String> {
    let destination = template_path(repo, name);
    if destination.exists() && !force {
        return Err(format!(
            "template '{}' already exists at {}; choose another name or use --force",
            name,
            destination.display()
        ));
    }
    Ok(())
}

pub fn try_persist_editor_graph(graph: &Graph, target: &EditorPersistTarget) -> Result<(), String> {
    let yaml = graph
        .to_yaml()
        .map_err(|error| format!("serialization failed: {error}"))?;

    match target {
        EditorPersistTarget::None => Ok(()),
        EditorPersistTarget::ProjectTemplate { repo, force } => {
            let destination = template_path(repo, &graph.metadata.name);
            write_graph_yaml(&destination, &yaml, *force)
        }
        EditorPersistTarget::GraphFile { path, force } => write_graph_yaml(path, &yaml, *force),
    }
}

fn write_graph_yaml(destination: &Path, yaml: &str, force: bool) -> Result<(), String> {
    // Called from the synchronous interactive editor, which itself runs inside
    // the CLI's Tokio runtime: creating (or blocking on) another runtime here
    // panics, so persistence uses the shared synchronous primitives directly.
    let write_result = if force {
        write_text_atomic_sync(destination, yaml)
    } else {
        write_text_no_replace_sync(destination, yaml)
    };

    write_result.map_err(|error| {
        if !force && error.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "output path exists: {}; use --force to overwrite",
                destination.display()
            )
        } else {
            format!(
                "failed to write graph to {}: {error}",
                destination.display()
            )
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphTemplate {
    Direct,
    PlanImplementVerify,
    ParallelResearchReduce,
    ReviewFixLoop,
    DesignWallBounce,
    Council,
    DecomposeFanoutReduce,
    ImplementTestLoop,
}

pub fn request_graph(
    name: impl Into<String>,
    goal: impl Into<String>,
    request: impl Into<String>,
) -> Graph {
    template_graph(
        name,
        goal,
        GraphTemplate::Direct,
        Some(request.into()),
        None,
        None,
    )
}

pub fn template_graph(
    name: impl Into<String>,
    goal: impl Into<String>,
    template: GraphTemplate,
    request: Option<String>,
    provider_profiles: Option<Vec<String>>,
    loop_cap: Option<u32>,
) -> Graph {
    let name = name.into();
    let goal = goal.into();
    let profiles = provider_profiles.unwrap_or_default();

    match template {
        GraphTemplate::Direct => direct_template(name, goal, request, &profiles),
        GraphTemplate::PlanImplementVerify => {
            plan_implement_verify_template(name, goal, request, &profiles)
        }
        GraphTemplate::ParallelResearchReduce => {
            parallel_research_reduce_template(name, goal, request, &profiles)
        }
        GraphTemplate::ReviewFixLoop => {
            review_fix_loop_template(name, goal, request, &profiles, loop_cap)
        }
        GraphTemplate::DesignWallBounce => {
            design_wall_bounce_template(name, goal, request, &profiles)
        }
        GraphTemplate::Council => council_template(name, goal, request, &profiles),
        GraphTemplate::DecomposeFanoutReduce => {
            decompose_fanout_reduce_template(name, goal, request, &profiles)
        }
        GraphTemplate::ImplementTestLoop => {
            implement_test_loop_template(name, goal, request, &profiles, loop_cap)
        }
    }
}

fn direct_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "Complete the requested task".to_owned());
    let prompt = format!("Complete this request and return the result:\n{request}");
    Graph::new(
        name,
        goal,
        vec![agent_node("request", &prompt, profiles.first().cloned(), 1)],
    )
}

fn plan_implement_verify_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested work".to_owned());
    let mut graph = Graph::new(
        name,
        goal,
        vec![
            agent_node(
                "plan",
                &format!(
                    "Plan role: planner. Write a concrete implementation plan for:\n{request}"
                ),
                profiles.first().cloned(),
                1,
            ),
            agent_node(
                "implement",
                &format!("Plan role: implementer. Execute the plan for:\n{request}"),
                profiles.get(1).cloned(),
                1,
            ),
            verify_node(
                "verify",
                vec!["git".into(), "diff".into(), "--check".into()],
            ),
        ],
    );
    graph.spec.edges.push(Edge::data("plan", "implement"));
    graph.spec.edges.push(Edge::data("implement", "verify"));
    graph
}

fn parallel_research_reduce_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested problem".to_owned());
    let mut graph = Graph::new(
        name,
        goal,
        vec![
            agent_node(
                "research_one",
                &format!("Research lane 1 for:\n{request}"),
                profiles.first().cloned(),
                2,
            ),
            agent_node(
                "research_two",
                &format!("Research lane 2 for:\n{request}"),
                profiles.get(1).cloned(),
                2,
            ),
            agent_node(
                "research_three",
                &format!("Research lane 3 for:\n{request}"),
                profiles.get(2).cloned(),
                2,
            ),
            reduce_node(
                "synthesize",
                "Combine findings from all lanes into one reconciled recommendation.",
                profiles.get(3).cloned(),
            ),
        ],
    );
    graph.spec.edges = vec![
        Edge::data("research_one", "synthesize"),
        Edge::data("research_two", "synthesize"),
        Edge::data("research_three", "synthesize"),
    ];
    graph.metadata.description = Some("parallel research and synthesis".to_owned());
    graph
}

fn review_fix_loop_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
    loop_cap: Option<u32>,
) -> Graph {
    let request = request.unwrap_or_else(|| "review the artifact".to_owned());
    let max_iterations = loop_cap.unwrap_or(4);
    let review_profile = profiles.first().cloned();
    let fix_profile = profiles.get(1).cloned();

    let nested_nodes = vec![
        agent_node(
            "review",
            &format!(
                "Reviewer role: inspect output for defects and risks, then request follow-up fixes for:\n{request}"
            ),
            review_profile,
            1,
        ),
        agent_node(
            "fix",
            &format!("Implement requested fixes for:\n{request}"),
            fix_profile,
            1,
        ),
        verify_node("judge", vec!["git".into(), "diff".into(), "--check".into()]),
    ];
    let mut nested_graph = Graph::new(
        "review-fix-iteration",
        "single review-fix iteration",
        nested_nodes,
    );
    nested_graph.spec.edges = vec![Edge::data("review", "fix"), Edge::data("fix", "judge")];
    nested_graph.metadata.description = Some("bounded review/fix iteration".to_owned());

    let loop_node = Node {
        id: "review_fix_loop".to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Loop {
            graph: Box::new(nested_graph),
            until: LoopCondition {
                node: "judge".to_owned(),
                status: gloop_core::NodeStatus::Succeeded,
                output_contains: None,
                json_pointer: None,
                equals: None,
            },
            max_iterations,
            stagnation_after: 2,
        },
    };

    let publish = command_node(
        "publish",
        vec!["printf".into(), "ready for release\n".into()],
    );
    let mut graph = Graph::new(name, goal, vec![loop_node, publish]);
    graph.spec.edges.push(Edge {
        from: "review_fix_loop".to_owned(),
        to: "publish".to_owned(),
        kind: EdgeKind::Control,
        when: None,
    });
    graph
}

/// Two designers produce independent blind proposals, critique each other's
/// proposal (wall-bounce), revise their own design in light of the critique,
/// and a final node integrates the two revised designs.
fn design_wall_bounce_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested design".to_owned());
    let lane_one_profile = profiles
        .first()
        .cloned()
        .unwrap_or_else(|| "claude".to_owned());
    let lane_two_profile = profiles
        .get(1)
        .cloned()
        .unwrap_or_else(|| "codex".to_owned());
    // The default model aliases only exist on the default harnesses; skip the
    // binding when a lane is rebound to a different profile.
    let lane_one_model = (lane_one_profile == "claude").then(|| "fable".to_owned());
    let lane_two_model = (lane_two_profile == "codex").then(|| "gpt-5.6-sol".to_owned());

    let design_prompt = |lane: &str| {
        format!(
            "You are designer {lane}, one of two independent designers. Produce a complete design for the following request. Work blindly: do not reference or assume any other proposal.\n\nRequest:\n{request}\n\nOutput sections: 1) goal and scope, 2) proposed design, 3) risks and mitigations, 4) open questions."
        )
    };
    let critique_prompt = |lane: &str, other: &str| {
        format!(
            "You are designer {lane}. Act as a wall-bounce partner for designer {other}: critique designer {other}'s proposal, which is provided in the dependency output. Identify weaknesses, gaps, risks, and concrete improvements. Be specific and constructive; do not rewrite the whole design."
        )
    };
    let revise_prompt = |lane: &str, other: &str| {
        format!(
            "You are designer {lane}. Revise your original design in light of designer {other}'s critique; both are provided in the dependency output. Keep what survives critique, fix what does not, and note any remaining disagreements explicitly."
        )
    };
    let lane_node = |id: &str, prompt: &str, profile: &str, model: Option<&str>| {
        let mut node = agent_node(id, prompt, Some(profile.to_owned()), 1);
        set_node_model(&mut node, model);
        node
    };

    let graph_nodes = vec![
        lane_node(
            "design_one",
            &design_prompt("one"),
            &lane_one_profile,
            lane_one_model.as_deref(),
        ),
        lane_node(
            "design_two",
            &design_prompt("two"),
            &lane_two_profile,
            lane_two_model.as_deref(),
        ),
        lane_node(
            "review_by_one",
            &critique_prompt("one", "two"),
            &lane_one_profile,
            lane_one_model.as_deref(),
        ),
        lane_node(
            "review_by_two",
            &critique_prompt("two", "one"),
            &lane_two_profile,
            lane_two_model.as_deref(),
        ),
        lane_node(
            "revise_one",
            &revise_prompt("one", "two"),
            &lane_one_profile,
            lane_one_model.as_deref(),
        ),
        lane_node(
            "revise_two",
            &revise_prompt("two", "one"),
            &lane_two_profile,
            lane_two_model.as_deref(),
        ),
        {
            let mut final_design = synthesize_node(
                "final_design",
                &format!(
                    "Integrate the two revised designs (provided in the dependency output) into one final design for:\n{request}\n\nOutput sections: 1) points of agreement, 2) disagreements and how they are resolved, 3) final design decisions, 4) remaining open questions."
                ),
                Some(lane_one_profile.clone()),
            );
            set_node_model(&mut final_design, lane_one_model.as_deref());
            final_design
        },
    ];

    let mut graph = Graph::new(name, goal, graph_nodes);
    graph.spec.edges = vec![
        Edge::data("design_two", "review_by_one"),
        Edge::data("design_one", "review_by_two"),
        Edge::data("design_one", "revise_one"),
        Edge::data("review_by_two", "revise_one"),
        Edge::data("design_two", "revise_two"),
        Edge::data("review_by_one", "revise_two"),
        Edge::data("revise_one", "final_design"),
        Edge::data("revise_two", "final_design"),
    ];
    graph.spec.policies.max_parallel = 4;
    graph.spec.budgets = gloop_core::RunBudgets {
        wall_time_seconds: Some(3600),
        model_calls: Some(7),
    };
    graph.metadata.description = Some(
        "two independent designers wall-bounce each other's proposals and integrate".to_owned(),
    );
    graph
}

fn set_node_model(node: &mut Node, model: Option<&str>) {
    match &mut node.kind {
        NodeKind::Agent { model: slot, .. }
        | NodeKind::Reduce { model: slot, .. }
        | NodeKind::Synthesize { model: slot, .. } => {
            *slot = model.map(ToOwned::to_owned);
        }
        _ => {}
    }
}

#[cfg(test)]
pub fn graph_from_yaml_bytes(contents: impl AsRef<str>) -> Result<Graph> {
    Graph::from_yaml_str(contents.as_ref())
        .map_err(|error| anyhow!("failed to parse graph YAML: {error}"))
}

pub fn interactive_graph(profiles: &[ProfileChoice]) -> Result<Graph> {
    interactive_graph_with_seed(None, None, profiles, &EditorPersistTarget::None)
}

pub fn interactive_edit_graph(
    graph: Graph,
    profiles: &[ProfileChoice],
    persist: &EditorPersistTarget,
) -> Result<Graph> {
    let state = EditorState::from_graph(graph, 0);
    interactive_graph_inner(&ColorfulTheme::default(), state, profiles, persist)
}

pub fn interactive_graph_with_seed(
    name: Option<&str>,
    goal: Option<&str>,
    profiles: &[ProfileChoice],
    persist: &EditorPersistTarget,
) -> Result<Graph> {
    let theme = ColorfulTheme::default();
    let name = prompt_identifier(&theme, "Graph name", name, &[])?;
    let goal = prompt_nonempty_text(&theme, "Graph goal", goal)?;
    let start = prompt_start_from(&theme)?;
    let state = seed_editor_state(&theme, &name, &goal, start, profiles)?;
    interactive_graph_inner(&theme, state, profiles, persist)
}

pub fn interactive_template_init(
    profiles: &[ProfileChoice],
    repo: &Path,
    force: bool,
) -> Result<Graph> {
    let theme = ColorfulTheme::default();
    let template_name = prompt_template_name(&theme, None)?;
    preflight_template_destination(repo, &template_name, force).map_err(|error| anyhow!(error))?;
    let description = prompt_optional_description(&theme)?;
    let start = prompt_start_from(&theme)?;
    let mut state = seed_editor_state(
        &theme,
        &template_name,
        DEFAULT_TEMPLATE_GOAL,
        start,
        profiles,
    )?;
    state.graph.metadata.name = template_name;
    if let Some(description) = description {
        state.graph.metadata.description = Some(description);
    }
    interactive_graph_inner(
        &theme,
        state,
        profiles,
        &EditorPersistTarget::ProjectTemplate {
            repo: repo.to_path_buf(),
            force,
        },
    )
}

fn interactive_graph_inner(
    theme: &ColorfulTheme,
    state: EditorState,
    profiles: &[ProfileChoice],
    persist: &EditorPersistTarget,
) -> Result<Graph> {
    match interactive_editor_loop(theme, state, profiles, persist)? {
        NestedEditorOutcome::Saved(graph) => Ok(*graph),
        NestedEditorOutcome::Cancelled => Err(anyhow!("graph authoring cancelled")),
    }
}

fn prompt_template_name(theme: &ColorfulTheme, default: Option<&str>) -> Result<String> {
    loop {
        let mut input =
            Input::with_theme(theme).with_prompt("Template name (kebab-case, max 64 characters)");
        if let Some(default) = default {
            input = input.default(default.to_owned());
        }
        let candidate: String = input.interact_text()?;
        if let Err(error) = validate_init_template_name(&candidate) {
            eprintln!("{error}");
            continue;
        }
        return Ok(candidate);
    }
}

fn prompt_optional_description(theme: &ColorfulTheme) -> Result<Option<String>> {
    let value: String = Input::with_theme(theme)
        .with_prompt("Optional template description")
        .allow_empty(true)
        .interact_text()?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.trim().to_owned()))
    }
}

fn prompt_start_from(theme: &ColorfulTheme) -> Result<StartTemplate> {
    let labels = [
        "empty graph (build node-by-node)",
        "direct",
        "plan-implement-verify",
        "parallel-research-reduce",
        "review-fix-loop",
        "design-wall-bounce",
        "council",
        "decompose-fanout-reduce",
        "implement-test-loop",
    ];
    let selected = Select::with_theme(theme)
        .with_prompt("Start from")
        .items(labels)
        .default(0)
        .interact()?;

    Ok(match selected {
        0 => StartTemplate::Empty,
        1 => StartTemplate::Builtin(GraphTemplate::Direct),
        2 => StartTemplate::Builtin(GraphTemplate::PlanImplementVerify),
        3 => StartTemplate::Builtin(GraphTemplate::ParallelResearchReduce),
        4 => StartTemplate::Builtin(GraphTemplate::ReviewFixLoop),
        5 => StartTemplate::Builtin(GraphTemplate::DesignWallBounce),
        6 => StartTemplate::Builtin(GraphTemplate::Council),
        7 => StartTemplate::Builtin(GraphTemplate::DecomposeFanoutReduce),
        8 => StartTemplate::Builtin(GraphTemplate::ImplementTestLoop),
        _ => unreachable!("invalid start template selection"),
    })
}

fn seed_editor_state(
    theme: &ColorfulTheme,
    name: &str,
    goal: &str,
    start: StartTemplate,
    profiles: &[ProfileChoice],
) -> Result<EditorState> {
    match start {
        StartTemplate::Empty => Ok(EditorState::new(name, goal)),
        StartTemplate::Builtin(template) => {
            let knobs = prompt_template_knobs(theme, template, profiles)?;
            Ok(seed_editor_from_template(name, goal, template, knobs))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TemplateKnobs {
    request: Option<String>,
    provider_profiles: Vec<String>,
    loop_cap: Option<u32>,
}

fn template_profile_slot_count(template: GraphTemplate) -> usize {
    match template {
        GraphTemplate::Direct => 1,
        GraphTemplate::PlanImplementVerify
        | GraphTemplate::ReviewFixLoop
        | GraphTemplate::DesignWallBounce => 2,
        GraphTemplate::ImplementTestLoop => 3,
        GraphTemplate::ParallelResearchReduce => 4,
        GraphTemplate::DecomposeFanoutReduce => 6,
        GraphTemplate::Council => 8,
    }
}

fn prompt_template_knobs(
    theme: &ColorfulTheme,
    template: GraphTemplate,
    profiles: &[ProfileChoice],
) -> Result<TemplateKnobs> {
    let request = prompt_optional_text_with_entry(theme, "Optional request text")?;
    let provider_profiles =
        prompt_template_provider_profiles(theme, profiles, template_profile_slot_count(template))?;
    let loop_cap = if matches!(
        template,
        GraphTemplate::ReviewFixLoop | GraphTemplate::ImplementTestLoop
    ) {
        prompt_optional_number(
            theme,
            "Optional loop iteration cap",
            1u32,
            MAX_LOOP_ITERATIONS,
        )?
    } else {
        None
    };

    Ok(TemplateKnobs {
        request,
        provider_profiles,
        loop_cap,
    })
}

fn prompt_template_provider_profiles(
    theme: &ColorfulTheme,
    profiles: &[ProfileChoice],
    slot_count: usize,
) -> Result<Vec<String>> {
    if slot_count == 0 {
        return Ok(Vec::new());
    }

    let enabled = enabled_profile_choices(profiles);
    let items = profile_select_items(&enabled);
    let selected = MultiSelect::with_theme(theme)
        .with_prompt(format!(
            "Select up to {slot_count} provider profile(s) in slot order (blank selection uses defaults)"
        ))
        .items(&items)
        .interact()?;

    let mut resolved = Vec::new();
    for index in selected {
        if resolved.len() >= slot_count {
            break;
        }
        if index < enabled.len() {
            resolved.push(enabled[index].name.clone());
        } else if index == enabled.len() + 1 {
            loop {
                let manual: String = Input::with_theme(theme)
                    .with_prompt("Profile name")
                    .interact_text()?;
                match classify_manual_profile_name(&manual, profiles) {
                    ManualProfileEntry::DefaultRouting => break,
                    ManualProfileEntry::Accepted(name) => {
                        resolved.push(name);
                        break;
                    }
                    ManualProfileEntry::KnownDisabled => {
                        eprintln!(
                            "Profile '{manual}' is disabled; choose an enabled profile or enter another name."
                        );
                    }
                }
            }
        }
    }
    Ok(resolved)
}

fn prompt_optional_text_with_entry(theme: &ColorfulTheme, prompt: &str) -> Result<Option<String>> {
    let editor_env = std::env::var("EDITOR").ok();
    let editor_label = editor_env.as_deref().unwrap_or("$EDITOR");
    let open_editor = format!("Open {editor_label}");
    let labels = ["Write inline", open_editor.as_str()];
    let selected = Select::with_theme(theme)
        .with_prompt(format!("{prompt} entry method"))
        .items(labels)
        .default(0)
        .interact()?;

    if selected == 0 {
        let value: String = Input::with_theme(theme)
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()?;
        if value.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(value));
    }

    match Editor::new().extension("txt").edit("") {
        Ok(Some(text)) if !text.trim().is_empty() => Ok(Some(text)),
        Ok(_) => {
            let value: String = Input::with_theme(theme)
                .with_prompt(prompt)
                .allow_empty(true)
                .interact_text()?;
            if value.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(value))
            }
        }
        Err(_) => {
            eprintln!("No editor available; falling back to inline input.");
            let value: String = Input::with_theme(theme)
                .with_prompt(prompt)
                .allow_empty(true)
                .interact_text()?;
            if value.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(value))
            }
        }
    }
}

fn interactive_editor_loop(
    theme: &ColorfulTheme,
    mut state: EditorState,
    profiles: &[ProfileChoice],
    persist: &EditorPersistTarget,
) -> Result<NestedEditorOutcome> {
    loop {
        let (header, node_line) = editor_summary_header(&state);
        eprintln!("{header}");
        if !node_line.is_empty() {
            eprintln!("{node_line}");
        }

        let actions = editor_actions(state.depth);
        let labels: Vec<&str> = actions.iter().map(|(label, _)| *label).collect();
        let selected_action = Select::with_theme(theme)
            .with_prompt("Choose an action")
            .items(labels)
            .default(0)
            .interact()?;
        let action = actions[selected_action].1;

        match action {
            EditorAction::AddNode => {
                state = prompt_add_node(theme, &state, profiles)?;
            }
            EditorAction::EditNode => {
                state = prompt_edit_node(theme, &state, profiles)?;
            }
            EditorAction::RemoveNode => {
                state = prompt_remove_node(theme, &state)?;
            }
            EditorAction::ManageEdges => {
                prompt_manage_edges(theme, &mut state)?;
            }
            EditorAction::GraphSettings => {
                let settings = prompt_graph_settings_with_defaults(theme, &state.graph)?;
                state = apply_editor_settings(&state, settings);
            }
            EditorAction::Preview => {
                show_editor_preview(&state);
            }
            EditorAction::SaveAndFinish => match validate_for_save(&state) {
                Ok(()) => match &persist {
                    EditorPersistTarget::None => {
                        return Ok(NestedEditorOutcome::Saved(Box::new(state.graph)));
                    }
                    target => match try_persist_editor_graph(&state.graph, target) {
                        Ok(()) => return Ok(NestedEditorOutcome::Saved(Box::new(state.graph))),
                        Err(error) => {
                            eprintln!("Save failed: {error}");
                            eprintln!(
                                "Returning to the editor. Adjust the template name, output path, or graph and try again."
                            );
                        }
                    },
                },
                Err(errors) => {
                    eprintln!("Cannot save yet:");
                    for error in errors {
                        eprintln!("  {error}");
                    }
                }
            },
            EditorAction::Cancel => {
                if Confirm::with_theme(theme)
                    .with_prompt("Discard this graph and exit?")
                    .default(false)
                    .interact()?
                {
                    return Ok(NestedEditorOutcome::Cancelled);
                }
            }
        }
    }
}

fn prompt_add_node(
    theme: &ColorfulTheme,
    state: &EditorState,
    profiles: &[ProfileChoice],
) -> Result<EditorState> {
    let node_actions = wizard_actions(state.depth);
    let labels: Vec<&str> = node_actions.iter().map(|(label, _)| *label).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Add node kind")
        .items(labels)
        .default(0)
        .interact()?;
    let action = node_actions[selected].1;

    let existing_ids: Vec<&str> = state
        .graph
        .spec
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect();
    let id = prompt_identifier(theme, "Node ID", None, &existing_ids)?;
    let Some(mut node) = build_node_for_action(theme, action, &id, state.depth, profiles)? else {
        return Ok(state.clone());
    };

    let common_settings =
        prompt_common_node_settings(theme, &node, &state.graph.spec.nodes, profiles)?;
    if let Err(error) = apply_common_node_settings(&mut node, common_settings) {
        eprintln!("{error}");
        return Ok(state.clone());
    }
    if let Some(node_output) = node_output_mut(&mut node) {
        let output = prompt_output_spec(theme)?;
        *node_output = output;
    }

    let inherit_source = match &node.workspace {
        WorkspaceSpec::Inherit { node } => Some(node.as_str()),
        _ => None,
    };
    let dependency_ids: Vec<&str> = state
        .graph
        .spec
        .nodes
        .iter()
        .map(|candidate| candidate.id.as_str())
        .filter(|candidate| Some(*candidate) != inherit_source)
        .collect();
    let selected_deps = if dependency_ids.is_empty() {
        Vec::new()
    } else {
        MultiSelect::with_theme(theme)
            .with_prompt("Pick dependency edges")
            .items(&dependency_ids)
            .interact()?
    };
    let dependency_drafts = if selected_deps.is_empty() {
        Vec::new()
    } else {
        select_dependency_drafts(theme, &dependency_ids, &selected_deps, &id)?
    };

    match add_node_to_editor(
        state,
        node,
        &dependency_ids,
        &selected_deps,
        &dependency_drafts,
    ) {
        Ok(next) => {
            eprintln!("Added node {id}.");
            Ok(next)
        }
        Err(error) => {
            eprintln!("{error}");
            Ok(state.clone())
        }
    }
}

fn prompt_edit_node(
    theme: &ColorfulTheme,
    state: &EditorState,
    profiles: &[ProfileChoice],
) -> Result<EditorState> {
    if state.graph.spec.nodes.is_empty() {
        eprintln!("No nodes to edit.");
        return Ok(state.clone());
    }
    let ids: Vec<&str> = state
        .graph
        .spec
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Node to edit")
        .items(&ids)
        .default(0)
        .interact()?;
    let node_id = ids[selected];
    let existing = state
        .graph
        .spec
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .expect("selected node exists")
        .clone();

    let mut node = edit_node_for_kind(theme, &existing, state.depth, profiles)?;
    preserve_unprompted_node_fields(&existing, &mut node);
    let prior_nodes: Vec<Node> = state
        .graph
        .spec
        .nodes
        .iter()
        .filter(|candidate| candidate.id != node_id)
        .cloned()
        .collect();
    let common_settings = prompt_common_node_settings_with_defaults(
        theme,
        &node,
        &prior_nodes,
        Some(&existing),
        profiles,
    )?;
    apply_common_node_settings(&mut node, common_settings)?;
    if let Some(node_output) = node_output_mut(&mut node) {
        let output = prompt_output_spec_with_defaults(theme, existing.output())?;
        *node_output = output;
    }

    match replace_node_in_editor(state, node_id, node) {
        Ok(next) => {
            eprintln!("Updated node {node_id}.");
            Ok(next)
        }
        Err(error) => {
            eprintln!("{error}");
            Ok(state.clone())
        }
    }
}

fn preserve_unprompted_node_fields(existing: &Node, edited: &mut Node) {
    edited.label.clone_from(&existing.label);
    edited.requires.clone_from(&existing.requires);
    edited.continue_on_failure = existing.continue_on_failure;

    match (&existing.kind, &mut edited.kind) {
        (
            NodeKind::Command { env, .. },
            NodeKind::Command {
                env: edited_env, ..
            },
        )
        | (
            NodeKind::Verify { env, .. },
            NodeKind::Verify {
                env: edited_env, ..
            },
        ) => {
            edited_env.clone_from(env);
        }
        (
            NodeKind::Gate { default, .. },
            NodeKind::Gate {
                default: edited_default,
                ..
            },
        ) => {
            *edited_default = *default;
        }
        _ => {}
    }
}

fn prompt_remove_node(theme: &ColorfulTheme, state: &EditorState) -> Result<EditorState> {
    if state.graph.spec.nodes.is_empty() {
        eprintln!("No nodes to remove.");
        return Ok(state.clone());
    }
    let ids: Vec<&str> = state
        .graph
        .spec
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Node to remove")
        .items(&ids)
        .default(0)
        .interact()?;
    let node_id = ids[selected];
    if !Confirm::with_theme(theme)
        .with_prompt(format!("Remove node '{node_id}' and its edges?"))
        .default(false)
        .interact()?
    {
        return Ok(state.clone());
    }
    let next = remove_node_from_editor(state, node_id);
    match next {
        Ok(state) => {
            eprintln!("Removed node {node_id}.");
            Ok(state)
        }
        Err(error) => {
            eprintln!("{error}");
            Ok(state.clone())
        }
    }
}

fn prompt_manage_edges(theme: &ColorfulTheme, state: &mut EditorState) -> Result<()> {
    loop {
        let action = Select::with_theme(theme)
            .with_prompt("Manage edges")
            .items(["Add edge", "Remove edge", "Back"])
            .default(2)
            .interact()?;
        match action {
            0 => {
                if state.graph.spec.nodes.len() < 2 {
                    eprintln!("At least two nodes are required to add an edge.");
                    continue;
                }
                let ids: Vec<String> = state
                    .graph
                    .spec
                    .nodes
                    .iter()
                    .map(|node| node.id.clone())
                    .collect();
                let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                let from_selected = Select::with_theme(theme)
                    .with_prompt("Edge from")
                    .items(&id_refs)
                    .default(0)
                    .interact()?;
                let from = ids[from_selected].clone();
                let to_selected = Select::with_theme(theme)
                    .with_prompt("Edge to")
                    .items(&id_refs)
                    .default(ids.len().saturating_sub(1))
                    .interact()?;
                let to = ids[to_selected].clone();
                let kind = select_edge_kind(theme, &from, &to)?;
                let when = if matches!(kind, EdgeKind::Conditional) {
                    Some(select_conditional_when(theme, &from, &to)?)
                } else {
                    None
                };
                match add_edge_to_editor(
                    state,
                    Edge {
                        from: from.clone(),
                        to: to.clone(),
                        kind,
                        when,
                    },
                ) {
                    Ok(next) => {
                        *state = next;
                        eprintln!("Added edge {from} -> {to}.");
                    }
                    Err(error) => eprintln!("{error}"),
                }
            }
            1 => {
                if state.graph.spec.edges.is_empty() {
                    eprintln!("No edges to remove.");
                    continue;
                }
                let labels: Vec<String> = state
                    .graph
                    .spec
                    .edges
                    .iter()
                    .map(|edge| format!("{} -{:?}-> {}", edge.from, edge.kind, edge.to))
                    .collect();
                let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let selected = Select::with_theme(theme)
                    .with_prompt("Edge to remove")
                    .items(&label_refs)
                    .default(0)
                    .interact()?;
                let edge = state.graph.spec.edges[selected].clone();
                match remove_edge_from_editor(&*state, &edge.from, &edge.to, edge.kind) {
                    Ok(next) => {
                        *state = next;
                        eprintln!("Removed edge {} -> {}.", edge.from, edge.to);
                    }
                    Err(error) => eprintln!("{error}"),
                }
            }
            _ => break,
        }
    }
    Ok(())
}

fn select_edge_kind(theme: &ColorfulTheme, from: &str, to: &str) -> Result<EdgeKind> {
    let labels = [
        "data — predecessor output feeds downstream context",
        "control — ordering only, no data transfer",
        "resource — serialize access to a shared resource",
        "conditional — activate when a terminal condition matches",
        "failure — propagate failure to downstream nodes",
    ];
    let selected = Select::with_theme(theme)
        .with_prompt(format!("Edge kind from '{from}' to '{to}'"))
        .items(labels)
        .default(0)
        .interact()?;
    Ok(match selected {
        0 => EdgeKind::Data,
        1 => EdgeKind::Control,
        2 => EdgeKind::Resource,
        3 => EdgeKind::Conditional,
        4 => EdgeKind::Failure,
        _ => unreachable!("invalid edge kind selection"),
    })
}

fn show_editor_preview(state: &EditorState) {
    match state.graph.compile() {
        Ok(compiled) => eprintln!("{}", compiled.render_mermaid()),
        Err(error) => eprintln!("Mermaid preview unavailable: {error}"),
    }
    let errors = validation_errors(state);
    let warnings = validation_warnings(state);
    if !errors.is_empty() {
        eprintln!("Validation errors:");
        for error in errors {
            eprintln!("  {error}");
        }
    }
    if !warnings.is_empty() {
        eprintln!("Validation warnings:");
        for warning in warnings {
            eprintln!("  {warning}");
        }
    }
}

fn prompt_graph_settings_with_defaults(
    theme: &ColorfulTheme,
    graph: &Graph,
) -> Result<GraphSettings> {
    let goal: String = Input::with_theme(theme)
        .with_prompt("Graph goal")
        .default(graph.spec.goal.clone())
        .interact_text()?;
    let max_parallel = prompt_bounded_number(
        theme,
        "Maximum parallel nodes",
        graph.spec.policies.max_parallel,
        1,
        MAX_PARALLELISM,
    )?;
    let failure_default = match graph.spec.policies.failure {
        FailurePolicy::FailFast => 0,
        FailurePolicy::Continue => 1,
    };
    let failure = match Select::with_theme(theme)
        .with_prompt("Failure policy")
        .items(["fail_fast", "continue"])
        .default(failure_default)
        .interact()?
    {
        0 => FailurePolicy::FailFast,
        1 => FailurePolicy::Continue,
        _ => unreachable!("invalid failure policy selection"),
    };
    let wall_time_seconds = prompt_optional_number_with_default(
        theme,
        "Optional wall-time budget in seconds",
        graph.spec.budgets.wall_time_seconds,
        0u64,
        MAX_DURATION_SECONDS,
    )?;
    let model_calls = prompt_optional_number_with_default(
        theme,
        "Optional model-call budget",
        graph.spec.budgets.model_calls,
        0u32,
        u32::MAX,
    )?;

    Ok(GraphSettings {
        max_parallel,
        failure,
        budgets: RunBudgets {
            wall_time_seconds,
            model_calls,
        },
        goal: Some(goal),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorAction {
    AddNode,
    EditNode,
    RemoveNode,
    ManageEdges,
    GraphSettings,
    Preview,
    SaveAndFinish,
    Cancel,
}

fn editor_actions(_depth: usize) -> Vec<(&'static str, EditorAction)> {
    vec![
        ("Add node", EditorAction::AddNode),
        ("Edit node", EditorAction::EditNode),
        ("Remove node", EditorAction::RemoveNode),
        ("Manage edges", EditorAction::ManageEdges),
        ("Graph settings", EditorAction::GraphSettings),
        ("Preview", EditorAction::Preview),
        ("Save and finish", EditorAction::SaveAndFinish),
        ("Cancel", EditorAction::Cancel),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardAction {
    Agent,
    Command,
    Verify,
    Gate,
    Reduce,
    Synthesize,
    Loop,
    Subgraph,
}

fn wizard_actions(depth: usize) -> Vec<(&'static str, WizardAction)> {
    let mut actions = vec![
        ("Add agent node", WizardAction::Agent),
        ("Add command node", WizardAction::Command),
        ("Add verify node", WizardAction::Verify),
        ("Add gate node", WizardAction::Gate),
        ("Add reduce node", WizardAction::Reduce),
        ("Add synthesize node", WizardAction::Synthesize),
    ];
    if depth < INTERACTIVE_NESTING_LIMIT {
        actions.push(("Add bounded loop node", WizardAction::Loop));
        actions.push(("Add subgraph node", WizardAction::Subgraph));
    }
    actions
}

fn build_node_for_action(
    theme: &ColorfulTheme,
    action: WizardAction,
    id: &str,
    depth: usize,
    profiles: &[ProfileChoice],
) -> Result<Option<Node>> {
    match action {
        WizardAction::Agent => build_agent_node(theme, id, profiles).map(Some),
        WizardAction::Command => build_command_node(theme, id).map(Some),
        WizardAction::Verify => build_verify_node(theme, id).map(Some),
        WizardAction::Gate => build_gate_node(theme, id).map(Some),
        WizardAction::Reduce => build_reduce_node(theme, id, profiles).map(Some),
        WizardAction::Synthesize => build_synthesize_node(theme, id, profiles).map(Some),
        WizardAction::Loop => build_loop_node(theme, id, depth, profiles),
        WizardAction::Subgraph => build_subgraph_node(theme, id, depth, profiles),
    }
}

#[allow(clippy::too_many_lines)]
fn edit_node_for_kind(
    theme: &ColorfulTheme,
    existing: &Node,
    _depth: usize,
    profiles: &[ProfileChoice],
) -> Result<Node> {
    match &existing.kind {
        NodeKind::Agent {
            prompt,
            profile,
            model,
            fan_out,
            ..
        } => {
            let prompt_text = prompt_inline_text(prompt)?;
            let prompt_text = prompt_prompt_text(
                theme,
                &format!("Prompt for agent node '{}'", existing.id),
                Some(&prompt_text),
            )?;
            let fan_out = prompt_bounded_number(theme, "Fan out", *fan_out, 1, MAX_FAN_OUT)?;
            let (profile, model) = prompt_profile_model(
                theme,
                &existing.id,
                "agent",
                profile.as_deref(),
                model.as_deref(),
                profiles,
            )?;
            let mut node = agent_node(&existing.id, &prompt_text, profile, fan_out);
            if let NodeKind::Agent {
                model: node_model, ..
            } = &mut node.kind
            {
                *node_model = model;
            }
            Ok(node)
        }
        NodeKind::Reduce {
            prompt,
            profile,
            model,
            ..
        } => {
            let prompt_text = prompt_inline_text(prompt)?;
            let prompt_text = prompt_prompt_text(
                theme,
                &format!("Prompt for reduce node '{}'", existing.id),
                Some(&prompt_text),
            )?;
            let (profile, model) = prompt_profile_model(
                theme,
                &existing.id,
                "reduce",
                profile.as_deref(),
                model.as_deref(),
                profiles,
            )?;
            let mut node = reduce_node(&existing.id, &prompt_text, profile);
            if let NodeKind::Reduce {
                model: node_model, ..
            } = &mut node.kind
            {
                *node_model = model;
            }
            Ok(node)
        }
        NodeKind::Synthesize {
            prompt,
            profile,
            model,
            ..
        } => {
            let prompt_text = prompt_inline_text(prompt)?;
            let prompt_text = prompt_prompt_text(
                theme,
                &format!("Prompt for synthesize node '{}'", existing.id),
                Some(&prompt_text),
            )?;
            let (profile, model) = prompt_profile_model(
                theme,
                &existing.id,
                "synthesize",
                profile.as_deref(),
                model.as_deref(),
                profiles,
            )?;
            let mut node = synthesize_node(&existing.id, &prompt_text, profile);
            if let NodeKind::Synthesize {
                model: node_model, ..
            } = &mut node.kind
            {
                *node_model = model;
            }
            Ok(node)
        }
        NodeKind::Command { argv, .. } => Ok(command_node(
            &existing.id,
            prompt_command_argv_with_default(theme, &existing.id, argv)?,
        )),
        NodeKind::Verify { argv, .. } => Ok(verify_node(
            &existing.id,
            prompt_command_argv_with_default(theme, &existing.id, argv)?,
        )),
        NodeKind::Gate { message, .. } => Ok(gate_node(
            &existing.id,
            &prompt_prompt_text(
                theme,
                &format!("Approval prompt for '{}'", existing.id),
                Some(message),
            )?,
        )),
        NodeKind::Loop { .. } | NodeKind::Subgraph { .. } => {
            eprintln!(
                "Loop and subgraph nodes cannot be edited here; remove and re-add to change them."
            );
            Ok(existing.clone())
        }
    }
}

fn prompt_identifier(
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<&str>,
    existing: &[&str],
) -> Result<String> {
    loop {
        let mut input = Input::with_theme(theme).with_prompt(format!(
            "{prompt} (lowercase, digits, -/_; must start with a letter)"
        ));
        if let Some(default) = default {
            input = input.default(default.to_owned());
        }
        let candidate: String = input.interact_text()?;
        if !is_valid_identifier(&candidate) {
            eprintln!(
                "Invalid identifier. Use lowercase letters, digits, '-' or '_' and start with a letter."
            );
            continue;
        }
        if existing.contains(&candidate.as_str()) {
            eprintln!("That identifier is already in use.");
            continue;
        }
        return Ok(candidate);
    }
}

fn prompt_nonempty_text(
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<&str>,
) -> Result<String> {
    loop {
        let mut input = Input::with_theme(theme).with_prompt(prompt);
        if let Some(default) = default {
            input = input.default(default.to_owned());
        }
        let value: String = input.interact_text()?;
        if value.trim().is_empty() {
            eprintln!("{prompt} cannot be empty.");
            continue;
        }
        return Ok(value);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphSettings {
    max_parallel: usize,
    failure: FailurePolicy,
    budgets: RunBudgets,
    goal: Option<String>,
}

fn apply_graph_settings(graph: &mut Graph, settings: GraphSettings) {
    graph.spec.policies.max_parallel = settings.max_parallel;
    graph.spec.policies.failure = settings.failure;
    graph.spec.budgets = settings.budgets;
    if let Some(goal) = settings.goal {
        graph.spec.goal = goal;
    }
}

fn prompt_bounded_number<T>(
    theme: &ColorfulTheme,
    prompt: &str,
    default: T,
    minimum: T,
    maximum: T,
) -> Result<T>
where
    T: Copy + PartialOrd + std::fmt::Display + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    loop {
        let value: T = Input::with_theme(theme)
            .with_prompt(prompt)
            .default(default)
            .interact_text()?;
        if (minimum..=maximum).contains(&value) {
            return Ok(value);
        }
        eprintln!("Value must be between {minimum} and {maximum}.");
    }
}

fn prompt_optional_number<T>(
    theme: &ColorfulTheme,
    prompt: &str,
    minimum: T,
    maximum: T,
) -> Result<Option<T>>
where
    T: Copy + PartialOrd + std::fmt::Display + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    loop {
        let value: String = Input::with_theme(theme)
            .with_prompt(format!("{prompt} (blank for none)"))
            .allow_empty(true)
            .interact_text()?;
        if value.trim().is_empty() {
            return Ok(None);
        }
        match value.trim().parse::<T>() {
            Ok(value) if (minimum..=maximum).contains(&value) => return Ok(Some(value)),
            Ok(_) => eprintln!("Value must be between {minimum} and {maximum}."),
            Err(error) => eprintln!("Invalid number: {error}"),
        }
    }
}

fn validate_graph_errors(graph: &Graph) -> Result<()> {
    for issue in graph.validate() {
        if issue.severity == IssueSeverity::Error {
            return Err(anyhow!(
                "graph validation failed [{}] at {}: {}",
                issue.code,
                issue.path,
                issue.message
            ));
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct DependencyDraft {
    kind: EdgeKind,
    when: Option<EdgeCondition>,
}

fn select_dependency_drafts(
    theme: &ColorfulTheme,
    dependency_ids: &[&str],
    selected_indices: &[usize],
    to_node: &str,
) -> Result<Vec<DependencyDraft>> {
    let kind_options = ["data", "control", "resource", "failure", "conditional"];
    let mut results = Vec::with_capacity(selected_indices.len());

    for &index in selected_indices {
        let dependency_id = dependency_ids
            .get(index)
            .ok_or_else(|| anyhow!("invalid dependency selection"))?;
        let kind = match Select::with_theme(theme)
            .with_prompt(format!(
                "Dependency edge kind from '{dependency_id}' to '{to_node}'"
            ))
            .items(kind_options)
            .default(0)
            .interact()?
        {
            0 => EdgeKind::Data,
            1 => EdgeKind::Control,
            2 => EdgeKind::Resource,
            3 => EdgeKind::Failure,
            4 => EdgeKind::Conditional,
            _ => unreachable!("invalid edge kind selection"),
        };

        let when = if matches!(kind, EdgeKind::Conditional) {
            Some(select_conditional_when(theme, dependency_id, to_node)?)
        } else {
            None
        };

        results.push(DependencyDraft { kind, when });
    }

    Ok(results)
}

fn select_terminal_status(theme: &ColorfulTheme, from: &str, to_node: &str) -> Result<NodeStatus> {
    let options = ["succeeded", "failed", "skipped", "blocked", "cancelled"];
    let status = Select::with_theme(theme)
        .with_prompt(format!(
            "Terminal status condition for '{from}' -> '{to_node}'"
        ))
        .items(options)
        .default(0)
        .interact()?;

    Ok(match status {
        0 => NodeStatus::Succeeded,
        1 => NodeStatus::Failed,
        2 => NodeStatus::Skipped,
        3 => NodeStatus::Blocked,
        4 => NodeStatus::Cancelled,
        _ => unreachable!("invalid status selection"),
    })
}

fn select_loop_terminal_status(
    theme: &ColorfulTheme,
    from: &str,
    loop_node: &str,
) -> Result<NodeStatus> {
    let options = ["succeeded", "skipped"];
    let status = Select::with_theme(theme)
        .with_prompt(format!(
            "Successful terminal status for '{from}' -> loop '{loop_node}'"
        ))
        .items(options)
        .default(0)
        .interact()?;

    Ok(match status {
        0 => NodeStatus::Succeeded,
        1 => NodeStatus::Skipped,
        _ => unreachable!("invalid loop terminal status selection"),
    })
}

fn select_conditional_when(
    theme: &ColorfulTheme,
    dependency_id: &str,
    to_node: &str,
) -> Result<EdgeCondition> {
    let status = select_terminal_status(theme, dependency_id, to_node)?;
    let (output_contains, json_pointer, equals) = prompt_condition_filters(theme)?;

    Ok(EdgeCondition {
        status: Some(status),
        output_contains,
        json_pointer,
        equals,
    })
}

fn prompt_condition_filters(
    theme: &ColorfulTheme,
) -> Result<(Option<String>, Option<String>, Option<Value>)> {
    let output_contains = if Confirm::with_theme(theme)
        .with_prompt("Add output_contains condition?")
        .default(false)
        .interact()?
    {
        Some(prompt_nonempty_text(theme, "Enter output_contains", None)?)
    } else {
        None
    };

    let json_pointer = if Confirm::with_theme(theme)
        .with_prompt("Add json_pointer + equals condition?")
        .default(false)
        .interact()?
    {
        let pointer = loop {
            let pointer: String = Input::with_theme(theme)
                .with_prompt("JSON Pointer (blank selects the document root)")
                .allow_empty(true)
                .interact_text()?;
            if is_valid_json_pointer(&pointer) {
                break pointer;
            }
            eprintln!(
                "Invalid RFC 6901 JSON Pointer. Use '' or a '/'-prefixed pointer with ~0/~1 escapes."
            );
        };
        let equals = loop {
            let literal: String = Input::with_theme(theme)
                .with_prompt("Expected JSON literal (for strings include quotes)")
                .allow_empty(true)
                .interact_text()?;
            match parse_json_literal(&literal) {
                Ok(value) => break value,
                Err(error) => eprintln!("Invalid JSON literal: {error}"),
            }
        };
        Some((pointer, equals))
    } else {
        None
    };

    Ok((
        output_contains,
        json_pointer.as_ref().map(|value| value.0.clone()),
        json_pointer.as_ref().map(|value| value.1.clone()),
    ))
}

fn parse_json_literal(input: &str) -> Result<Value> {
    serde_json::from_str(input).context("expected one JSON value")
}

fn is_valid_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') {
        return false;
    }

    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            let Some(escaped) = bytes.get(index + 1) else {
                return false;
            };
            if !matches!(escaped, b'0' | b'1') {
                return false;
            }
            index += 1;
        }
        index += 1;
    }
    true
}

fn build_dependency_edges(
    dependency_ids: &[&str],
    node_id: &str,
    selected_indices: &[usize],
    drafts: &[DependencyDraft],
) -> Result<Vec<Edge>> {
    if selected_indices.len() != drafts.len() {
        return Err(anyhow!("dependency draft mismatch"));
    }

    let mut edges = Vec::with_capacity(selected_indices.len());
    for (&index, draft) in std::iter::zip(selected_indices, drafts.iter()) {
        let from = dependency_ids
            .get(index)
            .ok_or_else(|| anyhow!("invalid dependency index"))?;

        if let EdgeKind::Conditional = draft.kind {
            let condition = draft
                .when
                .as_ref()
                .ok_or_else(|| anyhow!("conditional edges require a condition"))?;

            if !condition.status.is_some_and(NodeStatus::is_terminal) {
                return Err(anyhow!("conditional edges require a terminal status"));
            }

            if condition.equals.is_some() != condition.json_pointer.is_some() {
                return Err(anyhow!(
                    "json_pointer and equals must be specified together"
                ));
            }
            if let Some(pointer) = condition.json_pointer.as_deref()
                && !is_valid_json_pointer(pointer)
            {
                return Err(anyhow!("invalid json_pointer format"));
            }
        } else if draft.when.is_some() {
            return Err(anyhow!("condition can only be used with conditional edges"));
        }

        edges.push(Edge {
            from: (*from).to_owned(),
            to: node_id.to_owned(),
            kind: draft.kind,
            when: draft.when.clone(),
        });
    }

    Ok(edges)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommonNodeSettings {
    resources: Vec<String>,
    retry: RetryPolicy,
    timeout_seconds: Option<u64>,
    workspace: WorkspaceSpec,
    context: ContextSpec,
}

fn prompt_common_node_settings(
    theme: &ColorfulTheme,
    node: &Node,
    prior_nodes: &[Node],
    profiles: &[ProfileChoice],
) -> Result<CommonNodeSettings> {
    prompt_common_node_settings_with_defaults(theme, node, prior_nodes, None, profiles)
}

fn prompt_common_node_settings_with_defaults(
    theme: &ColorfulTheme,
    node: &Node,
    prior_nodes: &[Node],
    existing: Option<&Node>,
    profiles: &[ProfileChoice],
) -> Result<CommonNodeSettings> {
    let _ = profiles;
    let defaults = existing.unwrap_or(node);
    let resources = prompt_csv_with_default(
        theme,
        "Resources (comma-separated)",
        &join_csv(&defaults.resources),
    )?;
    let max_attempts = prompt_bounded_number(
        theme,
        "Maximum retry attempts",
        defaults.retry.max_attempts,
        1,
        MAX_RETRY_ATTEMPTS,
    )?;
    let backoff_seconds = prompt_bounded_number(
        theme,
        "Retry backoff in seconds",
        defaults.retry.backoff_seconds,
        0,
        MAX_DURATION_SECONDS,
    )?;
    let rebind_profiles = if node_supports_profiles(node) && max_attempts > 1 {
        let max_rebind_profiles = usize::try_from(max_attempts - 1)
            .context("retry attempt count does not fit this platform")?;
        loop {
            let profiles = prompt_csv_with_default(
                theme,
                "Retry rebind profiles in attempt order (comma-separated)",
                &join_csv(&defaults.retry.rebind_profiles),
            )?;
            if profiles.len() <= max_rebind_profiles {
                break profiles;
            }
            eprintln!(
                "At most {} rebind profile(s) can be used with {max_attempts} attempts.",
                max_attempts - 1
            );
        }
    } else {
        Vec::new()
    };
    let timeout_seconds = prompt_optional_number_with_default(
        theme,
        "Optional node timeout in seconds",
        defaults.timeout_seconds,
        0u64,
        MAX_DURATION_SECONDS,
    )?;
    let workspace = prompt_workspace_with_default(theme, node, prior_nodes, &defaults.workspace)?;
    let include_dependencies = Confirm::with_theme(theme)
        .with_prompt("Include dependency outputs in context?")
        .default(defaults.context.include_dependencies)
        .interact()?;
    let files = prompt_csv_with_default(
        theme,
        "Additional context files (comma-separated)",
        &join_csv_paths(&defaults.context.files),
    )?
    .into_iter()
    .map(PathBuf::from)
    .collect();
    let max_bytes = prompt_bounded_number(
        theme,
        "Maximum context bytes",
        defaults.context.max_bytes,
        0,
        MAX_CONTEXT_BYTES,
    )?;

    Ok(CommonNodeSettings {
        resources,
        retry: RetryPolicy {
            max_attempts,
            backoff_seconds,
            rebind_profiles,
        },
        timeout_seconds,
        workspace,
        context: ContextSpec {
            include_dependencies,
            files,
            max_bytes,
        },
    })
}

fn join_csv(values: &[String]) -> String {
    values.join(", ")
}

fn join_csv_paths(values: &[PathBuf]) -> String {
    values
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

fn prompt_csv_with_default(
    theme: &ColorfulTheme,
    prompt: &str,
    default: &str,
) -> Result<Vec<String>> {
    let value: String = Input::with_theme(theme)
        .with_prompt(prompt)
        .default(default.to_owned())
        .allow_empty(true)
        .interact_text()?;
    Ok(parse_csv(&value))
}

fn prompt_optional_number_with_default<T>(
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<T>,
    minimum: T,
    maximum: T,
) -> Result<Option<T>>
where
    T: Copy + PartialOrd + std::fmt::Display + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    if let Some(current) = default {
        let action = Select::with_theme(theme)
            .with_prompt(format!("{prompt} (current: {current})"))
            .items(["Keep current value", "Change value", "Clear value"])
            .default(0)
            .interact()?;
        let selected_action = match action {
            0 => OptionalNumberAction::Keep,
            1 => OptionalNumberAction::Change,
            _ => OptionalNumberAction::Clear,
        };
        let changed = if selected_action == OptionalNumberAction::Change {
            Some(prompt_bounded_number(
                theme, prompt, current, minimum, maximum,
            )?)
        } else {
            None
        };
        return Ok(resolve_optional_number(
            selected_action,
            Some(current),
            changed,
        ));
    }

    loop {
        let value: String = Input::with_theme(theme)
            .with_prompt(format!("{prompt} (blank for none)"))
            .allow_empty(true)
            .interact_text()?;
        if value.trim().is_empty() {
            return Ok(None);
        }
        match value.trim().parse::<T>() {
            Ok(parsed) if (minimum..=maximum).contains(&parsed) => return Ok(Some(parsed)),
            Ok(_) => eprintln!("Value must be between {minimum} and {maximum}."),
            Err(error) => eprintln!("Invalid number: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalNumberAction {
    Keep,
    Change,
    Clear,
}

pub fn resolve_optional_number<T>(
    action: OptionalNumberAction,
    current: Option<T>,
    changed: Option<T>,
) -> Option<T> {
    match action {
        OptionalNumberAction::Keep => current,
        OptionalNumberAction::Change => changed,
        OptionalNumberAction::Clear => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalModelAction {
    Keep,
    Change,
    Clear,
}

pub fn resolve_optional_model(
    action: OptionalModelAction,
    current: Option<&str>,
    changed: Option<String>,
) -> Option<String> {
    match action {
        OptionalModelAction::Keep => current.map(str::to_owned),
        OptionalModelAction::Change => changed,
        OptionalModelAction::Clear => None,
    }
}

fn parse_csv(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let item = item.to_owned();
        if !values.contains(&item) {
            values.push(item);
        }
    }
    values
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceChoice {
    Current,
    Worktree,
    Inherit,
}

fn prompt_workspace_with_default(
    theme: &ColorfulTheme,
    node: &Node,
    prior_nodes: &[Node],
    default: &WorkspaceSpec,
) -> Result<WorkspaceSpec> {
    let default_choice = match default {
        WorkspaceSpec::Current | WorkspaceSpec::Readonly => WorkspaceChoice::Current,
        WorkspaceSpec::Worktree { .. } => WorkspaceChoice::Worktree,
        WorkspaceSpec::Inherit { .. } => WorkspaceChoice::Inherit,
    };
    let mut choices = vec![("current", WorkspaceChoice::Current)];
    if node.fan_out() == 1 {
        choices.push(("worktree", WorkspaceChoice::Worktree));
    } else {
        eprintln!("Worktree is unavailable for this agent because fan_out must be 1.");
    }
    if !prior_nodes.is_empty() {
        choices.push(("inherit a prior node's workspace", WorkspaceChoice::Inherit));
    }
    let labels: Vec<&str> = choices.iter().map(|(label, _)| *label).collect();
    let default_index = choices
        .iter()
        .position(|(_, choice)| *choice == default_choice)
        .unwrap_or(0);
    let selected = Select::with_theme(theme)
        .with_prompt("Workspace mode")
        .items(labels)
        .default(default_index)
        .interact()?;

    match choices[selected].1 {
        WorkspaceChoice::Current => Ok(WorkspaceSpec::Current),
        WorkspaceChoice::Worktree => {
            let default_base = match default {
                WorkspaceSpec::Worktree { base, .. } => base.clone().unwrap_or_default(),
                _ => String::new(),
            };
            let default_auto_commit = matches!(
                default,
                WorkspaceSpec::Worktree {
                    auto_commit: true,
                    ..
                }
            );
            let base: String = Input::with_theme(theme)
                .with_prompt("Optional worktree base revision (blank uses captured run base)")
                .default(default_base)
                .allow_empty(true)
                .interact_text()?;
            let auto_commit = Confirm::with_theme(theme)
                .with_prompt("Auto-commit successful changes in the retained worktree?")
                .default(default_auto_commit)
                .interact()?;
            Ok(WorkspaceSpec::Worktree {
                base: (!base.trim().is_empty()).then(|| base.trim().to_owned()),
                auto_commit,
            })
        }
        WorkspaceChoice::Inherit => {
            let ids: Vec<&str> = prior_nodes.iter().map(|prior| prior.id.as_str()).collect();
            let default_index = match default {
                WorkspaceSpec::Inherit { node } => ids
                    .iter()
                    .position(|id| *id == node.as_str())
                    .unwrap_or(ids.len().saturating_sub(1)),
                _ => ids.len().saturating_sub(1),
            };
            let selected = Select::with_theme(theme)
                .with_prompt("Prior node whose workspace should be inherited")
                .items(&ids)
                .default(default_index)
                .interact()?;
            eprintln!(
                "A direct success/default data edge from '{}' will be added automatically.",
                ids[selected]
            );
            Ok(WorkspaceSpec::Inherit {
                node: ids[selected].to_owned(),
            })
        }
    }
}

fn node_supports_profiles(node: &Node) -> bool {
    matches!(
        &node.kind,
        NodeKind::Agent { .. } | NodeKind::Reduce { .. } | NodeKind::Synthesize { .. }
    )
}

fn apply_common_node_settings(node: &mut Node, settings: CommonNodeSettings) -> Result<()> {
    if matches!(settings.workspace, WorkspaceSpec::Readonly) {
        return Err(anyhow!(
            "readonly workspaces are not offered because gloop cannot enforce them"
        ));
    }
    if matches!(settings.workspace, WorkspaceSpec::Worktree { .. }) && node.fan_out() != 1 {
        return Err(anyhow!("worktree agent nodes require fan_out = 1"));
    }

    node.resources = settings.resources;
    node.retry = settings.retry;
    node.timeout_seconds = settings.timeout_seconds;
    node.workspace = settings.workspace;
    node.context = settings.context;
    Ok(())
}

fn ensure_workspace_inheritance_edge(node: &Node, edges: &mut Vec<Edge>) -> Result<()> {
    let WorkspaceSpec::Inherit { node: source } = &node.workspace else {
        return Ok(());
    };

    let existing: Vec<&Edge> = edges
        .iter()
        .filter(|edge| edge.from == *source && edge.to == node.id)
        .collect();
    if existing.is_empty() {
        edges.push(Edge::data(source, &node.id));
        return Ok(());
    }
    if existing.iter().any(|edge| edge.kind == EdgeKind::Failure) {
        return Err(anyhow!(
            "workspace inheritance cannot use a failure edge from '{source}'"
        ));
    }
    if existing.iter().any(|edge| {
        edge.when
            .as_ref()
            .is_some_and(|when| when.status != Some(NodeStatus::Succeeded))
    }) {
        return Err(anyhow!(
            "workspace inheritance requires a default or succeeded edge from '{source}'"
        ));
    }
    Ok(())
}

fn prompt_output_spec(theme: &ColorfulTheme) -> Result<OutputSpec> {
    let format = match Select::with_theme(theme)
        .with_prompt("Output format")
        .items(["text", "json"])
        .default(0)
        .interact()?
    {
        0 => OutputFormat::Text,
        1 => OutputFormat::Json,
        _ => unreachable!("invalid output format selection"),
    };
    let max_bytes = prompt_bounded_number(
        theme,
        "Maximum output bytes",
        1024 * 1024,
        0,
        MAX_OUTPUT_BYTES,
    )?;
    Ok(OutputSpec {
        format,
        schema: None,
        inline_schema: None,
        max_bytes,
    })
}

fn node_output_mut(node: &mut Node) -> Option<&mut OutputSpec> {
    match &mut node.kind {
        NodeKind::Agent { output, .. }
        | NodeKind::Command { output, .. }
        | NodeKind::Reduce { output, .. }
        | NodeKind::Verify { output, .. }
        | NodeKind::Synthesize { output, .. } => Some(output),
        NodeKind::Gate { .. } | NodeKind::Loop { .. } | NodeKind::Subgraph { .. } => None,
    }
}

fn prompt_output_spec_with_defaults(
    theme: &ColorfulTheme,
    existing: Option<&OutputSpec>,
) -> Result<OutputSpec> {
    let default_format = existing.map_or(0, |value| match value.format {
        OutputFormat::Text => 0,
        OutputFormat::Json => 1,
    });
    let format = match Select::with_theme(theme)
        .with_prompt("Output format")
        .items(["text", "json"])
        .default(default_format)
        .interact()?
    {
        0 => OutputFormat::Text,
        1 => OutputFormat::Json,
        _ => unreachable!("invalid output format selection"),
    };
    let max_bytes = prompt_bounded_number(
        theme,
        "Maximum output bytes",
        existing.map_or(1024 * 1024, |value| value.max_bytes),
        0,
        MAX_OUTPUT_BYTES,
    )?;
    Ok(merge_output_spec_edits(existing, format, max_bytes))
}

pub fn merge_output_spec_edits(
    existing: Option<&OutputSpec>,
    format: OutputFormat,
    max_bytes: usize,
) -> OutputSpec {
    OutputSpec {
        format,
        schema: existing.and_then(|value| value.schema.clone()),
        inline_schema: existing.and_then(|value| value.inline_schema.clone()),
        max_bytes,
    }
}

fn prompt_prompt_text(
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<&str>,
) -> Result<String> {
    let editor_env = std::env::var("EDITOR").ok();
    let editor_label = editor_env.as_deref().unwrap_or("$EDITOR");
    let open_editor = format!("Open {editor_label}");
    let labels = ["Write inline", open_editor.as_str()];
    let selected = Select::with_theme(theme)
        .with_prompt(format!("{prompt} entry method"))
        .items(labels)
        .default(0)
        .interact()?;

    if selected == 0 {
        return prompt_nonempty_text(theme, prompt, default);
    }

    let initial = default.unwrap_or("");
    match Editor::new().extension("txt").edit(initial) {
        Ok(Some(text)) if !text.trim().is_empty() => Ok(text),
        Ok(_) => prompt_nonempty_text(theme, prompt, default),
        Err(_) => {
            eprintln!("No editor available; falling back to inline input.");
            prompt_nonempty_text(theme, prompt, default)
        }
    }
}

fn prompt_inline_text(prompt: &PromptSpec) -> Result<String> {
    match prompt {
        PromptSpec::Inline(text) => Ok(text.clone()),
        PromptSpec::Package { .. } => Err(anyhow!(
            "only inline prompts are editable in the wizard (package prompts)"
        )),
    }
}

fn prompt_profile_model(
    theme: &ColorfulTheme,
    id: &str,
    kind: &str,
    default_profile: Option<&str>,
    default_model: Option<&str>,
    profiles: &[ProfileChoice],
) -> Result<(Option<String>, Option<String>)> {
    let enabled = enabled_profile_choices(profiles);
    let items = profile_select_items(&enabled);
    let default_index = profile_default_select_index(&enabled, default_profile);
    let selected = Select::with_theme(theme)
        .with_prompt(format!("{kind} profile for '{id}'"))
        .items(&items)
        .default(default_index)
        .interact()?;

    let profile = match selected.cmp(&enabled.len()) {
        std::cmp::Ordering::Less => Some(enabled[selected].name.clone()),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => loop {
            let manual: String = Input::with_theme(theme)
                .with_prompt("Profile name")
                .default(default_profile.unwrap_or("").to_owned())
                .interact_text()?;
            match classify_manual_profile_name(&manual, profiles) {
                ManualProfileEntry::DefaultRouting => break None,
                ManualProfileEntry::Accepted(name) => break Some(name),
                ManualProfileEntry::KnownDisabled => {
                    eprintln!(
                        "Profile '{manual}' is disabled; choose an enabled profile or enter another name."
                    );
                }
            }
        },
    };

    let model = prompt_optional_model(
        theme,
        id,
        kind,
        default_model,
        profile.as_deref(),
        default_profile,
        profiles,
    )?;

    Ok((profile, model))
}

fn prompt_optional_model(
    theme: &ColorfulTheme,
    id: &str,
    kind: &str,
    default_model: Option<&str>,
    selected_profile: Option<&str>,
    previous_profile: Option<&str>,
    profiles: &[ProfileChoice],
) -> Result<Option<String>> {
    if let Some(current) = default_model {
        let action = Select::with_theme(theme)
            .with_prompt(format!(
                "Optional {kind} model id for '{id}' (current: {current})"
            ))
            .items(["Keep current value", "Change value", "Clear value"])
            .default(0)
            .interact()?;
        let action = match action {
            0 => OptionalModelAction::Keep,
            1 => OptionalModelAction::Change,
            _ => OptionalModelAction::Clear,
        };
        let changed = if action == OptionalModelAction::Change {
            let model_default =
                profile_model_default(selected_profile, previous_profile, Some(current), profiles);
            let model: String = Input::with_theme(theme)
                .with_prompt(format!(
                    "Optional {kind} model id for '{id}' (blank keeps provider default)"
                ))
                .default(model_default)
                .allow_empty(true)
                .interact_text()?;
            if model.trim().is_empty() {
                None
            } else {
                Some(model.trim().to_owned())
            }
        } else {
            None
        };
        return Ok(resolve_optional_model(action, Some(current), changed));
    }

    let model_default =
        profile_model_default(selected_profile, previous_profile, default_model, profiles);
    let model: String = Input::with_theme(theme)
        .with_prompt(format!(
            "Optional {kind} model id for '{id}' (blank keeps provider default)"
        ))
        .default(model_default)
        .allow_empty(true)
        .interact_text()?;
    Ok(if model.trim().is_empty() {
        None
    } else {
        Some(model.trim().to_owned())
    })
}

fn build_agent_node(theme: &ColorfulTheme, id: &str, profiles: &[ProfileChoice]) -> Result<Node> {
    let prompt = prompt_prompt_text(theme, &format!("Prompt for agent node '{id}'"), None)?;
    let fan_out = prompt_bounded_number(theme, "Fan out", 1usize, 1, MAX_FAN_OUT)?;
    let (profile, model) = prompt_profile_model(theme, id, "agent", None, None, profiles)?;

    let mut node = agent_node(id, &prompt, profile, fan_out);
    if let NodeKind::Agent {
        model: node_model, ..
    } = &mut node.kind
    {
        *node_model = model;
    }
    Ok(node)
}

fn build_reduce_node(theme: &ColorfulTheme, id: &str, profiles: &[ProfileChoice]) -> Result<Node> {
    let prompt = prompt_prompt_text(theme, &format!("Prompt for reduce node '{id}'"), None)?;
    let (profile, model) = prompt_profile_model(theme, id, "reduce", None, None, profiles)?;
    let mut node = reduce_node(id, &prompt, profile);
    if let NodeKind::Reduce {
        model: node_model, ..
    } = &mut node.kind
    {
        *node_model = model;
    }
    Ok(node)
}

fn build_synthesize_node(
    theme: &ColorfulTheme,
    id: &str,
    profiles: &[ProfileChoice],
) -> Result<Node> {
    let prompt = prompt_prompt_text(theme, &format!("Prompt for synthesize node '{id}'"), None)?;
    let (profile, model) = prompt_profile_model(theme, id, "synthesize", None, None, profiles)?;
    let mut node = synthesize_node(id, &prompt, profile);
    if let NodeKind::Synthesize {
        model: node_model, ..
    } = &mut node.kind
    {
        *node_model = model;
    }
    Ok(node)
}

fn build_command_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    Ok(command_node(id, prompt_command_argv(theme, id)?))
}

fn build_verify_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    Ok(verify_node(id, prompt_command_argv(theme, id)?))
}

fn prompt_command_argv(theme: &ColorfulTheme, id: &str) -> Result<Vec<String>> {
    let executable = prompt_nonempty_text(theme, &format!("Executable for '{id}'"), None)?;

    loop {
        let args_text: String = Input::with_theme(theme)
            .with_prompt("Arguments (quoted tokens supported, e.g. \"a b\" '--flag')")
            .allow_empty(true)
            .interact_text()?;
        let mut argv = vec![executable.trim().to_owned()];
        if args_text.trim().is_empty() {
            return Ok(argv);
        }
        match parse_argv(&args_text) {
            Ok(arguments) => {
                argv.extend(arguments);
                return Ok(argv);
            }
            Err(error) => eprintln!("Invalid arguments: {error}. Please try again."),
        }
    }
}

fn build_gate_node(theme: &ColorfulTheme, id: &str) -> Result<Node> {
    let message = prompt_prompt_text(theme, &format!("Approval prompt for '{id}'"), None)?;
    Ok(gate_node(id, &message))
}

pub fn shell_quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_owned();
    }
    if arg
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./:".contains(character))
    {
        return arg.to_owned();
    }
    if !arg.contains('\'') {
        return format!("'{arg}'");
    }
    format!(
        "\"{}\"",
        arg.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    )
}

pub fn format_argv_for_shell(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| shell_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn prompt_command_argv_with_default(
    theme: &ColorfulTheme,
    id: &str,
    argv: &[String],
) -> Result<Vec<String>> {
    let executable_default = argv.first().map_or("", String::as_str);
    let args_default = if argv.len() > 1 {
        format_argv_for_shell(&argv[1..])
    } else {
        String::new()
    };
    let executable = prompt_nonempty_text(
        theme,
        &format!("Executable for '{id}'"),
        Some(executable_default),
    )?;

    loop {
        let args_text: String = Input::with_theme(theme)
            .with_prompt("Arguments (quoted tokens supported, e.g. \"a b\" '--flag')")
            .default(args_default.clone())
            .allow_empty(true)
            .interact_text()?;
        let mut parsed_argv = vec![executable.trim().to_owned()];
        if args_text.trim().is_empty() {
            return Ok(parsed_argv);
        }
        match parse_argv(&args_text) {
            Ok(arguments) => {
                parsed_argv.extend(arguments);
                return Ok(parsed_argv);
            }
            Err(error) => eprintln!("Invalid arguments: {error}. Please try again."),
        }
    }
}

fn build_loop_node(
    theme: &ColorfulTheme,
    id: &str,
    depth: usize,
    profiles: &[ProfileChoice],
) -> Result<Option<Node>> {
    let nested_name = format!("{id}-body");
    let nested_goal = format!("Bounded iteration body for {id}");
    let mut nested_state = EditorState::new(&nested_name, &nested_goal);
    nested_state.depth = depth + 1;
    let nested =
        match interactive_editor_loop(theme, nested_state, profiles, &EditorPersistTarget::None)? {
            NestedEditorOutcome::Saved(graph) => *graph,
            NestedEditorOutcome::Cancelled => return Ok(None),
        };
    let nested_ids: Vec<&str> = nested
        .spec
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Nested node used as the loop terminal condition")
        .items(&nested_ids)
        .default(nested_ids.len() - 1)
        .interact()?;
    let condition_node = nested_ids[selected].to_owned();
    let status = select_loop_terminal_status(theme, &condition_node, id)?;
    let (output_contains, json_pointer, equals) = prompt_condition_filters(theme)?;
    let max_iterations = prompt_bounded_number(
        theme,
        "Maximum loop iterations",
        4u32,
        1,
        MAX_LOOP_ITERATIONS,
    )?;
    let stagnation_after = prompt_bounded_number(
        theme,
        "Stop after this many stagnant iterations",
        2u32.min(max_iterations),
        1,
        max_iterations,
    )?;

    Ok(Some(loop_node(
        id,
        nested,
        LoopCondition {
            node: condition_node,
            status,
            output_contains,
            json_pointer,
            equals,
        },
        max_iterations,
        stagnation_after,
    )))
}

fn build_subgraph_node(
    theme: &ColorfulTheme,
    id: &str,
    depth: usize,
    profiles: &[ProfileChoice],
) -> Result<Option<Node>> {
    let nested_name = format!("{id}-graph");
    let nested_goal = format!("Nested workflow for {id}");
    let mut nested_state = EditorState::new(&nested_name, &nested_goal);
    nested_state.depth = depth + 1;
    let nested =
        match interactive_editor_loop(theme, nested_state, profiles, &EditorPersistTarget::None)? {
            NestedEditorOutcome::Saved(graph) => *graph,
            NestedEditorOutcome::Cancelled => return Ok(None),
        };
    Ok(Some(subgraph_node(id, nested)))
}

fn loop_node(
    id: &str,
    graph: Graph,
    until: LoopCondition,
    max_iterations: u32,
    stagnation_after: u32,
) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: RetryPolicy::default(),
        timeout_seconds: None,
        workspace: WorkspaceSpec::Current,
        context: ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Loop {
            graph: Box::new(graph),
            until,
            max_iterations,
            stagnation_after,
        },
    }
}

fn subgraph_node(id: &str, graph: Graph) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: RetryPolicy::default(),
        timeout_seconds: None,
        workspace: WorkspaceSpec::Current,
        context: ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Subgraph {
            graph: Box::new(graph),
        },
    }
}

fn parse_argv(input: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut token = String::new();
    let mut token_started = false;
    let mut state = QuoteState::Out;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match state {
            QuoteState::Out => match ch {
                '\'' => {
                    state = QuoteState::Single;
                    token_started = true;
                }
                '"' => {
                    state = QuoteState::Double;
                    token_started = true;
                }
                '\\' => {
                    let escaped = chars
                        .next()
                        .ok_or_else(|| anyhow!("unterminated escape sequence"))?;
                    token.push(escaped);
                    token_started = true;
                }
                c if c.is_whitespace() => {
                    if token_started {
                        args.push(std::mem::take(&mut token));
                        token_started = false;
                    }
                }
                c => {
                    token.push(c);
                    token_started = true;
                }
            },
            QuoteState::Single => match ch {
                '\'' => state = QuoteState::Out,
                c => token.push(c),
            },
            QuoteState::Double => match ch {
                '"' => state = QuoteState::Out,
                '\\' => {
                    let next = chars
                        .next()
                        .ok_or_else(|| anyhow!("unterminated escape sequence"))?;
                    match next {
                        'n' => token.push('\n'),
                        't' => token.push('\t'),
                        '"' => token.push('"'),
                        '\\' => token.push('\\'),
                        _ => token.push(next),
                    }
                    token_started = true;
                }
                c => token.push(c),
            },
        }
    }

    if !matches!(state, QuoteState::Out) {
        return Err(anyhow!("unterminated quote in command input"));
    }
    if token_started {
        args.push(token);
    }
    if args.is_empty() {
        return Err(anyhow!("command cannot be empty"));
    }

    Ok(args)
}

enum QuoteState {
    Out,
    Single,
    Double,
}

/// Council pattern: two designers answer the same task blind, an integrator
/// merges them into one design, an implementer implements it, three reviewers
/// critique the implementation from independent angles, and a second
/// integrator reconciles the reviews into the final judgment.
fn council_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested task".to_owned());
    let pick = |index: usize| profiles.get(index).cloned();
    let mut graph = Graph::new(
        name,
        goal,
        vec![
            agent_node(
                "design_one",
                &format!(
                    "Design lane 1. Answer this task independently and completely:\n{request}"
                ),
                pick(0),
                1,
            ),
            agent_node(
                "design_two",
                &format!(
                    "Design lane 2. Answer this task independently and completely:\n{request}"
                ),
                pick(1),
                1,
            ),
            synthesize_node(
                "integrate_design",
                &format!(
                    "You receive two independent designs for the same task. Judge their strengths and weaknesses, then merge them into one decisive design:\n{request}"
                ),
                pick(2),
            ),
            agent_node(
                "implement",
                &format!(
                    "Implementer role: implement the integrated design exactly as specified. Return what was implemented and how to verify it:\n{request}"
                ),
                pick(3),
                1,
            ),
            agent_node(
                "review_one",
                "Reviewer role: correctness. Find defects, logic errors, and spec violations in the implementation. List concrete findings only.",
                pick(4),
                1,
            ),
            agent_node(
                "review_two",
                "Reviewer role: robustness. Find edge cases, failure modes, and security problems in the implementation. List concrete findings only.",
                pick(5),
                1,
            ),
            agent_node(
                "review_three",
                "Reviewer role: maintainability. Find readability, structure, and testing gaps in the implementation. List concrete findings only.",
                pick(6),
                1,
            ),
            synthesize_node(
                "integrate_review",
                "You receive three independent reviews of the same implementation. Deduplicate findings, judge severity, and produce one reconciled final review with an action list.",
                pick(7),
            ),
        ],
    );
    graph.spec.edges = vec![
        Edge::data("design_one", "integrate_design"),
        Edge::data("design_two", "integrate_design"),
        Edge::data("integrate_design", "implement"),
        Edge::data("implement", "review_one"),
        Edge::data("implement", "review_two"),
        Edge::data("implement", "review_three"),
        Edge::data("review_one", "integrate_review"),
        Edge::data("review_two", "integrate_review"),
        Edge::data("review_three", "integrate_review"),
    ];
    graph.metadata.description =
        Some("council: blind designs, one implementation, panel review".to_owned());
    graph
}

/// Decompose pattern: one model decomposes the task into up to four
/// independent work packages, a bank of lightweight workers executes the
/// package assigned to its lane (idle lanes answer SKIP), and an integrator
/// assembles the final deliverable. Worker lanes are fixed by the graph, so
/// decomposers must stay within four packages.
fn decompose_fanout_reduce_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested task".to_owned());
    let pick = |index: usize| profiles.get(index).cloned();
    let worker_prompt = |lane: usize| {
        format!(
            "Worker lane {lane}. The upstream decomposition lists numbered work packages. Execute only package {lane}; if the decomposition has fewer than {lane} packages, answer exactly SKIP. Return the package result only."
        )
    };
    let mut graph = Graph::new(
        name,
        goal,
        vec![
            agent_node(
                "decompose",
                &format!(
                    "Decomposer role: split this task into at most 4 independent, parallelizable work packages. Number them 1..N, one paragraph each, no overlap:\n{request}"
                ),
                pick(0),
                1,
            ),
            agent_node("worker_one", &worker_prompt(1), pick(1), 1),
            agent_node("worker_two", &worker_prompt(2), pick(2), 1),
            agent_node("worker_three", &worker_prompt(3), pick(3), 1),
            agent_node("worker_four", &worker_prompt(4), pick(4), 1),
            synthesize_node(
                "integrate",
                &format!(
                    "You receive results from up to four worker lanes (lanes without work answer SKIP). Assemble them into one coherent final deliverable for the original task:\n{request}"
                ),
                pick(5),
            ),
        ],
    );
    graph.spec.edges = vec![
        Edge::data("decompose", "worker_one"),
        Edge::data("decompose", "worker_two"),
        Edge::data("decompose", "worker_three"),
        Edge::data("decompose", "worker_four"),
        Edge::data("worker_one", "integrate"),
        Edge::data("worker_two", "integrate"),
        Edge::data("worker_three", "integrate"),
        Edge::data("worker_four", "integrate"),
    ];
    graph.metadata.description =
        Some("decompose into packages, fan out to workers, integrate".to_owned());
    graph
}

/// Implement-test-loop: an implementer works toward the goal, then a bounded
/// loop runs the verification command; when it fails, a fixer consumes the
/// failure details (failure edge) and the loop retries the verification.
/// Replace the placeholder test command with the real suite, e.g.
/// `pnpm run test` or `cargo test --workspace`.
fn implement_test_loop_template(
    name: String,
    goal: String,
    request: Option<String>,
    profiles: &[String],
    loop_cap: Option<u32>,
) -> Graph {
    let request = request.unwrap_or_else(|| "the requested implementation".to_owned());
    let max_iterations = loop_cap.unwrap_or(3);
    let implement_profile = profiles.first().cloned();
    let fix_profile = profiles.get(1).cloned();

    let nested_nodes = vec![
        verify_node(
            "test",
            vec![
                "sh".into(),
                "-c".into(),
                "echo 'TEMPLATE: replace with your test command, e.g. pnpm run test'".into(),
            ],
        ),
        agent_node(
            "fix",
            &format!(
                "Fixer role: the verification command failed. You receive the failure details upstream. Diagnose the root cause, apply the smallest correct fix, and describe exactly what you changed for:\n{request}"
            ),
            fix_profile,
            1,
        ),
    ];
    let mut nested_graph = Graph::new(
        "test-fix-iteration",
        "single verify-fix iteration",
        nested_nodes,
    );
    nested_graph.spec.edges = vec![Edge {
        from: "test".to_owned(),
        to: "fix".to_owned(),
        kind: EdgeKind::Failure,
        when: None,
    }];
    nested_graph.metadata.description = Some("bounded verify/fix iteration".to_owned());

    let loop_node = Node {
        id: "test_fix_loop".to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: RetryPolicy::default(),
        timeout_seconds: None,
        workspace: WorkspaceSpec::default(),
        context: ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Loop {
            graph: Box::new(nested_graph),
            until: LoopCondition {
                node: "test".to_owned(),
                status: NodeStatus::Succeeded,
                output_contains: None,
                json_pointer: None,
                equals: None,
            },
            max_iterations,
            stagnation_after: 2,
        },
    };

    let mut graph = Graph::new(
        name,
        goal,
        vec![
            agent_node(
                "implement",
                &format!(
                    "Implementer role: implement the following so that the verification command passes:\n{request}"
                ),
                implement_profile,
                1,
            ),
            loop_node,
            agent_node(
                "report",
                "Summarize the loop result: what was implemented, which verification iterations ran, and the final state. Be concise.",
                profiles.get(2).cloned(),
                1,
            ),
        ],
    );
    graph
        .spec
        .edges
        .push(Edge::data("implement", "test_fix_loop"));
    graph.spec.edges.push(Edge {
        from: "test_fix_loop".to_owned(),
        to: "report".to_owned(),
        kind: EdgeKind::Control,
        when: None,
    });
    graph.metadata.description =
        Some("implement, then bounded verify/fix loop until tests pass".to_owned());
    graph
}

fn agent_node(id: &str, prompt: &str, profile: Option<String>, fan_out: usize) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Agent {
            prompt: PromptSpec::Inline(prompt.to_owned()),
            profile,
            model: None,
            fan_out,
            output: OutputSpec::default(),
        },
    }
}

fn command_node(id: &str, argv: Vec<String>) -> Node {
    Node::command(id, argv)
}

fn verify_node(id: &str, argv: Vec<String>) -> Node {
    let mut base = command_node(id, argv);
    let (argv, env) = match &mut base.kind {
        NodeKind::Command { argv, env, .. } => (std::mem::take(argv), std::mem::take(env)),
        _ => unreachable!("command_node must return a command node"),
    };

    base.kind = NodeKind::Verify {
        argv,
        env,
        output: OutputSpec::default(),
    };
    base
}

fn reduce_node(id: &str, prompt: &str, profile: Option<String>) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Reduce {
            prompt: PromptSpec::Inline(prompt.to_owned()),
            profile,
            model: None,
            output: OutputSpec::default(),
        },
    }
}

fn synthesize_node(id: &str, prompt: &str, profile: Option<String>) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Synthesize {
            prompt: PromptSpec::Inline(prompt.to_owned()),
            profile,
            model: None,
            output: OutputSpec::default(),
        },
    }
}

fn gate_node(id: &str, message: &str) -> Node {
    Node {
        id: id.to_owned(),
        label: None,
        requires: Vec::new(),
        resources: Vec::new(),
        retry: gloop_core::RetryPolicy::default(),
        timeout_seconds: None,
        workspace: gloop_core::WorkspaceSpec::default(),
        context: gloop_core::ContextSpec::default(),
        continue_on_failure: false,
        kind: NodeKind::Gate {
            message: message.to_owned(),
            default: gloop_core::GateDefault::Reject,
        },
    }
}

fn is_valid_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || character == '_'
        })
        && !value.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        CommonNodeSettings, DependencyDraft, Edge, EdgeCondition, EdgeKind, EditorPersistTarget,
        EditorState, Graph, GraphSettings, GraphTemplate, ManualProfileEntry, NestedEditorOutcome,
        NodeKind, NodeStatus, OptionalModelAction, OptionalNumberAction, ProfileChoice,
        ProfileSource, WizardAction, add_edge_to_editor, add_node_to_editor,
        apply_common_node_settings, apply_graph_settings, build_dependency_edges,
        classify_manual_profile_name, editor_summary_header, enabled_profile_choices,
        ensure_workspace_inheritance_edge, format_argv_for_shell, format_node_summary,
        graph_from_yaml_bytes, is_valid_json_pointer, loop_node, map_profile_selection,
        merge_output_spec_edits, node_output_mut, parse_csv, parse_json_literal,
        preflight_template_destination, profile_choice_label, profile_default_select_index,
        profile_model_default, profile_select_items, remove_edge_from_editor,
        remove_node_from_editor, replace_node_in_editor, resolve_optional_model,
        resolve_optional_number, seed_editor_from_template, shell_quote_arg, subgraph_node,
        template_graph, try_persist_editor_graph, validate_for_save, wizard_actions,
        workspace_inheritance_dependents,
    };
    use super::{parse_argv, synthesize_node};
    use gloop_core::{
        ContextSpec, FailurePolicy, IssueSeverity, LoopCondition, OutputFormat, PromptSpec,
        RetryPolicy, RunBudgets, WorkspaceSpec,
    };
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn parse_argv_with_quotes() {
        let parsed = parse_argv(r#"printf "hello world" 'a b' test"#).expect("parse");
        assert_eq!(
            parsed,
            vec![
                "printf".to_owned(),
                "hello world".to_owned(),
                "a b".to_owned(),
                "test".to_owned(),
            ]
        );
    }

    #[test]
    fn parse_argv_with_escaped_space() {
        let parsed = parse_argv("printf a\\ b").expect("parse");
        assert_eq!(parsed, vec!["printf".to_owned(), "a b".to_owned()]);
    }

    #[test]
    fn parse_argv_with_empty_quoted_argument() {
        let parsed = parse_argv(r#"printf "" "a b""#).expect("parse");
        assert_eq!(
            parsed,
            vec!["printf".to_owned(), String::new(), "a b".to_owned()]
        );
    }

    fn assert_no_profiles(node: &NodeKind) {
        match node {
            NodeKind::Agent { profile, .. } | NodeKind::Reduce { profile, .. } => {
                assert!(profile.is_none());
            }
            NodeKind::Loop { graph, .. } => {
                for nested_node in &graph.spec.nodes {
                    assert_no_profiles(&nested_node.kind);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn template_no_profiles_without_input() {
        let templates = [
            GraphTemplate::Direct,
            GraphTemplate::PlanImplementVerify,
            GraphTemplate::ParallelResearchReduce,
            GraphTemplate::ReviewFixLoop,
        ];

        for template in templates {
            let graph = template_graph("t", "g", template, Some("request".to_owned()), None, None);
            for node in &graph.spec.nodes {
                assert_no_profiles(&node.kind);
            }
        }
    }

    #[test]
    fn builtin_direct_template_without_request_remains_valid() {
        let graph = template_graph("t", "", GraphTemplate::Direct, None, None, None);

        match &graph.spec.nodes[0].kind {
            NodeKind::Agent {
                prompt: PromptSpec::Inline(prompt),
                ..
            } => assert!(prompt.contains("Complete the requested task")),
            _ => panic!("direct template must contain one agent node"),
        }
    }

    #[test]
    fn design_wall_bounce_defaults_to_claude_fable_and_codex_sol() {
        let graph = template_graph(
            "design",
            "design task",
            GraphTemplate::DesignWallBounce,
            Some("design a sync engine".to_owned()),
            None,
            None,
        );

        assert!(
            graph
                .validate()
                .iter()
                .all(|issue| issue.severity != IssueSeverity::Error),
            "wall-bounce template must validate by default"
        );

        let expected = [
            ("design_one", "claude", Some("fable")),
            ("design_two", "codex", Some("gpt-5.6-sol")),
            ("review_by_one", "claude", Some("fable")),
            ("review_by_two", "codex", Some("gpt-5.6-sol")),
            ("revise_one", "claude", Some("fable")),
            ("revise_two", "codex", Some("gpt-5.6-sol")),
            ("final_design", "claude", Some("fable")),
        ];
        assert_eq!(graph.spec.nodes.len(), expected.len());
        for (node, (id, profile, model)) in graph.spec.nodes.iter().zip(expected) {
            assert_eq!(node.id, id);
            match &node.kind {
                NodeKind::Agent {
                    profile: bound,
                    model: bound_model,
                    ..
                }
                | NodeKind::Synthesize {
                    profile: bound,
                    model: bound_model,
                    ..
                } => {
                    assert_eq!(bound.as_deref(), Some(profile));
                    assert_eq!(bound_model.as_deref(), model);
                }
                _ => panic!("{id} must be prompt-based"),
            }
        }

        let edges: Vec<(String, String)> = graph
            .spec
            .edges
            .iter()
            .map(|edge| (edge.from.clone(), edge.to.clone()))
            .collect();
        assert_eq!(
            edges,
            vec![
                ("design_two".to_owned(), "review_by_one".to_owned()),
                ("design_one".to_owned(), "review_by_two".to_owned()),
                ("design_one".to_owned(), "revise_one".to_owned()),
                ("review_by_two".to_owned(), "revise_one".to_owned()),
                ("design_two".to_owned(), "revise_two".to_owned()),
                ("review_by_one".to_owned(), "revise_two".to_owned()),
                ("revise_one".to_owned(), "final_design".to_owned()),
                ("revise_two".to_owned(), "final_design".to_owned()),
            ]
        );
        assert_eq!(graph.spec.budgets.model_calls, Some(7));
    }

    #[test]
    fn design_wall_bounce_skips_model_bindings_for_rebound_lanes() {
        let graph = template_graph(
            "design",
            "design task",
            GraphTemplate::DesignWallBounce,
            Some("design a sync engine".to_owned()),
            Some(vec!["qwen".to_owned(), "opencode".to_owned()]),
            None,
        );
        for node in &graph.spec.nodes {
            match &node.kind {
                NodeKind::Agent { profile, model, .. }
                | NodeKind::Synthesize { profile, model, .. } => {
                    assert!(model.is_none(), "rebound lanes must not pin model aliases");
                    assert!(matches!(profile.as_deref(), Some("qwen" | "opencode")));
                }
                _ => {}
            }
        }
    }

    #[test]
    fn templates_roundtrip_yaml() {
        let examples = [
            "direct.yaml",
            "plan-implement-verify.yaml",
            "parallel-research-reduce.yaml",
            "bounded-loop.yaml",
            "command-only.yaml",
        ];
        let examples_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples");
        for file in examples {
            let content = fs::read_to_string(examples_root.join(file)).expect("example exists");
            let graph = graph_from_yaml_bytes(content).expect("example parses");
            assert!(graph.validate().is_empty());
            let yaml = graph.to_yaml().expect("serialize");
            let reparsed = graph_from_yaml_bytes(yaml).expect("roundtrip parses");
            assert_eq!(graph, reparsed);
        }
    }

    #[test]
    fn dependency_edges_have_declared_kinds() {
        let dependencies = ["research_one", "review", "verify"];
        let selected = [0usize, 2];
        let drafts = vec![
            DependencyDraft {
                kind: EdgeKind::Control,
                when: None,
            },
            DependencyDraft {
                kind: EdgeKind::Failure,
                when: None,
            },
        ];
        let edges = build_dependency_edges(&dependencies, "synthesize", &selected, &drafts)
            .expect("edges should be built");

        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].from, "research_one");
        assert_eq!(edges[0].to, "synthesize");
        assert_eq!(edges[0].kind, EdgeKind::Control);
        assert_eq!(edges[1].from, "verify");
        assert_eq!(edges[1].to, "synthesize");
        assert_eq!(edges[1].kind, EdgeKind::Failure);
        assert!(edges[0].when.is_none());
    }

    #[test]
    fn dependency_edges_reject_wrong_shape() {
        let dependencies = ["a", "b"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Data,
            when: None,
        }];
        assert!(build_dependency_edges(&dependencies, "x", &[5], &drafts).is_err());

        let selected = [0usize, 1usize];
        assert!(build_dependency_edges(&dependencies, "x", &selected, &drafts).is_err());
    }

    #[test]
    fn dependency_edges_reject_conditional_without_when() {
        let dependencies = ["a"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Conditional,
            when: None,
        }];
        assert!(build_dependency_edges(&dependencies, "x", &[0], &drafts).is_err());
    }

    #[test]
    fn dependency_edges_allow_conditional_with_terminal_status() {
        let dependencies = ["a"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: Some("ok".to_owned()),
                json_pointer: Some("/result".to_owned()),
                equals: Some(serde_json::Value::String("true".to_owned())),
            }),
        }];
        let edges = build_dependency_edges(&dependencies, "x", &[0], &drafts)
            .expect("conditional edge should be built");

        assert_eq!(edges.len(), 1);
        let when = edges[0].when.as_ref().expect("when condition exists");
        assert_eq!(when.status, Some(NodeStatus::Succeeded));
    }

    #[test]
    fn graph_validates_conditional_edge_with_terminal_status() {
        let mut nodes = vec![
            super::agent_node("a", "prompt", None, 1),
            super::command_node("b", vec!["echo".into(), "done".into()]),
        ];
        nodes.push(super::agent_node("c", "sink", None, 1));
        let mut graph = Graph::new("g", "validate", nodes);
        graph.spec.edges.push(Edge {
            from: "a".to_owned(),
            to: "c".to_owned(),
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Succeeded),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        });
        let issues = graph.validate();
        assert!(
            issues
                .iter()
                .all(|issue| issue.severity != gloop_core::IssueSeverity::Error)
        );
    }

    #[test]
    fn graph_rejects_conditional_edge_with_non_terminal_status_helper() {
        let dependencies = ["a"];
        let drafts = vec![DependencyDraft {
            kind: EdgeKind::Conditional,
            when: Some(EdgeCondition {
                status: Some(NodeStatus::Running),
                output_contains: None,
                json_pointer: None,
                equals: None,
            }),
        }];
        assert!(build_dependency_edges(&dependencies, "x", &[0], &drafts).is_err());
    }

    #[test]
    fn json_pointer_validator_follows_rfc_6901_string_form() {
        assert!(is_valid_json_pointer(""));
        assert!(is_valid_json_pointer("/"));
        assert!(is_valid_json_pointer("/result"));
        assert!(is_valid_json_pointer("/items/0/name"));
        assert!(is_valid_json_pointer("/a//b"));
        assert!(is_valid_json_pointer("/a~1b/~0key/日本語 space"));
        assert!(!is_valid_json_pointer("result"));
        assert!(!is_valid_json_pointer("#/result"));
        assert!(!is_valid_json_pointer("/bad~"));
        assert!(!is_valid_json_pointer("/bad~2escape"));

        let invalid = build_dependency_edges(
            &["source"],
            "sink",
            &[0],
            &[DependencyDraft {
                kind: EdgeKind::Conditional,
                when: Some(EdgeCondition {
                    status: Some(NodeStatus::Succeeded),
                    output_contains: None,
                    json_pointer: Some("/bad~2escape".to_owned()),
                    equals: Some(json!(true)),
                }),
            }],
        );
        assert!(invalid.is_err());
    }

    #[test]
    fn json_condition_values_keep_their_types() {
        assert_eq!(parse_json_literal("true").expect("boolean"), json!(true));
        assert_eq!(parse_json_literal("42").expect("number"), json!(42));
        assert_eq!(parse_json_literal("null").expect("null"), json!(null));
        assert_eq!(
            parse_json_literal(r#"{"route":"review"}"#).expect("object"),
            json!({"route": "review"})
        );
        assert_eq!(
            parse_json_literal(r#""review""#).expect("string"),
            json!("review")
        );
        assert!(parse_json_literal("plain text is not JSON").is_err());

        let dependencies = ["judge"];
        let edges = build_dependency_edges(
            &dependencies,
            "next",
            &[0],
            &[DependencyDraft {
                kind: EdgeKind::Conditional,
                when: Some(EdgeCondition {
                    status: Some(NodeStatus::Succeeded),
                    output_contains: None,
                    json_pointer: Some("/done".to_owned()),
                    equals: Some(json!(true)),
                }),
            }],
        )
        .expect("typed condition");
        assert_eq!(
            edges[0].when.as_ref().and_then(|when| when.equals.clone()),
            Some(json!(true))
        );

        let mut graph = Graph::new(
            "typed-condition",
            "route using typed JSON",
            vec![
                super::command_node("judge", vec!["true".to_owned()]),
                super::command_node("next", vec!["true".to_owned()]),
            ],
        );
        graph.spec.edges = edges;
        assert_no_validation_errors(&graph);
        let reparsed = Graph::from_yaml_str(&graph.to_yaml().expect("serialize"))
            .expect("typed condition roundtrip");
        assert_eq!(
            reparsed.spec.edges[0]
                .when
                .as_ref()
                .and_then(|when| when.equals.clone()),
            Some(json!(true))
        );
    }

    #[test]
    fn graph_and_common_node_settings_roundtrip_strict_schema() {
        let mut node = super::agent_node("author", "write", Some("primary".to_owned()), 1);
        apply_common_node_settings(
            &mut node,
            CommonNodeSettings {
                resources: vec!["repository".to_owned(), "network".to_owned()],
                retry: RetryPolicy {
                    max_attempts: 3,
                    backoff_seconds: 2,
                    rebind_profiles: vec!["backup-a".to_owned(), "backup-b".to_owned()],
                },
                timeout_seconds: Some(90),
                workspace: WorkspaceSpec::Current,
                context: ContextSpec {
                    include_dependencies: false,
                    files: vec![PathBuf::from("README.md"), PathBuf::from("docs/SCHEMA.md")],
                    max_bytes: 32_768,
                },
            },
        )
        .expect("apply common settings");
        let output = node_output_mut(&mut node).expect("agent has output");
        output.format = OutputFormat::Json;
        output.max_bytes = 65_536;

        let mut graph = Graph::new("authored", "exercise settings", vec![node]);
        apply_graph_settings(
            &mut graph,
            GraphSettings {
                max_parallel: 7,
                failure: FailurePolicy::Continue,
                budgets: RunBudgets {
                    wall_time_seconds: Some(600),
                    model_calls: Some(20),
                },
                goal: None,
            },
        );
        assert_no_validation_errors(&graph);

        let reparsed = Graph::from_yaml_str(&graph.to_yaml().expect("serialize"))
            .expect("strict-schema roundtrip");
        assert_eq!(reparsed, graph);
        assert_eq!(reparsed.spec.policies.max_parallel, 7);
        assert_eq!(reparsed.spec.policies.failure, FailurePolicy::Continue);
        assert_eq!(reparsed.spec.budgets.wall_time_seconds, Some(600));
        assert_eq!(reparsed.spec.budgets.model_calls, Some(20));
        let authored = &reparsed.spec.nodes[0];
        assert_eq!(authored.resources, ["repository", "network"]);
        assert_eq!(authored.retry.max_attempts, 3);
        assert_eq!(authored.retry.backoff_seconds, 2);
        assert_eq!(authored.retry.rebind_profiles, ["backup-a", "backup-b"]);
        assert_eq!(authored.timeout_seconds, Some(90));
        assert!(!authored.context.include_dependencies);
        assert_eq!(authored.context.files.len(), 2);
        assert_eq!(authored.context.max_bytes, 32_768);
        assert_eq!(
            authored.output().expect("output").format,
            OutputFormat::Json
        );
        assert_eq!(authored.output().expect("output").max_bytes, 65_536);
    }

    #[test]
    fn worktree_to_inherit_adds_direct_success_edge() {
        let mut owner = super::agent_node("owner", "edit", None, 1);
        owner.workspace = WorkspaceSpec::Worktree {
            base: Some("main".to_owned()),
            auto_commit: true,
        };
        let mut followup = super::command_node("followup", vec!["true".to_owned()]);
        followup.workspace = WorkspaceSpec::Inherit {
            node: "owner".to_owned(),
        };
        let mut edges = Vec::new();
        ensure_workspace_inheritance_edge(&followup, &mut edges).expect("inherit edge");

        assert_eq!(edges, vec![Edge::data("owner", "followup")]);
        let mut graph = Graph::new(
            "workspace-chain",
            "reuse retained workspace",
            vec![owner, followup],
        );
        graph.spec.edges = edges;
        assert_no_validation_errors(&graph);
        assert_eq!(
            Graph::from_yaml_str(&graph.to_yaml().expect("serialize")).expect("roundtrip"),
            graph
        );
    }

    #[test]
    fn worktree_rejects_agent_fan_out_above_one() {
        let mut node = super::agent_node("parallel", "edit", None, 2);
        let error = apply_common_node_settings(
            &mut node,
            CommonNodeSettings {
                resources: Vec::new(),
                retry: RetryPolicy::default(),
                timeout_seconds: None,
                workspace: WorkspaceSpec::Worktree {
                    base: None,
                    auto_commit: false,
                },
                context: ContextSpec::default(),
            },
        )
        .expect_err("worktree fan-out must be rejected");
        assert!(error.to_string().contains("fan_out = 1"));
    }

    #[test]
    fn nested_loop_and_subgraph_roundtrip_and_validate() {
        let loop_body = Graph::new(
            "loop-body",
            "produce completion JSON",
            vec![super::command_node(
                "judge",
                vec!["printf".to_owned(), r#"{"done":true}"#.to_owned()],
            )],
        );
        let loop_node = loop_node(
            "iterate",
            loop_body,
            LoopCondition {
                node: "judge".to_owned(),
                status: NodeStatus::Succeeded,
                output_contains: None,
                json_pointer: Some("/done".to_owned()),
                equals: Some(json!(true)),
            },
            5,
            2,
        );
        let nested = Graph::new(
            "nested-work",
            "run nested work",
            vec![super::command_node("work", vec!["true".to_owned()])],
        );
        let subgraph = subgraph_node("nested", nested);
        let graph = Graph::new("composed", "exercise nesting", vec![loop_node, subgraph]);
        assert_no_validation_errors(&graph);

        let yaml = graph.to_yaml().expect("serialize nested graph");
        let reparsed = Graph::from_yaml_str(&yaml).expect("parse nested graph");
        assert_eq!(reparsed, graph);
        assert_no_validation_errors(&reparsed);
    }

    #[test]
    fn nesting_actions_stop_at_the_interactive_ux_limit() {
        let at_limit = wizard_actions(super::INTERACTIVE_NESTING_LIMIT);
        assert!(
            !at_limit.iter().any(|(_, action)| {
                matches!(action, WizardAction::Loop | WizardAction::Subgraph)
            })
        );
        let below_limit = wizard_actions(super::INTERACTIVE_NESTING_LIMIT - 1);
        assert!(
            below_limit
                .iter()
                .any(|(_, action)| *action == WizardAction::Loop)
        );
        assert!(
            below_limit
                .iter()
                .any(|(_, action)| *action == WizardAction::Subgraph)
        );
    }

    #[test]
    fn csv_inputs_are_trimmed_and_deduplicated() {
        assert_eq!(
            parse_csv("repo, network,repo,, docs "),
            ["repo", "network", "docs"]
        );
    }

    #[test]
    fn build_synthesize_node_kind_and_yaml_roundtrip() {
        let mut node = synthesize_node("synth", "combine findings", Some("provider-x".to_owned()));
        if let NodeKind::Synthesize { model, .. } = &mut node.kind {
            *model = Some("model-y".to_owned());
        } else {
            panic!("expected synthesize node");
        }

        let graph = Graph::new("synth-graph", "synthesize in tests", vec![node.clone()]);
        let yaml = graph.to_yaml().expect("serialize graph");

        assert!(yaml.contains("kind: synthesize"));
        assert!(yaml.contains("prompt:"));

        let reparsed_graph = graph_from_yaml_bytes(yaml).expect("serialize+roundtrip parse");
        match &reparsed_graph.spec.nodes[0].kind {
            NodeKind::Synthesize {
                prompt,
                profile,
                model,
                ..
            } => {
                assert_eq!(prompt, &node_prompt());
                assert_eq!(profile.as_deref(), Some("provider-x"));
                assert_eq!(model.as_deref(), Some("model-y"));
            }
            _ => panic!("expected synthesize node"),
        }

        let reparsed = graph_from_yaml_bytes(
            r#"apiVersion: gloop.dev/v1alpha1
kind: Graph
metadata:
  name: synth-graph
  version: "1.0.0"
spec:
  goal: synthesize in tests
  nodes:
    - id: synth
      kind: synthesize
      prompt: |-
        combine findings
  edges: []"#,
        )
        .expect("parse inline synthesize yaml");
        match &reparsed.spec.nodes[0].kind {
            NodeKind::Synthesize {
                prompt,
                profile,
                model,
                ..
            } => {
                assert_eq!(
                    prompt,
                    &gloop_core::PromptSpec::Inline("combine findings".to_owned())
                );
                assert_eq!(profile, &None);
                assert_eq!(model, &None);
            }
            _ => panic!("expected parsed synthesize node"),
        }
    }

    fn node_prompt() -> gloop_core::PromptSpec {
        gloop_core::PromptSpec::Inline("combine findings".to_owned())
    }

    fn assert_no_validation_errors(graph: &Graph) {
        let errors: Vec<_> = graph
            .validate()
            .into_iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .collect();
        assert!(errors.is_empty(), "validation errors: {errors:#?}");
    }

    #[test]
    fn template_seeding_lands_in_editor_state() {
        let state = seed_editor_from_template(
            "seeded",
            "seed goal",
            GraphTemplate::Direct,
            super::TemplateKnobs {
                request: Some("do work".to_owned()),
                provider_profiles: vec!["codex".to_owned()],
                loop_cap: None,
            },
        );
        assert_eq!(state.graph.metadata.name, "seeded");
        assert_eq!(state.graph.spec.goal, "seed goal");
        assert_eq!(state.graph.spec.nodes.len(), 1);
        assert_eq!(state.graph.spec.nodes[0].id, "request");
    }

    #[test]
    fn profile_choice_rendering_and_selection_mapping() {
        let profiles = vec![ProfileChoice {
            name: "codex".to_owned(),
            kind: "command".to_owned(),
            source: ProfileSource::Builtin,
            enabled: true,
            default_model: None,
        }];
        assert_eq!(
            profile_choice_label(&profiles[0]),
            "codex  (command, builtin)"
        );
        let items = profile_select_items(&profiles);
        assert_eq!(items.len(), 3);
        assert_eq!(
            map_profile_selection(&profiles, 0).expect("profile"),
            Some("codex".to_owned())
        );
        assert_eq!(
            map_profile_selection(&profiles, 1).expect("default routing"),
            None
        );
    }

    #[test]
    fn editor_edge_add_and_remove() {
        let mut state = EditorState::new("edges", "test edges");
        state.graph.spec.nodes = vec![
            super::agent_node("a", "prompt", None, 1),
            super::command_node("b", vec!["true".to_owned()]),
        ];
        state = add_edge_to_editor(&state, Edge::data("a", "b")).expect("add edge");
        assert_eq!(state.graph.spec.edges.len(), 1);
        state = remove_edge_from_editor(&state, "a", "b", EdgeKind::Data).expect("remove edge");
        assert!(state.graph.spec.edges.is_empty());
    }

    #[test]
    fn editor_edit_and_remove_node() {
        let mut state = EditorState::new("nodes", "test nodes");
        let node = super::agent_node("worker", "prompt", Some("codex".to_owned()), 1);
        state = add_node_to_editor(&state, node, &[], &[], &[]).expect("add node");
        let updated = super::agent_node("worker", "updated prompt", Some("claude".to_owned()), 1);
        state = replace_node_in_editor(&state, "worker", updated).expect("edit node");
        match &state.graph.spec.nodes[0].kind {
            NodeKind::Agent {
                prompt, profile, ..
            } => {
                assert_eq!(
                    prompt,
                    &gloop_core::PromptSpec::Inline("updated prompt".to_owned())
                );
                assert_eq!(profile.as_deref(), Some("claude"));
            }
            _ => panic!("expected agent node"),
        }
        state = remove_node_from_editor(&state, "worker").expect("remove node");
        assert!(state.graph.spec.nodes.is_empty());
    }

    #[test]
    fn save_blocked_by_validation_returns_errors() {
        let state = EditorState::new("empty", "no nodes yet");
        let errors = validate_for_save(&state).expect_err("empty graph cannot save");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("at least one node"))
        );
    }

    #[test]
    fn editor_summary_header_formats_node_line() {
        let mut state = EditorState::new("flow", "goal");
        state.graph.spec.nodes = vec![
            super::agent_node("implement", "prompt", Some("codex".to_owned()), 1),
            super::verify_node("test", vec!["true".to_owned()]),
        ];
        state.graph.spec.edges.push(Edge::data("implement", "test"));
        let (header, node_line) = editor_summary_header(&state);
        assert!(header.contains("2 nodes, 1 edges"));
        assert!(node_line.contains("implement(agent:codex)"));
        assert!(node_line.contains("test(verify)"));
        assert_eq!(
            format_node_summary(&state.graph.spec.nodes[0]),
            "implement(agent:codex)"
        );
    }

    #[test]
    fn persist_collision_can_be_retried_without_losing_graph_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut state = EditorState::new("dup", "goal");
        state.graph.spec.nodes = vec![super::agent_node("worker", "prompt", None, 1)];
        let target = EditorPersistTarget::ProjectTemplate {
            repo: dir.path().to_path_buf(),
            force: false,
        };
        try_persist_editor_graph(&state.graph, &target).expect("first save");
        let error = try_persist_editor_graph(&state.graph, &target).expect_err("collision");
        assert!(error.contains("output path exists"));
        let forced = EditorPersistTarget::ProjectTemplate {
            repo: dir.path().to_path_buf(),
            force: true,
        };
        try_persist_editor_graph(&state.graph, &forced).expect("forced overwrite");
        assert_eq!(state.graph.spec.nodes.len(), 1);
    }

    #[test]
    fn preflight_template_destination_rejects_existing_template() {
        let dir = tempfile::tempdir().expect("tempdir");
        let destination = super::template_path(dir.path(), "existing");
        std::fs::create_dir_all(destination.parent().unwrap()).expect("templates dir");
        std::fs::write(&destination, "existing").expect("seed template");
        let error =
            preflight_template_destination(dir.path(), "existing", false).expect_err("collision");
        assert!(error.contains("already exists"));
    }

    #[test]
    fn nested_editor_cancel_preserves_parent_state() {
        let parent = EditorState::new("parent", "goal");
        let parent = add_node_to_editor(
            &parent,
            super::agent_node("seed", "prompt", None, 1),
            &[],
            &[],
            &[],
        )
        .expect("seed parent node");
        let preserved = match NestedEditorOutcome::Cancelled {
            NestedEditorOutcome::Cancelled => parent.clone(),
            NestedEditorOutcome::Saved(_) => panic!("cancel should not replace parent state"),
        };
        assert_eq!(preserved.graph.spec.nodes.len(), 1);
        assert_eq!(preserved.graph.spec.nodes[0].id, "seed");
    }

    #[test]
    fn removal_blocked_when_other_nodes_inherit_workspace() {
        let mut owner = super::agent_node("owner", "prompt", None, 1);
        owner.workspace = WorkspaceSpec::Current;
        let mut follower = super::command_node("follower", vec!["true".to_owned()]);
        follower.workspace = WorkspaceSpec::Inherit {
            node: "owner".to_owned(),
        };
        let state =
            EditorState::from_graph(Graph::new("inherit", "goal", vec![owner, follower]), 0);
        assert_eq!(
            workspace_inheritance_dependents(&state, "owner"),
            vec!["follower".to_owned()]
        );
        let error = remove_node_from_editor(&state, "owner").expect_err("blocked removal");
        assert!(error.to_string().contains("follower"));
    }

    #[test]
    fn workspace_edit_reconciles_auto_inheritance_edges() {
        let owner = super::agent_node("owner", "prompt", None, 1);
        let mut follower = super::command_node("follower", vec!["true".to_owned()]);
        follower.workspace = WorkspaceSpec::Inherit {
            node: "owner".to_owned(),
        };
        let mut state = EditorState::from_graph(
            Graph::new("inherit", "goal", vec![owner.clone(), follower.clone()]),
            0,
        );
        ensure_workspace_inheritance_edge(&follower, &mut state.graph.spec.edges)
            .expect("initial inherit edge");
        let mut updated = follower.clone();
        updated.workspace = WorkspaceSpec::Current;
        state = replace_node_in_editor(&state, "follower", updated).expect("clear inherit");
        assert!(
            !state
                .graph
                .spec
                .edges
                .iter()
                .any(|edge| edge.from == "owner" && edge.to == "follower")
        );

        let mut reinherit = state.graph.spec.nodes[1].clone();
        reinherit.workspace = WorkspaceSpec::Inherit {
            node: "owner".to_owned(),
        };
        state = replace_node_in_editor(&state, "follower", reinherit).expect("restore inherit");
        assert!(
            state
                .graph
                .spec
                .edges
                .iter()
                .any(|edge| edge.from == "owner" && edge.to == "follower")
        );
    }

    #[test]
    fn output_schema_preserved_on_edit() {
        let existing = gloop_core::OutputSpec {
            format: OutputFormat::Json,
            schema: Some(PathBuf::from("schemas/out.json")),
            inline_schema: Some(json!({"type": "object"})),
            max_bytes: 4096,
        };
        let merged = merge_output_spec_edits(Some(&existing), OutputFormat::Json, 8192);
        assert_eq!(merged.schema, existing.schema);
        assert_eq!(merged.inline_schema, existing.inline_schema);
        assert_eq!(merged.max_bytes, 8192);
    }

    #[test]
    fn default_routing_is_default_profile_selection() {
        let profiles = vec![
            ProfileChoice {
                name: "codex".to_owned(),
                kind: "command".to_owned(),
                source: ProfileSource::Builtin,
                enabled: true,
                default_model: None,
            },
            ProfileChoice {
                name: "claude".to_owned(),
                kind: "anthropic".to_owned(),
                source: ProfileSource::Builtin,
                enabled: true,
                default_model: None,
            },
        ];
        assert_eq!(
            profile_default_select_index(&profiles, None),
            profiles.len()
        );
        assert_eq!(
            profile_default_select_index(&profiles, Some("")),
            profiles.len()
        );
    }

    #[test]
    fn empty_string_profile_selects_default_routing() {
        let profiles = vec![ProfileChoice {
            name: "codex".to_owned(),
            kind: "command".to_owned(),
            source: ProfileSource::Builtin,
            enabled: true,
            default_model: None,
        }];
        assert_eq!(
            map_profile_selection(&profiles, profiles.len()).expect("default routing index"),
            None
        );
        assert_eq!(
            profile_default_select_index(&profiles, Some("")),
            profiles.len()
        );
    }

    #[test]
    fn manual_profile_entry_classifies_disabled_and_unknown_profiles() {
        let profiles = vec![
            ProfileChoice {
                name: "enabled".to_owned(),
                kind: "command".to_owned(),
                source: ProfileSource::Builtin,
                enabled: true,
                default_model: None,
            },
            ProfileChoice {
                name: "disabled".to_owned(),
                kind: "command".to_owned(),
                source: ProfileSource::Builtin,
                enabled: false,
                default_model: None,
            },
        ];
        assert_eq!(
            classify_manual_profile_name("disabled", &profiles),
            ManualProfileEntry::KnownDisabled
        );
        assert_eq!(
            classify_manual_profile_name("external", &profiles),
            ManualProfileEntry::Accepted("external".to_owned())
        );
        assert_eq!(
            classify_manual_profile_name("enabled", &profiles),
            ManualProfileEntry::Accepted("enabled".to_owned())
        );
    }

    #[test]
    fn optional_model_can_be_cleared() {
        assert_eq!(
            resolve_optional_model(
                OptionalModelAction::Clear,
                Some("gpt-5"),
                Some("other".to_owned())
            ),
            None
        );
        assert_eq!(
            resolve_optional_model(
                OptionalModelAction::Keep,
                Some("gpt-5"),
                Some("other".to_owned())
            ),
            Some("gpt-5".to_owned())
        );
        assert_eq!(
            resolve_optional_model(
                OptionalModelAction::Change,
                Some("gpt-5"),
                Some("claude-sonnet".to_owned())
            ),
            Some("claude-sonnet".to_owned())
        );
    }

    #[test]
    fn failed_non_force_interactive_save_leaves_no_partial_destination_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let destination = dir.path().join("simulate-fail-graph.yml");

        let mut state = EditorState::new("flow", "goal");
        state.graph.spec.nodes = vec![super::agent_node("worker", "prompt", None, 1)];
        let target = EditorPersistTarget::GraphFile {
            path: destination.clone(),
            force: false,
        };
        let error = try_persist_editor_graph(&state.graph, &target).expect_err("save must fail");
        assert!(error.contains("simulated write failure"));
        assert!(
            !destination.exists(),
            "failed non-force interactive save must not leave a partial destination file: {error}"
        );
    }

    #[test]
    fn profile_change_uses_new_profile_model_default() {
        let profiles = vec![
            ProfileChoice {
                name: "codex".to_owned(),
                kind: "openai".to_owned(),
                source: ProfileSource::Builtin,
                enabled: true,
                default_model: Some("gpt-5".to_owned()),
            },
            ProfileChoice {
                name: "claude".to_owned(),
                kind: "anthropic".to_owned(),
                source: ProfileSource::Builtin,
                enabled: true,
                default_model: Some("claude-sonnet".to_owned()),
            },
        ];
        assert_eq!(
            profile_model_default(Some("claude"), Some("codex"), Some("old-model"), &profiles),
            "claude-sonnet"
        );
        assert_eq!(
            profile_model_default(Some("codex"), Some("codex"), Some("old-model"), &profiles),
            "old-model"
        );
    }

    #[test]
    fn disabled_profiles_are_filtered_from_selectable_choices() {
        let profiles = vec![
            ProfileChoice {
                name: "enabled".to_owned(),
                kind: "command".to_owned(),
                source: ProfileSource::Builtin,
                enabled: true,
                default_model: None,
            },
            ProfileChoice {
                name: "disabled".to_owned(),
                kind: "command".to_owned(),
                source: ProfileSource::Builtin,
                enabled: false,
                default_model: None,
            },
        ];
        let enabled = enabled_profile_choices(&profiles);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "enabled");
        let items = profile_select_items(&enabled);
        assert!(!items.iter().any(|item| item.starts_with("disabled")));
    }

    #[test]
    fn optional_budget_can_be_cleared() {
        assert_eq!(
            resolve_optional_number(OptionalNumberAction::Clear, Some(42u32), Some(99)),
            None
        );
        assert_eq!(
            resolve_optional_number(OptionalNumberAction::Keep, Some(42u32), Some(99)),
            Some(42)
        );
        assert_eq!(
            resolve_optional_number(OptionalNumberAction::Change, Some(42u32), Some(99)),
            Some(99)
        );
    }

    #[test]
    fn argv_shell_quoting_round_trips_token_boundaries() {
        let argv = [
            "sh".to_owned(),
            "-c".to_owned(),
            "echo a".to_owned(),
            "hello world".to_owned(),
            "say \"hi\"".to_owned(),
        ];
        let rendered = format_argv_for_shell(&argv[1..]);
        let reparsed = parse_argv(&rendered).expect("parse quoted argv");
        assert_eq!(reparsed, argv[1..]);
        assert_eq!(shell_quote_arg("plain"), "plain");
        assert_eq!(shell_quote_arg("hello world"), "'hello world'");
    }

    #[tokio::test]
    async fn write_graph_yaml_saves_inside_async_runtime() {
        // Regression: the previous implementation built a nested Tokio runtime
        // and panicked with "Cannot start a runtime from within a runtime"
        // whenever the interactive editor saved from the CLI's own runtime.
        let dir = tempfile::tempdir().expect("tempdir");
        let destination = dir.path().join("graph.yaml");
        super::write_graph_yaml(&destination, "kind: Graph\n", false).expect("no-replace save");
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read back"),
            "kind: Graph\n"
        );
        super::write_graph_yaml(&destination, "kind: Graph2\n", true).expect("force save");
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read back"),
            "kind: Graph2\n"
        );
        let error = super::write_graph_yaml(&destination, "x\n", false).expect_err("exists");
        assert!(error.contains("use --force"), "unexpected error: {error}");
    }
}
