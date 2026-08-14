//! Resident terminal UI for selecting a graph, provider profile, and model.
//!
//! The TUI owns only presentation state. Graph validation, provider invocation,
//! scheduling, retries, artifacts, and journal persistence remain in the
//! existing core/provider/runtime crates.

use std::{
    collections::{HashMap, VecDeque},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use gloop_core::{Edge, Graph, Node, NodeKind, NodeStatus, PromptSpec, RunEventKind, RunSummary};
use gloop_provider::{PROJECT_CONFIG_PATH, ProfileStore, ProviderRegistry};
use gloop_runtime::{GateDecision, GateRequest, HumanGate, ProgressEvent, RunOptions, Runtime};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    atomic_write,
    commands::{apply_model_to_agent_nodes, build_profile_choices},
    templates,
    wizard::{self, EditorState, GraphTemplate, ProfileChoice},
};

const TEMPLATE_CHOICES: [GraphTemplate; 4] = [
    GraphTemplate::Direct,
    GraphTemplate::PlanImplementVerify,
    GraphTemplate::ParallelResearchReduce,
    GraphTemplate::ReviewFixLoop,
];

const DEFAULT_TASK: &str = "Describe the task for the graph";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Overview,
    Builder,
    Run,
}

impl Screen {
    const ALL: [Self; 3] = [Self::Overview, Self::Builder, Self::Run];

    const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Builder => "Graph Builder",
            Self::Run => "Run Monitor",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Builder => 1,
            Self::Run => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputTarget {
    Task,
    Model,
    NodePrompt,
}

#[derive(Debug)]
struct InputState {
    target: InputTarget,
    value: String,
    cursor: usize,
}

#[derive(Debug)]
struct ActiveRun {
    cancellation: CancellationToken,
    gates: mpsc::UnboundedReceiver<GateEnvelope>,
    progress: mpsc::UnboundedReceiver<ProgressEvent>,
    task: JoinHandle<std::result::Result<RunSummary, String>>,
}

#[derive(Debug)]
struct GateEnvelope {
    request: GateRequest,
    reply: oneshot::Sender<GateDecision>,
}

#[derive(Debug, Clone)]
struct TuiGate {
    requests: mpsc::UnboundedSender<GateEnvelope>,
}

#[async_trait]
impl HumanGate for TuiGate {
    async fn decide(&self, request: GateRequest) -> std::result::Result<GateDecision, String> {
        let (reply, decision) = oneshot::channel();
        self.requests
            .send(GateEnvelope { request, reply })
            .map_err(|_| "TUI gate closed".to_owned())?;
        decision
            .await
            .map_err(|_| "TUI gate response was closed".to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Continue,
    Quit,
    Save,
}

#[derive(Debug)]
struct App {
    repo: PathBuf,
    trust_project_profiles: bool,
    screen: Screen,
    graph: Graph,
    template: GraphTemplate,
    template_index: usize,
    task: String,
    model: Option<String>,
    profiles: Vec<ProfileChoice>,
    profile_index: Option<usize>,
    selected_node: usize,
    connect_from: Option<String>,
    input: Option<InputState>,
    active_run: Option<ActiveRun>,
    pending_gates: VecDeque<GateEnvelope>,
    node_status: HashMap<String, NodeStatus>,
    last_summary: Option<RunSummary>,
    events: VecDeque<String>,
    status: String,
    dirty: bool,
}

impl App {
    fn new(repo: PathBuf, trust_project_profiles: bool) -> Result<Self> {
        let profiles = build_profile_choices(&repo, trust_project_profiles)
            .map_err(|error| anyhow!("failed to load provider profiles: {error}"))?;
        let profile_index = profiles.iter().position(|profile| profile.enabled);
        let template = GraphTemplate::Direct;
        let task = DEFAULT_TASK.to_owned();
        let mut graph = Self::build_graph(
            template,
            &task,
            profile_name(&profiles, profile_index),
            None,
        );
        graph.metadata.name = String::from("work");
        graph.spec.goal.clone_from(&task);

        Ok(Self {
            repo,
            trust_project_profiles,
            screen: Screen::Overview,
            graph,
            template,
            template_index: 0,
            task,
            model: None,
            profiles,
            profile_index,
            selected_node: 0,
            connect_from: None,
            input: None,
            active_run: None,
            pending_gates: VecDeque::new(),
            node_status: HashMap::new(),
            last_summary: None,
            events: VecDeque::new(),
            status: "Ready. Press i to enter a task, then r to run.".to_owned(),
            dirty: false,
        })
    }

    fn build_graph(
        template: GraphTemplate,
        task: &str,
        profile: Option<&str>,
        model: Option<&str>,
    ) -> Graph {
        let mut graph = wizard::template_graph(
            "work",
            task,
            template,
            Some(task.to_owned()),
            profile.map(|value| vec![value.to_owned()]),
            None,
        );
        if let Some(model) = model {
            apply_model_to_agent_nodes(&mut graph, model);
        }
        graph
    }

    fn selected_profile(&self) -> Option<&str> {
        profile_name(&self.profiles, self.profile_index)
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    fn rebuild_from_choices(&mut self) {
        self.graph = Self::build_graph(
            self.template,
            &self.task,
            self.selected_profile(),
            self.model.as_deref(),
        );
        self.selected_node = 0;
        self.connect_from = None;
        self.dirty = true;
    }

    fn cycle_template(&mut self) {
        self.template_index = (self.template_index + 1) % TEMPLATE_CHOICES.len();
        self.template = TEMPLATE_CHOICES[self.template_index];
        self.rebuild_from_choices();
        self.status = format!(
            "Template: {}. Existing node edits were reset from the template.",
            template_label(self.template)
        );
    }

    fn cycle_profile(&mut self) {
        let enabled: Vec<usize> = self
            .profiles
            .iter()
            .enumerate()
            .filter_map(|(index, profile)| profile.enabled.then_some(index))
            .collect();
        if enabled.is_empty() {
            self.set_status(
                "No enabled provider profiles were found; runtime default routing remains active.",
            );
            return;
        }
        let next = self
            .profile_index
            .and_then(|current| enabled.iter().position(|index| *index == current))
            .map_or(0, |position| (position + 1) % enabled.len());
        self.profile_index = Some(enabled[next]);
        self.rebuild_from_choices();
        self.status = format!(
            "Profile: {}. Applied to agent-like nodes.",
            self.selected_profile().unwrap_or("runtime default")
        );
    }

    fn begin_input(&mut self, target: InputTarget) {
        let value = match target {
            InputTarget::Task => self.task.clone(),
            InputTarget::Model => self.model.clone().unwrap_or_default(),
            InputTarget::NodePrompt => {
                let Some(node) = self.graph.spec.nodes.get(self.selected_node) else {
                    self.set_status("No node is selected.");
                    return;
                };
                let Some(prompt) = node_prompt(node) else {
                    self.set_status("The selected node does not have an inline prompt.");
                    return;
                };
                prompt.to_owned()
            }
        };
        let cursor = value.len();
        self.input = Some(InputState {
            target,
            value,
            cursor,
        });
    }

    fn commit_input(&mut self) {
        let Some(input) = self.input.take() else {
            return;
        };
        let value = input.value.trim().to_owned();
        if value.is_empty() && matches!(input.target, InputTarget::Task) {
            self.set_status("Task cannot be empty.");
            return;
        }
        match input.target {
            InputTarget::Task => {
                self.task = value;
                self.rebuild_from_choices();
                self.set_status("Task updated. Review the graph, then press r to run.");
            }
            InputTarget::Model => {
                self.model = (!value.is_empty()).then_some(value);
                if let Some(model) = self.model.as_deref() {
                    apply_model_to_agent_nodes(&mut self.graph, model);
                } else {
                    self.rebuild_from_choices();
                }
                self.dirty = true;
                self.status = format!(
                    "Model override: {}.",
                    self.model.as_deref().unwrap_or("provider default")
                );
            }
            InputTarget::NodePrompt => {
                let Some(node) = self.graph.spec.nodes.get_mut(self.selected_node) else {
                    self.set_status("No node is selected.");
                    return;
                };
                match &mut node.kind {
                    NodeKind::Agent { prompt, .. }
                    | NodeKind::Reduce { prompt, .. }
                    | NodeKind::Synthesize { prompt, .. } => {
                        *prompt = PromptSpec::Inline(value);
                        self.dirty = true;
                        self.set_status("Node prompt updated.");
                    }
                    _ => self.set_status("The selected node is not prompt-based."),
                }
            }
        }
    }

    fn cancel_input(&mut self) {
        self.input = None;
        self.set_status("Edit cancelled.");
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        let Some(input) = self.input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.cancel_input(),
            KeyCode::Enter => self.commit_input(),
            KeyCode::Backspace => {
                if input.cursor > 0 {
                    let previous = input.value[..input.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                    input.value.drain(previous..input.cursor);
                    input.cursor = previous;
                }
            }
            KeyCode::Delete => {
                if input.cursor < input.value.len() {
                    let next = input.value[input.cursor..]
                        .char_indices()
                        .nth(1)
                        .map_or(input.value.len(), |(index, _)| input.cursor + index);
                    input.value.drain(input.cursor..next);
                }
            }
            KeyCode::Left => {
                input.cursor = input.value[..input.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(index, _)| index);
            }
            KeyCode::Right => {
                input.cursor = input.value[input.cursor..]
                    .char_indices()
                    .nth(1)
                    .map_or(input.value.len(), |(index, _)| input.cursor + index);
            }
            KeyCode::Home => input.cursor = 0,
            KeyCode::End => input.cursor = input.value.len(),
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                input.value.insert(input.cursor, character);
                input.cursor += character.len_utf8();
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.input.is_some() {
            self.handle_input_key(key);
            return Action::Continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(active) = &self.active_run {
                active.cancellation.cancel();
                self.set_status("Cancellation requested; waiting for the runtime to finish...");
            } else {
                return Action::Quit;
            }
            return Action::Continue;
        }
        if self.active_run.is_some() {
            if !self.pending_gates.is_empty() {
                match key.code {
                    KeyCode::Char('y') => self.resolve_gate(GateDecision::Approve),
                    KeyCode::Char('n') | KeyCode::Esc => self.resolve_gate(GateDecision::Reject),
                    KeyCode::Enter => {
                        let default = self
                            .pending_gates
                            .front()
                            .map_or(GateDecision::Reject, |gate| gate.request.default);
                        self.resolve_gate(default);
                    }
                    _ => {}
                }
                return Action::Continue;
            }
            match key.code {
                KeyCode::Char('1') => self.screen = Screen::Overview,
                KeyCode::Char('2') => self.screen = Screen::Builder,
                KeyCode::Char('3') => self.screen = Screen::Run,
                KeyCode::Tab => self.next_screen(),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                _ => {}
            }
            return Action::Continue;
        }
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('1') => {
                self.screen = Screen::Overview;
                Action::Continue
            }
            KeyCode::Char('2') => {
                self.screen = Screen::Builder;
                Action::Continue
            }
            KeyCode::Char('3') => {
                self.screen = Screen::Run;
                Action::Continue
            }
            KeyCode::Tab => {
                self.next_screen();
                Action::Continue
            }
            KeyCode::Char('t') => {
                self.cycle_template();
                Action::Continue
            }
            KeyCode::Char('p') => {
                self.cycle_profile();
                Action::Continue
            }
            KeyCode::Char('m') => {
                self.begin_input(InputTarget::Model);
                Action::Continue
            }
            KeyCode::Char('i') => {
                self.begin_input(InputTarget::Task);
                Action::Continue
            }
            KeyCode::Char('r') => {
                self.start_run();
                Action::Continue
            }
            KeyCode::Char('s') => Action::Save,
            KeyCode::Char('v') => {
                self.validate_graph();
                Action::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::Continue
            }
            KeyCode::Char('a') if self.screen == Screen::Builder => {
                self.add_node();
                Action::Continue
            }
            KeyCode::Char('x') if self.screen == Screen::Builder => {
                self.remove_node();
                Action::Continue
            }
            KeyCode::Char('e') if self.screen == Screen::Builder => {
                self.begin_input(InputTarget::NodePrompt);
                Action::Continue
            }
            KeyCode::Char('c') if self.screen == Screen::Builder => {
                self.begin_connection();
                Action::Continue
            }
            KeyCode::Enter if self.connect_from.is_some() => {
                self.connect_selected_node();
                Action::Continue
            }
            KeyCode::Esc if self.connect_from.take().is_some() => {
                self.set_status("Connection cancelled.");
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    fn next_screen(&mut self) {
        let next = (self.screen.index() + 1) % Screen::ALL.len();
        self.screen = Screen::ALL[next];
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.graph.spec.nodes.len();
        if len == 0 {
            self.selected_node = 0;
            return;
        }
        let offset = delta.unsigned_abs();
        let next = if delta.is_negative() {
            self.selected_node.saturating_sub(offset)
        } else {
            self.selected_node.saturating_add(offset)
        };
        self.selected_node = next.min(len - 1);
    }

    fn add_node(&mut self) {
        let mut counter = self.graph.spec.nodes.len() + 1;
        let id = loop {
            let candidate = format!("step_{counter}");
            if !self
                .graph
                .spec
                .nodes
                .iter()
                .any(|node| node.id == candidate)
            {
                break candidate;
            }
            counter += 1;
        };
        let mut node = Node::agent(id.clone(), format!("Continue the task:\n{}", self.task));
        if let NodeKind::Agent { profile, model, .. } = &mut node.kind {
            *profile = self.selected_profile().map(ToOwned::to_owned);
            model.clone_from(&self.model);
        }
        let state = EditorState::from_graph(self.graph.clone(), 0);
        let mut state = match wizard::add_node_to_editor(&state, node, &[], &[], &[]) {
            Ok(state) => state,
            Err(error) => {
                self.status = format!("Could not add node: {error}");
                return;
            }
        };
        if let Some(previous) = self.graph.spec.nodes.last() {
            state = match wizard::add_edge_to_editor(&state, Edge::data(&previous.id, &id)) {
                Ok(state) => state,
                Err(error) => {
                    self.status = format!("Could not connect new node: {error}");
                    return;
                }
            };
        }
        self.graph = state.graph;
        self.selected_node = self.graph.spec.nodes.len() - 1;
        self.dirty = true;
        self.status = format!("Added {id}. Press e to edit its prompt or c to connect it.");
    }

    fn remove_node(&mut self) {
        if self.graph.spec.nodes.len() <= 1 {
            self.set_status("Keep at least one node in the graph.");
            return;
        }
        let removed = self.graph.spec.nodes[self.selected_node].id.clone();
        let state = EditorState::from_graph(self.graph.clone(), 0);
        let state = match wizard::remove_node_from_editor(&state, &removed) {
            Ok(state) => state,
            Err(error) => {
                self.status = format!("Could not remove {removed}: {error}");
                return;
            }
        };
        self.graph = state.graph;
        self.selected_node = self
            .selected_node
            .min(self.graph.spec.nodes.len().saturating_sub(1));
        self.dirty = true;
        self.status = format!("Removed {removed} and its incident edges.");
    }

    fn begin_connection(&mut self) {
        let Some(node) = self.graph.spec.nodes.get(self.selected_node) else {
            return;
        };
        self.connect_from = Some(node.id.clone());
        self.status = format!(
            "Connecting from {}. Move to a target and press Enter.",
            node.id
        );
    }

    fn connect_selected_node(&mut self) {
        let Some(from) = self.connect_from.take() else {
            return;
        };
        let Some(target) = self.graph.spec.nodes.get(self.selected_node) else {
            return;
        };
        let to = target.id.clone();
        if from == to {
            self.set_status("A node cannot connect to itself.");
            return;
        }
        let state = EditorState::from_graph(self.graph.clone(), 0);
        let state = match wizard::add_edge_to_editor(&state, Edge::data(&from, &to)) {
            Ok(state) => state,
            Err(error) => {
                self.status = format!("Connection rejected: {error}");
                return;
            }
        };
        self.graph = state.graph;
        self.dirty = true;
        self.status = format!("Added data edge {from} -> {to}.");
    }

    fn validate_graph(&mut self) {
        let issues = self.graph.validate();
        let errors = issues
            .iter()
            .filter(|issue| issue.severity == gloop_core::IssueSeverity::Error)
            .count();
        let warnings = issues
            .iter()
            .filter(|issue| issue.severity == gloop_core::IssueSeverity::Warning)
            .count();
        self.status = if errors == 0 {
            format!("Graph valid ({warnings} warning(s)).")
        } else {
            format!("Graph invalid: {errors} error(s), {warnings} warning(s).")
        };
    }

    fn start_run(&mut self) {
        if self.active_run.is_some() {
            self.set_status("A run is already active.");
            return;
        }
        let issues = self.graph.validate();
        if issues
            .iter()
            .any(|issue| issue.severity == gloop_core::IssueSeverity::Error)
        {
            self.set_status(
                "Run blocked: fix graph validation errors first (press v for a count).",
            );
            return;
        }
        let graph = self.graph.clone();
        let repo = self.repo.clone();
        let trust_project_profiles = self.trust_project_profiles;
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let (progress_tx, progress) = mpsc::unbounded_channel();
        let (gate_tx, gates) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let store = if trust_project_profiles {
                ProfileStore::load_trusted_project(&repo)
            } else {
                ProfileStore::load(&repo)
            }
            .map_err(|error| format!("failed to load provider profiles: {error}"))?;
            let runtime = Runtime::new(
                ProviderRegistry::new(store),
                repo.join(PROJECT_CONFIG_PATH).with_file_name("runs"),
            )
            .with_human_gate(Arc::new(TuiGate { requests: gate_tx }));
            runtime
                .run(
                    &graph,
                    RunOptions {
                        current_dir: repo,
                        cancellation: task_cancellation,
                        progress: Some(progress_tx),
                        ..RunOptions::default()
                    },
                )
                .await
                .map_err(|error| error.to_string())
        });
        self.node_status = self
            .graph
            .spec
            .nodes
            .iter()
            .map(|node| (node.id.clone(), NodeStatus::Pending))
            .collect();
        self.events.clear();
        self.last_summary = None;
        self.active_run = Some(ActiveRun {
            cancellation,
            gates,
            progress,
            task,
        });
        self.screen = Screen::Run;
        self.set_status(
            "Run started. The runtime owns scheduling; this screen only observes events.",
        );
    }

    async fn drain_run_events(&mut self) {
        let mut pending = Vec::new();
        let mut gates = Vec::new();
        let finished = {
            let Some(active) = self.active_run.as_mut() else {
                return;
            };
            while let Ok(event) = active.progress.try_recv() {
                pending.push(event);
            }
            while let Ok(gate) = active.gates.try_recv() {
                gates.push(gate);
            }
            active.task.is_finished()
        };
        for event in pending {
            self.apply_progress(&event);
        }
        for gate in gates {
            self.pending_gates.push_back(gate);
        }
        if !self.pending_gates.is_empty() {
            self.set_status(
                "Human gate waiting: press y to approve, n to reject, Enter for default.",
            );
        }
        if !finished {
            return;
        }
        let active = self.active_run.take().expect("active run exists");
        match active.task.await {
            Ok(Ok(summary)) => {
                self.status = format!("Run finished: {:?}.", summary.status);
                self.last_summary = Some(summary);
            }
            Ok(Err(error)) => self.status = format!("Run failed: {error}"),
            Err(error) => self.status = format!("Run task stopped unexpectedly: {error}"),
        }
    }

    fn resolve_gate(&mut self, decision: GateDecision) {
        let Some(gate) = self.pending_gates.pop_front() else {
            return;
        };
        let node = gate.request.node_id;
        let _ = gate.reply.send(decision);
        self.status = format!(
            "Gate {node}: {}.",
            match decision {
                GateDecision::Approve => "approved",
                GateDecision::Reject => "rejected",
            }
        );
    }

    fn apply_progress(&mut self, event: &ProgressEvent) {
        if let Some(node) = event.node_id.as_deref()
            && let Some(status) = status_for_event(event.kind)
        {
            self.node_status.insert(node.to_owned(), status);
        }
        let node = event.node_id.as_deref().unwrap_or("run");
        let message = event.message.as_deref().unwrap_or_default();
        let line = format!(
            "{:>4} {:<22} {:<18} {}",
            event.sequence,
            node,
            event_kind_label(event.kind),
            message
        );
        self.events.push_back(line);
        while self.events.len() > 80 {
            self.events.pop_front();
        }
    }

    async fn save(&mut self) -> Result<()> {
        let issues = self.graph.validate();
        if issues
            .iter()
            .any(|issue| issue.severity == gloop_core::IssueSeverity::Error)
        {
            self.set_status("Save blocked: graph is invalid.");
            return Ok(());
        }
        templates::ensure_managed_directory(&self.repo, Path::new(templates::GRAPHS_DIR))
            .map_err(|error| anyhow!("managed graph directory is unsafe: {error}"))?;
        let path = templates::graph_path(&self.repo, &self.graph.metadata.name);
        let yaml = self
            .graph
            .to_yaml()
            .map_err(|error| anyhow!(error.to_string()))?;
        atomic_write::write_text_atomic(&path, &yaml)
            .await
            .map_err(|error| anyhow!("failed to save graph: {error}"))?;
        self.dirty = false;
        self.status = format!("Saved {}.", path.display());
        Ok(())
    }
}

fn profile_name(profiles: &[ProfileChoice], index: Option<usize>) -> Option<&str> {
    index
        .and_then(|index| profiles.get(index))
        .filter(|profile| profile.enabled)
        .map(|profile| profile.name.as_str())
}

fn node_prompt(node: &Node) -> Option<&str> {
    match &node.kind {
        NodeKind::Agent { prompt, .. }
        | NodeKind::Reduce { prompt, .. }
        | NodeKind::Synthesize { prompt, .. } => match prompt {
            PromptSpec::Inline(value) => Some(value),
            PromptSpec::Package { .. } => None,
        },
        _ => None,
    }
}

fn template_label(template: GraphTemplate) -> &'static str {
    match template {
        GraphTemplate::Direct => "direct",
        GraphTemplate::PlanImplementVerify => "plan-implement-verify",
        GraphTemplate::ParallelResearchReduce => "parallel-research-reduce",
        GraphTemplate::ReviewFixLoop => "review-fix-loop",
    }
}

fn event_kind_label(kind: RunEventKind) -> &'static str {
    match kind {
        RunEventKind::RunStarted => "run started",
        RunEventKind::NodeReady => "ready",
        RunEventKind::NodeStarted => "running",
        RunEventKind::NodeOutput => "output",
        RunEventKind::NodeSucceeded => "succeeded",
        RunEventKind::NodeFailed => "failed",
        RunEventKind::NodeSkipped => "skipped",
        RunEventKind::NodeBlocked => "blocked",
        RunEventKind::RetryScheduled => "retry",
        RunEventKind::LoopStarted => "loop started",
        RunEventKind::LoopIterationStarted => "iteration",
        RunEventKind::LoopIterationFinished => "iteration done",
        RunEventKind::LoopFinished => "loop done",
        RunEventKind::RunCancelled => "cancelled",
        RunEventKind::RunFinished => "run finished",
    }
}

fn status_for_event(kind: RunEventKind) -> Option<NodeStatus> {
    Some(match kind {
        RunEventKind::NodeReady => NodeStatus::Ready,
        RunEventKind::NodeStarted => NodeStatus::Running,
        RunEventKind::NodeSucceeded => NodeStatus::Succeeded,
        RunEventKind::NodeFailed => NodeStatus::Failed,
        RunEventKind::NodeSkipped => NodeStatus::Skipped,
        RunEventKind::NodeBlocked => NodeStatus::Blocked,
        _ => return None,
    })
}

fn status_symbol(status: NodeStatus) -> (&'static str, Color) {
    match status {
        NodeStatus::Pending => ("·", Color::DarkGray),
        NodeStatus::Ready => ("○", Color::Yellow),
        NodeStatus::Running => ("◐", Color::Cyan),
        NodeStatus::Succeeded => ("✓", Color::Green),
        NodeStatus::Failed => ("×", Color::Red),
        NodeStatus::Skipped => ("–", Color::DarkGray),
        NodeStatus::Blocked => ("!", Color::Red),
        NodeStatus::Cancelled => ("□", Color::Yellow),
    }
}

fn node_kind_label(node: &Node) -> &'static str {
    match &node.kind {
        NodeKind::Agent { .. } => "agent",
        NodeKind::Command { .. } => "command",
        NodeKind::Reduce { .. } => "reduce",
        NodeKind::Verify { .. } => "verify",
        NodeKind::Synthesize { .. } => "synthesize",
        NodeKind::Gate { .. } => "gate",
        NodeKind::Loop { .. } => "loop",
        NodeKind::Subgraph { .. } => "subgraph",
    }
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

    let titles: Vec<Line> = Screen::ALL
        .iter()
        .map(|screen| Line::from(format!(" {} ", screen.title())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.screen.index())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" gloop graph "),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, root[0]);

    match app.screen {
        Screen::Overview => render_overview(frame, app, root[1]),
        Screen::Builder => render_builder(frame, app, root[1]),
        Screen::Run => render_run(frame, app, root[1]),
    }

    let footer = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.status),
        ]),
        Line::from(vec![
            Span::styled("1/2/3", Style::default().fg(Color::Cyan)),
            Span::raw(" screens  "),
            Span::styled("i", Style::default().fg(Color::Cyan)),
            Span::raw(" task  "),
            Span::styled("t", Style::default().fg(Color::Cyan)),
            Span::raw(" template  "),
            Span::styled("p", Style::default().fg(Color::Cyan)),
            Span::raw(" profile  "),
            Span::styled("m", Style::default().fg(Color::Cyan)),
            Span::raw(" model  "),
            Span::styled("r", Style::default().fg(Color::Green)),
            Span::raw(" run  "),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::raw(" save  "),
            Span::styled("q", Style::default().fg(Color::Red)),
            Span::raw(" quit"),
        ]),
    ]))
    .block(Block::default().borders(Borders::TOP));
    frame.render_widget(footer, root[2]);

    if let Some(input) = &app.input {
        render_input(frame, input, centered_rect(90, 7, area));
    }
    if let Some(gate) = app.pending_gates.front() {
        render_gate(frame, gate, centered_rect(70, 9, area));
    }
}

fn render_overview(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);
    let selected_profile = app.selected_profile().unwrap_or("runtime default");
    let model = app.model.as_deref().unwrap_or("provider default");
    let summary = vec![
        Line::from(Span::styled(
            "START HERE",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Task       ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.task),
        ]),
        Line::from(vec![
            Span::styled("Template   ", Style::default().fg(Color::DarkGray)),
            Span::raw(template_label(app.template)),
        ]),
        Line::from(vec![
            Span::styled("Profile    ", Style::default().fg(Color::DarkGray)),
            Span::raw(selected_profile),
        ]),
        Line::from(vec![
            Span::styled("Model      ", Style::default().fg(Color::DarkGray)),
            Span::raw(model),
        ]),
        Line::from(vec![
            Span::styled("Graph      ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "{} nodes / {} edges",
                app.graph.spec.nodes.len(),
                app.graph.spec.edges.len()
            )),
        ]),
        Line::from(vec![
            Span::styled("Save       ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(".gloop/graphs/{}.yaml", app.graph.metadata.name)),
        ]),
        Line::from(""),
        Line::from("Press i to type the natural-language task."),
        Line::from("Press t/p/m to change the graph bindings."),
        Line::from("Press 2 to inspect and edit the node list."),
        Line::from("Press r only after the graph is valid."),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(summary))
            .block(Block::default().borders(Borders::ALL).title(" Selection "))
            .wrap(Wrap { trim: false }),
        columns[0],
    );

    let flow = app
        .graph
        .spec
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let selected = index == app.selected_node;
            let profile = node.profile().unwrap_or("runtime");
            let model = node.model().unwrap_or("default");
            let style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>2} ", index + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(node.id.clone(), style),
                Span::raw(format!(
                    "  [{}]  {profile} / {model}",
                    node_kind_label(node)
                )),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(flow)
            .block(Block::default().borders(Borders::ALL).title(" Graph flow "))
            .highlight_style(Style::default().fg(Color::Cyan)),
        columns[1],
    );
}

fn render_builder(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);
    let items = app
        .graph
        .spec
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let marker = if index == app.selected_node {
                "▸"
            } else {
                " "
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {}  {}", node.id, node_kind_label(node))),
            ]))
        })
        .collect::<Vec<_>>();
    let title = if let Some(from) = &app.connect_from {
        format!(" Nodes · connecting from {from} ")
    } else {
        " Nodes · j/k select ".to_owned()
    };
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
        columns[0],
    );

    let Some(node) = app.graph.spec.nodes.get(app.selected_node) else {
        frame.render_widget(
            Paragraph::new("No node selected.").block(Block::default().borders(Borders::ALL)),
            columns[1],
        );
        return;
    };
    let edges = app
        .graph
        .spec
        .edges
        .iter()
        .map(|edge| format!("{} -{:?}-> {}", edge.from, edge.kind, edge.to))
        .collect::<Vec<_>>();
    let prompt = node_prompt(node).unwrap_or("(not a prompt node)");
    let detail = vec![
        Line::from(vec![
            Span::styled(
                "NODE ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(&node.id),
        ]),
        Line::from(format!("kind: {}", node_kind_label(node))),
        Line::from(format!(
            "profile: {}",
            node.profile().unwrap_or("runtime default")
        )),
        Line::from(format!(
            "model: {}",
            node.model().unwrap_or("provider default")
        )),
        Line::from(format!("retry attempts: {}", node.retry.max_attempts)),
        Line::from(format!("fan-out: {}", node.fan_out())),
        Line::from(""),
        Line::from(Span::styled(
            "PROMPT",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(prompt),
        Line::from(""),
        Line::from(Span::styled(
            "EDGES",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    let mut text = detail;
    text.extend(edges.into_iter().map(Line::from));
    text.push(Line::from(""));
    text.push(Line::from(
        "a add agent  e edit prompt  x remove  c connect  Enter finish",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(text))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Node editor "),
            )
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

fn render_run(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    let nodes = app
        .graph
        .spec
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let status = app
                .node_status
                .get(&node.id)
                .copied()
                .unwrap_or(NodeStatus::Pending);
            let (symbol, color) = status_symbol(status);
            let marker = if index == app.selected_node {
                "▸"
            } else {
                " "
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} {symbol} "), Style::default().fg(color)),
                Span::raw(format!("{:<18} ", node.id)),
                Span::styled(format!("{status:?}"), Style::default().fg(color)),
            ]))
        })
        .collect::<Vec<_>>();
    let graph_status = if app.active_run.is_some() {
        "RUNNING"
    } else {
        "IDLE"
    };
    frame.render_widget(
        List::new(nodes).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Nodes · {graph_status} ")),
        ),
        columns[0],
    );

    let mut lines = vec![Line::from(Span::styled(
        "EVENT STREAM",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];
    lines.extend(app.events.iter().cloned().map(Line::from));
    if let Some(summary) = &app.last_summary {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "RESULT",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("status: {:?}", summary.status)));
        lines.push(Line::from(format!(
            "run: {} · {}ms",
            summary.run_id, summary.duration_ms
        )));
        if let Some(node) = app.graph.spec.nodes.get(app.selected_node)
            && let Some(outcome) = summary.nodes.get(&node.id)
        {
            lines.push(Line::from(format!(
                "selected: {} → {:?}",
                node.id, outcome.status
            )));
            if let Some(output) = &outcome.output {
                lines.push(Line::from(format!("output: {output}")));
            }
            if let Some(error) = &outcome.error {
                lines.push(Line::from(format!("error: {error}")));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Runtime / results "),
            )
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

fn render_input(frame: &mut Frame, input: &InputState, area: Rect) {
    let title = match input.target {
        InputTarget::Task => " Task · Enter save / Esc cancel ",
        InputTarget::Model => " Model override · Enter save / blank clears ",
        InputTarget::NodePrompt => " Node prompt · Enter save / Esc cancel ",
    };
    let cursor = input.cursor.min(input.value.len());
    let (before, after) = input.value.split_at(cursor);
    let line = Line::from(vec![
        Span::raw(before),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
        Span::raw(after),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), inner);
}

fn render_gate(frame: &mut Frame, gate: &GateEnvelope, area: Rect) {
    let default = match gate.request.default {
        GateDecision::Approve => "approve",
        GateDecision::Reject => "reject",
    };
    let content = vec![
        Line::from(Span::styled(
            format!("Node: {}", gate.request.node_id),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(gate.request.message.clone()),
        Line::from(""),
        Line::from(format!("y approve   n reject   Enter default ({default})")),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Human gate ");
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(content))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
) -> Result<()> {
    loop {
        app.drain_run_events().await;
        terminal.draw(|frame| render(frame, &app))?;
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            match app.handle_key(key) {
                Action::Continue => {}
                Action::Quit => break,
                Action::Save => {
                    if let Err(error) = app.save().await {
                        app.status = error.to_string();
                    }
                }
            }
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

pub async fn launch(repo: PathBuf, trust_project_profiles: bool) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(anyhow!("graph TUI requires an interactive terminal"));
    }
    let guard = TerminalGuard;
    let mut terminal = setup_terminal()?;
    let result = match App::new(repo, trust_project_profiles) {
        Ok(app) => run_loop(&mut terminal, app).await,
        Err(error) => Err(error),
    };
    let restore = restore_terminal(&mut terminal);
    drop(guard);
    result.and(restore)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_labels_are_stable_for_help_and_status() {
        assert_eq!(template_label(GraphTemplate::Direct), "direct");
        assert_eq!(
            template_label(GraphTemplate::ReviewFixLoop),
            "review-fix-loop"
        );
    }

    #[test]
    fn data_edge_cycle_is_rejected_without_mutating_graph() {
        let mut graph = wizard::template_graph(
            "work",
            "task",
            GraphTemplate::Direct,
            Some("task".to_owned()),
            None,
            None,
        );
        graph.spec.nodes.push(Node::agent("second", "second"));
        graph.spec.edges.push(Edge::data("request", "second"));
        let mut candidate = graph.clone();
        candidate.spec.edges.push(Edge::data("second", "request"));
        assert!(
            candidate
                .validate()
                .iter()
                .any(|issue| issue.severity == gloop_core::IssueSeverity::Error)
        );
        assert_eq!(graph.spec.edges.len(), 1);
    }
}
