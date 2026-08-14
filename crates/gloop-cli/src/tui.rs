//! Resident terminal UI for selecting a graph, provider profile, and model.
//!
//! The TUI owns only presentation state. Graph validation, provider invocation,
//! scheduling, retries, artifacts, and journal persistence remain in the
//! existing core/provider/runtime crates.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use gloop_core::{Edge, Graph, Node, NodeKind, NodeStatus, PromptSpec, RunEventKind, RunSummary};
use gloop_provider::{PROJECT_CONFIG_PATH, ProfileStore, ProviderRegistry};
use gloop_runtime::{
    GateDecision, GateRequest, HumanGate, ProgressEvent, RunOptions, Runtime,
    replay_journal_partial,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs},
};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

use crate::{
    atomic_write::{self, write_text_atomic_if_unchanged_sync},
    commands::{apply_model_to_agent_nodes, build_profile_choices, clear_model_on_agent_nodes},
    i18n::{Language, Strings, fill},
    templates,
    wizard::{self, EditorState, GraphTemplate, ProfileChoice},
};

const TEMPLATE_CHOICES: [GraphTemplate; 8] = [
    GraphTemplate::Direct,
    GraphTemplate::PlanImplementVerify,
    GraphTemplate::ParallelResearchReduce,
    GraphTemplate::ReviewFixLoop,
    GraphTemplate::DesignWallBounce,
    GraphTemplate::Council,
    GraphTemplate::DecomposeFanoutReduce,
    GraphTemplate::ImplementTestLoop,
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

    fn title(self, strings: &Strings) -> &'static str {
        match self {
            Self::Overview => strings.screen_overview,
            Self::Builder => strings.screen_builder,
            Self::Run => strings.screen_run,
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

impl InputTarget {
    const fn multiline(self) -> bool {
        !matches!(self, Self::Model)
    }
}

/// Multi-line text editor state. `cursor` is a byte offset on a character
/// boundary; `preferred_col` keeps the horizontal position across vertical
/// moves. Rendering derives the visible window from the cursor position.
#[derive(Debug)]
struct InputState {
    target: InputTarget,
    value: String,
    cursor: usize,
    preferred_col: Option<usize>,
}

#[derive(Debug)]
struct TemplatePreview {
    template: GraphTemplate,
    shape: String,
    nodes: usize,
    edges: usize,
}

#[derive(Debug)]
enum Modal {
    Input(InputState),
    TemplatePicker {
        selected: usize,
        previews: Vec<TemplatePreview>,
    },
    ProfilePicker {
        selected: usize,
    },
    Issues {
        lines: Vec<String>,
        offset: usize,
    },
    Help {
        offset: usize,
    },
    Output {
        node: String,
        lines: Vec<String>,
        offset: usize,
    },
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
    cancellation: CancellationToken,
}

#[async_trait]
impl HumanGate for TuiGate {
    async fn decide(&self, request: GateRequest) -> std::result::Result<GateDecision, String> {
        let (reply, decision) = oneshot::channel();
        self.requests
            .send(GateEnvelope { request, reply })
            .map_err(|_| "TUI gate closed".to_owned())?;
        tokio::select! {
            biased;
            result = decision => result.map_err(|_| "TUI gate response was closed".to_owned()),
            () = self.cancellation.cancelled() => Err("TUI gate cancelled".to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Continue,
    Quit,
    Save,
    Run,
    OpenOutput,
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
struct App {
    repo: PathBuf,
    graph_path: PathBuf,
    expected_sha256: Option<String>,
    create_only: bool,
    trust_project_profiles: bool,
    lang: Language,
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
    modal: Option<Modal>,
    active_run: Option<ActiveRun>,
    pending_gates: VecDeque<GateEnvelope>,
    node_status: HashMap<String, NodeStatus>,
    last_summary: Option<RunSummary>,
    last_run_id: Option<String>,
    announced_run: bool,
    events: VecDeque<String>,
    status: String,
    /// Graph differs from what is saved on disk.
    dirty: bool,
    /// The node list was edited manually (or loaded from disk), so rebuilding
    /// from a template would replace user content. Task edits then update the
    /// goal only instead of rebuilding.
    manual_edits: bool,
}

impl App {
    fn new(repo: PathBuf, trust_project_profiles: bool, lang: Language) -> Result<Self> {
        let profiles = build_profile_choices(&repo, trust_project_profiles).map_err(|error| {
            anyhow!(
                "{}",
                fill(
                    lang.strings().status_load_profiles_failed,
                    &[("error", &error.to_string())]
                )
            )
        })?;
        let graph_path = templates::graph_path(&repo, "work");
        templates::ensure_managed_directory(&repo, Path::new(templates::GRAPHS_DIR))
            .map_err(|error| anyhow!("managed graph directory is unsafe: {error}"))?;
        let (graph, expected_sha256, create_only) = match fs::symlink_metadata(&graph_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(anyhow!(
                    "graph save target is a symlink: {}",
                    graph_path.display()
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(anyhow!(
                    "graph save target is not a regular file: {}",
                    graph_path.display()
                ));
            }
            Ok(_) => {
                let graph = Graph::from_path(&graph_path)
                    .map_err(|error| anyhow!("failed to load {}: {error}", graph_path.display()))?;
                let expected_sha256 = file_sha256(&graph_path)?;
                (graph, Some(expected_sha256), false)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let template = GraphTemplate::Direct;
                let task = DEFAULT_TASK.to_owned();
                let mut graph = Self::build_graph(template, &task, None, None);
                graph.metadata.name = String::from("work");
                graph.spec.goal.clone_from(&task);
                (graph, None, true)
            }
            Err(error) => {
                return Err(anyhow!(
                    "failed to inspect graph save target {}: {error}",
                    graph_path.display()
                ));
            }
        };
        let task = graph.spec.goal.clone();
        let profile_index = graph
            .spec
            .nodes
            .iter()
            .find_map(Node::profile)
            .and_then(|name| {
                profiles
                    .iter()
                    .position(|profile| profile.enabled && profile.name == name)
            })
            .or_else(|| profiles.iter().position(|profile| profile.enabled));
        let model = graph
            .spec
            .nodes
            .iter()
            .find_map(Node::model)
            .map(str::to_owned);
        let template = GraphTemplate::Direct;
        // A graph loaded from disk is treated as user content: applying a
        // template replaces it, so the pickers warn about it.
        let manual_edits = expected_sha256.is_some();

        Ok(Self {
            repo,
            graph_path,
            expected_sha256,
            create_only,
            trust_project_profiles,
            lang,
            screen: Screen::Overview,
            graph,
            template,
            template_index: 0,
            task,
            model,
            profiles,
            profile_index,
            selected_node: 0,
            connect_from: None,
            modal: None,
            active_run: None,
            pending_gates: VecDeque::new(),
            node_status: HashMap::new(),
            last_summary: None,
            last_run_id: None,
            announced_run: false,
            events: VecDeque::new(),
            status: lang.strings().status_ready.to_owned(),
            dirty: false,
            manual_edits,
        })
    }

    fn strings(&self) -> &'static Strings {
        self.lang.strings()
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

    /// Rebuild the draft from the current template/task/profile/model.
    /// Manual node edits are replaced; callers surface that through the
    /// picker warning and the post-apply status message.
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
        self.manual_edits = false;
    }

    fn apply_template(&mut self, index: usize) {
        let had_edits = self.manual_edits;
        self.template_index = index;
        self.template = TEMPLATE_CHOICES[index];
        self.rebuild_from_choices();
        let strings = self.strings();
        let template = match self.template {
            GraphTemplate::Direct => strings.template_desc_direct,
            GraphTemplate::PlanImplementVerify => strings.template_desc_plan_implement_verify,
            GraphTemplate::ParallelResearchReduce => strings.template_desc_parallel_research_reduce,
            GraphTemplate::ReviewFixLoop => strings.template_desc_review_fix_loop,
            GraphTemplate::DesignWallBounce => strings.template_desc_design_wall_bounce,
            GraphTemplate::Council => strings.template_desc_council,
            GraphTemplate::DecomposeFanoutReduce => strings.template_desc_decompose_fanout_reduce,
            GraphTemplate::ImplementTestLoop => strings.template_desc_implement_test_loop,
        };
        let shape = graph_shape(&self.graph);
        let key = if had_edits {
            strings.status_template_edits_discarded
        } else {
            strings.status_template_applied
        };
        self.status = fill(
            key,
            &[
                ("name", template_label(self.template)),
                ("shape", &format!("{shape} ({template})")),
            ],
        );
    }

    fn apply_profile(&mut self, index: usize) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        if !profile.enabled {
            return;
        }
        let had_edits = self.manual_edits;
        self.profile_index = Some(index);
        self.rebuild_from_choices();
        let strings = self.strings();
        let mut status = fill(
            strings.status_profile_applied,
            &[("name", self.selected_profile().unwrap_or_default())],
        );
        if had_edits {
            status.push(' ');
            status.push_str(strings.status_edits_discarded_note);
        }
        self.status = status;
    }

    fn open_template_picker(&mut self) {
        let previews = TEMPLATE_CHOICES
            .iter()
            .map(|template| {
                let graph = Self::build_graph(
                    *template,
                    &self.task,
                    self.selected_profile(),
                    self.model.as_deref(),
                );
                TemplatePreview {
                    template: *template,
                    shape: graph_shape(&graph),
                    nodes: graph.spec.nodes.len(),
                    edges: graph.spec.edges.len(),
                }
            })
            .collect();
        self.modal = Some(Modal::TemplatePicker {
            selected: self.template_index,
            previews,
        });
    }

    fn open_profile_picker(&mut self) {
        let selected = self
            .profile_index
            .unwrap_or(0)
            .min(self.profiles.len().saturating_sub(1));
        self.modal = Some(Modal::ProfilePicker { selected });
    }

    fn validate_and_show_issues(&mut self) {
        let issues = self.graph.validate();
        let strings = self.strings();
        let errors = issues
            .iter()
            .filter(|issue| issue.severity == gloop_core::IssueSeverity::Error)
            .count();
        let warnings = issues.len() - errors;
        self.status = if errors == 0 {
            fill(
                strings.status_graph_valid,
                &[("warnings", &warnings.to_string())],
            )
        } else {
            fill(
                strings.status_graph_invalid,
                &[
                    ("errors", &errors.to_string()),
                    ("warnings", &warnings.to_string()),
                ],
            )
        };
        let lines = if issues.is_empty() {
            vec![strings.issues_none.to_owned()]
        } else {
            issues
                .iter()
                .map(|issue| {
                    let severity = if issue.severity == gloop_core::IssueSeverity::Error {
                        strings.issue_error
                    } else {
                        strings.issue_warning
                    };
                    format!("[{severity}] {} — {}", issue.path, issue.message)
                })
                .collect()
        };
        self.modal = Some(Modal::Issues { lines, offset: 0 });
    }

    fn open_help(&mut self) {
        self.modal = Some(Modal::Help { offset: 0 });
    }

    fn toggle_language(&mut self) {
        self.lang = match self.lang {
            Language::En => Language::Ja,
            Language::Ja => Language::En,
        };
        self.status = fill(
            self.strings().status_language,
            &[("lang", self.lang.as_str())],
        );
    }

    fn begin_input(&mut self, target: InputTarget) {
        let value = match target {
            InputTarget::Task => self.task.clone(),
            InputTarget::Model => self.model.clone().unwrap_or_default(),
            InputTarget::NodePrompt => {
                let Some(node) = self.graph.spec.nodes.get(self.selected_node) else {
                    self.set_status(self.strings().status_no_node);
                    return;
                };
                let Some(prompt) = node_prompt(node) else {
                    self.set_status(self.strings().status_not_prompt_node);
                    return;
                };
                prompt.to_owned()
            }
        };
        let cursor = value.len();
        self.modal = Some(Modal::Input(InputState {
            target,
            value,
            cursor,
            preferred_col: None,
        }));
    }

    fn commit_input(&mut self) {
        let Some(Modal::Input(input)) = self.modal.take() else {
            return;
        };
        let value = input.value.trim().to_owned();
        match input.target {
            InputTarget::Task => {
                if value.is_empty() {
                    self.set_status(self.strings().status_task_empty);
                    return;
                }
                self.task.clone_from(&value);
                if self.manual_edits {
                    self.graph.spec.goal.clone_from(&value);
                    self.dirty = true;
                    self.set_status(self.strings().status_task_goal_only);
                } else {
                    self.rebuild_from_choices();
                    self.set_status(self.strings().status_task_updated);
                }
            }
            InputTarget::Model => {
                if value.is_empty() {
                    self.model = None;
                    clear_model_on_agent_nodes(&mut self.graph);
                } else {
                    self.model = Some(value.clone());
                    apply_model_to_agent_nodes(&mut self.graph, &value);
                }
                self.dirty = true;
                self.status = fill(
                    self.strings().status_model_override,
                    &[(
                        "model",
                        self.model
                            .as_deref()
                            .unwrap_or_else(|| self.strings().provider_default),
                    )],
                );
            }
            InputTarget::NodePrompt => {
                let Some(node) = self.graph.spec.nodes.get_mut(self.selected_node) else {
                    self.set_status(self.strings().status_no_node);
                    return;
                };
                match &mut node.kind {
                    NodeKind::Agent { prompt, .. }
                    | NodeKind::Reduce { prompt, .. }
                    | NodeKind::Synthesize { prompt, .. } => {
                        *prompt = PromptSpec::Inline(value);
                        self.dirty = true;
                        self.manual_edits = true;
                        self.set_status(self.strings().status_node_prompt_updated);
                    }
                    _ => self.set_status(self.strings().status_not_prompt_node),
                }
            }
        }
    }

    fn handle_paste(&mut self, text: &str) {
        let Some(Modal::Input(input)) = self.modal.as_mut() else {
            return;
        };
        input.value.insert_str(input.cursor, text);
        input.cursor += text.len();
        input.preferred_col = None;
    }
}

/// Byte offset of the character preceding `cursor` (character-boundary safe).
fn char_prev(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

/// Byte offset of the character following `cursor` (character-boundary safe).
fn char_next(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(index, _)| cursor + index)
}

/// Byte offsets of every logical line start.
fn line_starts(value: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, character) in value.char_indices() {
        if character == '\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// `(line, column)` of a byte offset, column counted in characters.
fn line_col_at(value: &str, cursor: usize) -> (usize, usize) {
    let starts = line_starts(value);
    let line = starts
        .iter()
        .rposition(|start| *start <= cursor)
        .unwrap_or(0);
    let col = value[starts[line]..cursor].chars().count();
    (line, col)
}

/// Move the cursor one logical line up or down, keeping the preferred column.
fn move_vertical(
    value: &str,
    cursor: usize,
    delta: isize,
    preferred_col: &mut Option<usize>,
) -> usize {
    let starts = line_starts(value);
    let (line, col) = line_col_at(value, cursor);
    let target_col = preferred_col.unwrap_or(col);
    let next_line = if delta.is_negative() {
        let Some(next) = line.checked_sub(delta.unsigned_abs()) else {
            return cursor;
        };
        next
    } else {
        (line + delta.unsigned_abs()).min(starts.len() - 1)
    };
    let line_start = starts[next_line];
    let line_end = if next_line + 1 < starts.len() {
        starts[next_line + 1] - 1
    } else {
        value.len()
    };
    let mut offset = line_start;
    let mut column = 0usize;
    for (index, _) in value[line_start..line_end].char_indices() {
        if column == target_col {
            offset = line_start + index;
            break;
        }
        column += 1;
        offset = line_start
            + index
            + value[line_start + index..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
    }
    if column < target_col {
        offset = line_end;
    }
    *preferred_col = Some(target_col);
    offset
}

/// One soft-wrapped display row.
#[derive(Debug)]
struct DisplayRow {
    text: String,
}

/// Soft-wrap one logical line into display rows of at most `width` columns,
/// using terminal character widths (CJK characters count as two columns).
fn wrap_line(line: &str, width: usize) -> Vec<DisplayRow> {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let limit = width.max(1);
    for character in line.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0).max(1);
        if current_width + character_width > limit && !current.is_empty() {
            rows.push(DisplayRow { text: current });
            current = String::new();
            current_width = 0;
        }
        current.push(character);
        current_width += character_width;
    }
    rows.push(DisplayRow { text: current });
    rows
}

impl App {
    fn handle_input_key(&mut self, key: KeyEvent) {
        let Some(Modal::Input(input)) = self.modal.as_mut() else {
            return;
        };
        let multiline = input.target.multiline();
        match key.code {
            KeyCode::Esc => {
                self.modal = None;
                self.set_status(self.strings().status_edit_cancelled);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.modal = None;
                self.set_status(self.strings().status_edit_cancelled);
            }
            KeyCode::Char('s' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.commit_input();
            }
            KeyCode::Enter => {
                let newline_modifier = key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
                if multiline && newline_modifier {
                    input.value.insert(input.cursor, '\n');
                    input.cursor += 1;
                    input.preferred_col = Some(0);
                } else {
                    self.commit_input();
                }
            }
            KeyCode::Backspace => {
                if input.cursor > 0 {
                    let previous = char_prev(&input.value, input.cursor);
                    input.value.drain(previous..input.cursor);
                    input.cursor = previous;
                    input.preferred_col = None;
                }
            }
            KeyCode::Delete => {
                if input.cursor < input.value.len() {
                    let next = char_next(&input.value, input.cursor);
                    input.value.drain(input.cursor..next);
                    input.preferred_col = None;
                }
            }
            KeyCode::Left => {
                input.cursor = char_prev(&input.value, input.cursor);
                input.preferred_col = None;
            }
            KeyCode::Right => {
                input.cursor = char_next(&input.value, input.cursor);
                input.preferred_col = None;
            }
            KeyCode::Up if multiline => {
                input.cursor =
                    move_vertical(&input.value, input.cursor, -1, &mut input.preferred_col);
            }
            KeyCode::Down if multiline => {
                input.cursor =
                    move_vertical(&input.value, input.cursor, 1, &mut input.preferred_col);
            }
            KeyCode::Home => {
                input.cursor = 0;
                input.preferred_col = None;
            }
            KeyCode::End => {
                input.cursor = input.value.len();
                input.preferred_col = None;
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                input.value.insert(input.cursor, character);
                input.cursor += character.len_utf8();
                input.preferred_col = None;
            }
            _ => {}
        }
    }

    fn scroll_modal(&mut self, delta: isize) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        let (offset, len) = match modal {
            Modal::Issues { lines, offset } | Modal::Output { lines, offset, .. } => {
                (offset, lines.len())
            }
            Modal::Help { offset } => (offset, help_line_count(self.lang)),
            _ => return,
        };
        let magnitude = delta.unsigned_abs();
        let next = if delta.is_negative() {
            offset.saturating_sub(magnitude)
        } else {
            (*offset + magnitude).min(len.saturating_sub(1))
        };
        *offset = next;
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Action {
        match self.modal.as_mut() {
            Some(Modal::Input(_)) => {
                self.handle_input_key(key);
                Action::Continue
            }
            Some(Modal::TemplatePicker { selected, previews }) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.modal = None;
                    Action::Continue
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.modal = None;
                    Action::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    Action::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(previews.len().saturating_sub(1));
                    Action::Continue
                }
                KeyCode::Enter => {
                    let index = *selected;
                    self.modal = None;
                    self.apply_template(index);
                    Action::Continue
                }
                _ => Action::Continue,
            },
            Some(Modal::ProfilePicker { selected }) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.modal = None;
                    Action::Continue
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.modal = None;
                    Action::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    Action::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(self.profiles.len().saturating_sub(1));
                    Action::Continue
                }
                KeyCode::Enter => {
                    let index = *selected;
                    self.modal = None;
                    if self
                        .profiles
                        .get(index)
                        .is_some_and(|profile| profile.enabled)
                    {
                        self.apply_profile(index);
                    } else {
                        self.set_status(self.strings().status_profile_none);
                    }
                    Action::Continue
                }
                _ => Action::Continue,
            },
            Some(Modal::Issues { .. } | Modal::Help { .. } | Modal::Output { .. }) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.modal = None;
                        Action::Continue
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.modal = None;
                        Action::Continue
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.scroll_modal(-1);
                        Action::Continue
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.scroll_modal(1);
                        Action::Continue
                    }
                    KeyCode::PageUp => {
                        self.scroll_modal(-10);
                        Action::Continue
                    }
                    KeyCode::PageDown => {
                        self.scroll_modal(10);
                        Action::Continue
                    }
                    _ => Action::Continue,
                }
            }
            None => Action::Continue,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if matches!(self.modal, Some(Modal::Input(_))) {
            return self.handle_modal_key(key);
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.modal.is_some() {
                self.modal = None;
                return Action::Continue;
            }
            if let Some(active) = &self.active_run {
                active.cancellation.cancel();
                self.pending_gates.clear();
                self.set_status(self.strings().status_cancel_requested);
            } else {
                return Action::Quit;
            }
            return Action::Continue;
        }
        if self.active_run.is_some() && !self.pending_gates.is_empty() {
            self.modal = None;
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
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        if self.active_run.is_some() {
            match key.code {
                KeyCode::Char('1') => self.screen = Screen::Overview,
                KeyCode::Char('2') => self.screen = Screen::Builder,
                KeyCode::Char('3') => self.screen = Screen::Run,
                KeyCode::Tab => self.next_screen(),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Char('o') => return Action::OpenOutput,
                KeyCode::Char('?') => self.open_help(),
                KeyCode::Char('l') => self.toggle_language(),
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
                self.open_template_picker();
                Action::Continue
            }
            KeyCode::Char('p') => {
                self.open_profile_picker();
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
            KeyCode::Char('r') => Action::Run,
            KeyCode::Char('s') => Action::Save,
            KeyCode::Char('v') => {
                self.validate_and_show_issues();
                Action::Continue
            }
            KeyCode::Char('o') => Action::OpenOutput,
            KeyCode::Char('?') => {
                self.open_help();
                Action::Continue
            }
            KeyCode::Char('l') => {
                self.toggle_language();
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
                self.set_status(self.strings().status_connect_cancelled);
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
                self.status = fill(
                    self.strings().status_add_node_failed,
                    &[("error", &error.to_string())],
                );
                return;
            }
        };
        if let Some(previous) = self.graph.spec.nodes.last() {
            state = match wizard::add_edge_to_editor(&state, Edge::data(&previous.id, &id)) {
                Ok(state) => state,
                Err(error) => {
                    self.status = fill(
                        self.strings().status_connect_new_failed,
                        &[("error", &error.to_string())],
                    );
                    return;
                }
            };
        }
        self.graph = state.graph;
        self.selected_node = self.graph.spec.nodes.len() - 1;
        self.dirty = true;
        self.manual_edits = true;
        self.status = fill(self.strings().status_added_node, &[("id", &id)]);
    }

    fn remove_node(&mut self) {
        if self.graph.spec.nodes.len() <= 1 {
            self.set_status(self.strings().status_keep_one_node);
            return;
        }
        let removed = self.graph.spec.nodes[self.selected_node].id.clone();
        let state = EditorState::from_graph(self.graph.clone(), 0);
        let state = match wizard::remove_node_from_editor(&state, &removed) {
            Ok(state) => state,
            Err(error) => {
                self.status = fill(
                    self.strings().status_remove_failed,
                    &[("id", &removed), ("error", &error.to_string())],
                );
                return;
            }
        };
        self.graph = state.graph;
        self.selected_node = self
            .selected_node
            .min(self.graph.spec.nodes.len().saturating_sub(1));
        self.dirty = true;
        self.manual_edits = true;
        self.status = fill(self.strings().status_removed_node, &[("id", &removed)]);
    }

    fn begin_connection(&mut self) {
        let Some(node) = self.graph.spec.nodes.get(self.selected_node) else {
            return;
        };
        self.connect_from = Some(node.id.clone());
        self.status = fill(self.strings().status_connecting, &[("id", &node.id)]);
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
            self.set_status(self.strings().status_self_connect);
            return;
        }
        let state = EditorState::from_graph(self.graph.clone(), 0);
        let state = match wizard::add_edge_to_editor(&state, Edge::data(&from, &to)) {
            Ok(state) => state,
            Err(error) => {
                self.status = fill(
                    self.strings().status_connect_rejected,
                    &[("error", &error.to_string())],
                );
                return;
            }
        };
        self.graph = state.graph;
        self.dirty = true;
        self.manual_edits = true;
        self.status = fill(
            self.strings().status_edge_added,
            &[("from", &from), ("to", &to)],
        );
    }

    fn start_run(&mut self) {
        if self.active_run.is_some() {
            self.set_status(self.strings().status_run_active);
            return;
        }
        let issues = self.graph.validate();
        if issues
            .iter()
            .any(|issue| issue.severity == gloop_core::IssueSeverity::Error)
        {
            self.set_status(self.strings().status_run_blocked_invalid);
            return;
        }
        let graph = self.graph.clone();
        let repo = self.repo.clone();
        let trust_project_profiles = self.trust_project_profiles;
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let gate_cancellation = cancellation.clone();
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
            .with_human_gate(Arc::new(TuiGate {
                requests: gate_tx,
                cancellation: gate_cancellation,
            }));
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
        self.last_run_id = None;
        self.announced_run = false;
        self.active_run = Some(ActiveRun {
            cancellation,
            gates,
            progress,
            task,
        });
        self.screen = Screen::Run;
        self.set_status(self.strings().status_run_started);
    }

    async fn drain_run_events(&mut self) {
        let mut pending = Vec::new();
        let mut gates = Vec::new();
        let (finished, cancelled) = {
            let Some(active) = self.active_run.as_mut() else {
                return;
            };
            while let Ok(event) = active.progress.try_recv() {
                pending.push(event);
            }
            while let Ok(gate) = active.gates.try_recv() {
                gates.push(gate);
            }
            (
                active.task.is_finished(),
                active.cancellation.is_cancelled(),
            )
        };
        for event in pending {
            self.apply_progress(&event);
        }
        for gate in gates {
            self.pending_gates.push_back(gate);
        }
        if cancelled {
            self.pending_gates.clear();
        } else if !self.pending_gates.is_empty() {
            self.set_status(self.strings().status_gate_waiting);
        }
        if !finished {
            return;
        }
        let ActiveRun {
            progress,
            gates,
            task,
            ..
        } = self.active_run.take().expect("active run exists");
        let result = task.await;
        let mut progress = progress;
        while let Ok(event) = progress.try_recv() {
            self.apply_progress(&event);
        }
        let mut gates = gates;
        while gates.try_recv().is_ok() {}
        self.pending_gates.clear();
        let strings = self.strings();
        match result {
            Ok(Ok(summary)) => {
                self.status = fill(
                    strings.status_run_finished,
                    &[("status", &format!("{:?}", summary.status))],
                );
                self.last_summary = Some(summary);
            }
            Ok(Err(error)) => {
                self.status = fill(strings.status_run_failed, &[("error", &error)]);
            }
            Err(error) => {
                self.status = fill(
                    strings.status_run_task_stopped,
                    &[("error", &error.to_string())],
                );
            }
        }
    }

    fn resolve_gate(&mut self, decision: GateDecision) {
        let Some(gate) = self.pending_gates.pop_front() else {
            return;
        };
        let node = gate.request.node_id;
        let _ = gate.reply.send(decision);
        let strings = self.strings();
        self.status = fill(
            match decision {
                GateDecision::Approve => strings.status_gate_approved,
                GateDecision::Reject => strings.status_gate_rejected,
            },
            &[("node", &node)],
        );
    }

    fn apply_progress(&mut self, event: &ProgressEvent) {
        if self.last_run_id.is_none() {
            self.last_run_id = Some(event.run_id.clone());
        }
        if !self.announced_run {
            self.announced_run = true;
            if let Some(run_id) = self.last_run_id.clone() {
                self.status = fill(self.strings().status_run_dir, &[("run_id", &run_id)]);
            }
        }
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
            event_kind_label(event.kind, self.lang),
            message
        );
        self.events.push_back(line);
        while self.events.len() > 80 {
            self.events.pop_front();
        }
    }

    async fn prepare_output(&mut self) {
        let Some(node) = self.graph.spec.nodes.get(self.selected_node) else {
            self.set_status(self.strings().status_no_node);
            return;
        };
        let node_id = node.id.clone();
        let mut lines: Vec<String> = Vec::new();
        if let Some(summary) = &self.last_summary
            && let Some(outcome) = summary.nodes.get(&node_id)
        {
            if let Some(output) = &outcome.output {
                lines.push(
                    serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string()),
                );
            }
            if let Some(error) = &outcome.error {
                lines.push(format!("error: {error}"));
            }
        }
        if lines.is_empty()
            && self.active_run.is_some()
            && let Some(run_id) = self.last_run_id.clone()
        {
            let journal_path = self
                .repo
                .join(PROJECT_CONFIG_PATH)
                .with_file_name("runs")
                .join(&run_id)
                .join("journal.jsonl");
            if let Ok(report) = replay_journal_partial(&journal_path).await
                && let Some(outcome) = report.nodes.get(&node_id)
                && let Some(output) = &outcome.output
            {
                lines.push(
                    serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string()),
                );
            }
            if lines.is_empty() {
                lines.push(fill(
                    self.strings().output_live_hint,
                    &[("run_id", &run_id)],
                ));
            }
        }
        if lines.is_empty() {
            lines.push(self.strings().output_unavailable.to_owned());
        }
        self.modal = Some(Modal::Output {
            node: node_id,
            lines,
            offset: 0,
        });
    }

    async fn save(&mut self) -> Result<()> {
        let issues = self.graph.validate();
        if issues
            .iter()
            .any(|issue| issue.severity == gloop_core::IssueSeverity::Error)
        {
            self.set_status(self.strings().status_save_blocked);
            return Ok(());
        }
        templates::ensure_managed_directory(&self.repo, Path::new(templates::GRAPHS_DIR))
            .map_err(|error| anyhow!("managed graph directory is unsafe: {error}"))?;
        let path = self.graph_path.clone();
        let yaml = self
            .graph
            .to_yaml()
            .map_err(|error| anyhow!(error.to_string()))?;
        let write_result = if let Some(expected_sha256) = self.expected_sha256.clone() {
            let path = path.clone();
            let yaml = yaml.clone();
            tokio::task::spawn_blocking(move || {
                write_text_atomic_if_unchanged_sync(&path, &expected_sha256, &yaml)
            })
            .await
            .map_err(|error| anyhow!("graph save task stopped: {error}"))?
        } else if self.create_only {
            atomic_write::write_text_no_replace(&path, &yaml).await
        } else {
            atomic_write::write_text_atomic(&path, &yaml).await
        };
        write_result.map_err(|error| anyhow!("failed to save graph: {error}"))?;
        self.expected_sha256 = Some(format!("{:x}", Sha256::digest(yaml.as_bytes())));
        self.create_only = false;
        self.dirty = false;
        self.status = fill(
            self.strings().status_saved,
            &[("path", &path.display().to_string())],
        );
        Ok(())
    }
}

fn profile_name(profiles: &[ProfileChoice], index: Option<usize>) -> Option<&str> {
    index
        .and_then(|index| profiles.get(index))
        .filter(|profile| profile.enabled)
        .map(|profile| profile.name.as_str())
}

fn file_sha256(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| anyhow!("failed to inspect graph before save: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(anyhow!("graph save target is not a regular file"));
    }
    let bytes =
        fs::read(path).map_err(|error| anyhow!("failed to read graph before save: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
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
        GraphTemplate::DesignWallBounce => "design-wall-bounce",
        GraphTemplate::Council => "council",
        GraphTemplate::DecomposeFanoutReduce => "decompose-fanout-reduce",
        GraphTemplate::ImplementTestLoop => "implement-test-loop",
    }
}

fn template_desc(strings: &Strings, template: GraphTemplate) -> &'static str {
    match template {
        GraphTemplate::Direct => strings.template_desc_direct,
        GraphTemplate::PlanImplementVerify => strings.template_desc_plan_implement_verify,
        GraphTemplate::ParallelResearchReduce => strings.template_desc_parallel_research_reduce,
        GraphTemplate::ReviewFixLoop => strings.template_desc_review_fix_loop,
        GraphTemplate::DesignWallBounce => strings.template_desc_design_wall_bounce,
        GraphTemplate::Council => strings.template_desc_council,
        GraphTemplate::DecomposeFanoutReduce => strings.template_desc_decompose_fanout_reduce,
        GraphTemplate::ImplementTestLoop => strings.template_desc_implement_test_loop,
    }
}

/// One-line description of what the graph actually is: a `a -> b -> c` chain
/// when the edges form one, otherwise the explicit edge list. This is the
/// answer to "what am I about to run?" for both pickers and the Overview.
fn graph_shape(graph: &Graph) -> String {
    let nodes = &graph.spec.nodes;
    let edges = &graph.spec.edges;
    if nodes.is_empty() {
        return "-".to_owned();
    }
    let ids: std::collections::HashSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    let mut indegree: HashMap<&str, usize> = HashMap::new();
    let mut next: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        if ids.contains(edge.from.as_str()) && ids.contains(edge.to.as_str()) {
            *indegree.entry(edge.to.as_str()).or_insert(0) += 1;
            next.entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
    }
    let chain_candidate = edges.len() + 1 == nodes.len()
        && indegree.values().all(|degree| *degree <= 1)
        && next.values().all(|targets| targets.len() <= 1);
    if chain_candidate {
        let heads: Vec<&str> = nodes
            .iter()
            .filter(|node| indegree.get(node.id.as_str()).copied().unwrap_or(0) == 0)
            .map(|node| node.id.as_str())
            .collect();
        if heads.len() == 1 {
            let mut chain = vec![heads[0]];
            let mut current = heads[0];
            while let Some(target) = next.get(current).and_then(|targets| targets.first()) {
                chain.push(target);
                current = target;
            }
            if chain.len() == nodes.len() {
                return chain.join(" -> ");
            }
        }
    }
    edges
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>()
        .join(", ")
}

fn event_kind_label(kind: RunEventKind, lang: Language) -> &'static str {
    let strings = lang.strings();
    match kind {
        RunEventKind::RunStarted => strings.event_run_started,
        RunEventKind::NodeReady => strings.event_ready,
        RunEventKind::NodeStarted => strings.event_running,
        RunEventKind::NodeOutput => strings.event_output,
        RunEventKind::NodeSucceeded => strings.event_succeeded,
        RunEventKind::NodeFailed => strings.event_failed,
        RunEventKind::NodeSkipped => strings.event_skipped,
        RunEventKind::NodeBlocked => strings.event_blocked,
        RunEventKind::RetryScheduled => strings.event_retry,
        RunEventKind::LoopStarted => strings.event_loop_started,
        RunEventKind::LoopIterationStarted => strings.event_iteration,
        RunEventKind::LoopIterationFinished => strings.event_iteration_done,
        RunEventKind::LoopFinished => strings.event_loop_done,
        RunEventKind::RunCancelled => strings.event_cancelled,
        RunEventKind::RunFinished => strings.event_run_finished,
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

fn node_status_label(status: NodeStatus, lang: Language) -> &'static str {
    let strings = lang.strings();
    match status {
        NodeStatus::Pending => strings.node_pending,
        NodeStatus::Ready => strings.node_ready,
        NodeStatus::Running => strings.node_running,
        NodeStatus::Succeeded => strings.node_succeeded,
        NodeStatus::Failed => strings.node_failed,
        NodeStatus::Skipped => strings.node_skipped,
        NodeStatus::Blocked => strings.node_blocked,
        NodeStatus::Cancelled => strings.node_cancelled,
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

fn help_lines(lang: Language) -> Vec<String> {
    let s = lang.strings();
    vec![
        s.help_intro.to_owned(),
        String::new(),
        s.help_step_task.to_owned(),
        s.help_step_bindings.to_owned(),
        s.help_step_builder.to_owned(),
        s.help_step_validate.to_owned(),
        s.help_step_run.to_owned(),
        s.help_step_monitor.to_owned(),
        String::new(),
        s.help_step_status.to_owned(),
        String::new(),
        format!("{}:", s.help_keys_title),
        format!("1/2/3 — {}", s.key_screens),
        format!("i — {}  ·  m — {}", s.key_task, s.key_model),
        format!("t — {}  ·  p — {}", s.key_template, s.key_profile),
        format!("v — {}  ·  s — {}", s.key_validate, s.key_save),
        format!("r — {}  ·  o — output", s.key_run),
        format!("? — {}  ·  l — {}", s.key_help, s.key_lang),
        format!("q — {}", s.key_quit),
    ]
}

fn help_line_count(lang: Language) -> usize {
    help_lines(lang).len()
}

#[allow(clippy::too_many_lines)]
fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let strings = app.strings();
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
        .map(|screen| Line::from(format!(" {} ", screen.title(strings))))
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

    let footer_keys = vec![
        Span::styled("1/2/3", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_screens)),
        Span::styled("i", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_task)),
        Span::styled("t", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_template)),
        Span::styled("p", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_profile)),
        Span::styled("m", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_model)),
        Span::styled("v", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_validate)),
        Span::styled("r", Style::default().fg(Color::Green)),
        Span::raw(format!(" {}  ", strings.key_run)),
        Span::styled("s", Style::default().fg(Color::Yellow)),
        Span::raw(format!(" {}  ", strings.key_save)),
        Span::styled("?", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_help)),
        Span::styled("l", Style::default().fg(Color::Cyan)),
        Span::raw(format!(" {}  ", strings.key_lang)),
        Span::styled("q", Style::default().fg(Color::Red)),
        Span::raw(format!(" {}", strings.key_quit)),
    ];
    let footer = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&app.status),
        ]),
        Line::from(footer_keys),
    ]))
    .block(Block::default().borders(Borders::TOP));
    frame.render_widget(footer, root[2]);

    match &app.modal {
        Some(Modal::Input(input)) => {
            let height = input_height(input, area);
            render_input(frame, input, strings, centered_rect(90, height, area));
        }
        Some(Modal::TemplatePicker { selected, previews }) => {
            render_template_picker(frame, app, *selected, previews, centered_rect(84, 17, area));
        }
        Some(Modal::ProfilePicker { selected }) => {
            render_profile_picker(frame, app, *selected, centered_rect(70, 15, area));
        }
        Some(Modal::Issues { lines, offset }) => {
            render_scroll_overlay(
                frame,
                strings.issues_title,
                lines,
                *offset,
                centered_rect(84, 15, area),
                Color::Red,
            );
        }
        Some(Modal::Help { offset }) => {
            let lines = help_lines(app.lang);
            render_scroll_overlay(
                frame,
                strings.help_title,
                &lines,
                *offset,
                centered_rect(84, 21, area),
                Color::Cyan,
            );
        }
        Some(Modal::Output {
            node,
            lines,
            offset,
        }) => {
            let title = fill(strings.output_title, &[("node", node)]);
            render_scroll_overlay(
                frame,
                &title,
                lines,
                *offset,
                centered_rect(84, 21, area),
                Color::Green,
            );
        }
        None => {}
    }
    if let Some(gate) = app.pending_gates.front() {
        render_gate(frame, gate, strings, centered_rect(70, 9, area));
    }
}

#[allow(clippy::too_many_lines)]
fn render_overview(frame: &mut Frame, app: &App, area: Rect) {
    let strings = app.strings();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);
    let selected_profile = app.selected_profile().unwrap_or(strings.runtime_default);
    let model = app.model.as_deref().unwrap_or(strings.provider_default);
    let save_line = if app.dirty {
        format!("{} ({})", app.graph_path.display(), strings.label_dirty)
    } else {
        app.graph_path.display().to_string()
    };
    let label_style = Style::default().fg(Color::DarkGray);
    let summary = vec![
        Line::from(Span::styled(
            strings.start_here,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("{:<10} ", strings.label_task), label_style),
            Span::raw(app.task.lines().next().unwrap_or_default().to_owned()),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10} ", strings.label_template), label_style),
            Span::raw(template_label(app.template)),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10} ", strings.label_profile), label_style),
            Span::raw(selected_profile.to_owned()),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10} ", strings.label_model), label_style),
            Span::raw(model.to_owned()),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10} ", strings.label_graph), label_style),
            Span::raw(fill(
                strings.nodes_edges,
                &[
                    ("nodes", &app.graph.spec.nodes.len().to_string()),
                    ("edges", &app.graph.spec.edges.len().to_string()),
                ],
            )),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10} ", strings.label_shape), label_style),
            Span::styled(
                graph_shape(&app.graph),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<10} ", strings.label_save), label_style),
            Span::raw(save_line),
        ]),
        Line::from(""),
        Line::from(strings.guide_task),
        Line::from(strings.guide_bindings),
        Line::from(strings.guide_builder),
        Line::from(strings.guide_run),
        Line::from(strings.guide_help),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(summary))
            .block(Block::default().borders(Borders::ALL).title(" Selection "))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        columns[0],
    );

    let mut flow = app
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
    if !app.graph.spec.edges.is_empty() {
        flow.push(ListItem::new(Line::from(Span::styled(
            format!(" {}", strings.edges_header),
            Style::default().fg(Color::DarkGray),
        ))));
        for edge in &app.graph.spec.edges {
            flow.push(ListItem::new(Line::from(Span::styled(
                format!("   {} -> {}", edge.from, edge.to),
                Style::default().fg(Color::DarkGray),
            ))));
        }
    }
    frame.render_widget(
        List::new(flow).block(
            Block::default()
                .borders(Borders::ALL)
                .title(strings.graph_flow_title),
        ),
        columns[1],
    );
}

#[allow(clippy::too_many_lines)]
fn render_builder(frame: &mut Frame, app: &App, area: Rect) {
    let strings = app.strings();
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
        fill(strings.builder_connecting, &[("id", from)])
    } else {
        strings.builder_nodes_title.to_owned()
    };
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
        columns[0],
    );

    let Some(node) = app.graph.spec.nodes.get(app.selected_node) else {
        frame.render_widget(
            Paragraph::new(strings.builder_no_node).block(Block::default().borders(Borders::ALL)),
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
    let prompt = node_prompt(node).unwrap_or(strings.builder_not_prompt);
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
        Line::from(format!(
            "{}: {}",
            strings.builder_label_kind,
            node_kind_label(node)
        )),
        Line::from(format!(
            "{}: {}",
            strings.builder_label_profile,
            node.profile().unwrap_or(strings.runtime_default)
        )),
        Line::from(format!(
            "{}: {}",
            strings.builder_label_model,
            node.model().unwrap_or(strings.provider_default)
        )),
        Line::from(format!(
            "{}: {}",
            strings.builder_label_retry, node.retry.max_attempts
        )),
        Line::from(format!(
            "{}: {}",
            strings.builder_label_fanout,
            node.fan_out()
        )),
        Line::from(""),
        Line::from(Span::styled(
            strings.builder_label_prompt,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(prompt),
        Line::from(""),
        Line::from(Span::styled(
            strings.builder_label_edges,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    let mut text = detail;
    text.extend(edges.into_iter().map(Line::from));
    text.push(Line::from(""));
    text.push(Line::from(strings.builder_keys));
    frame.render_widget(
        Paragraph::new(Text::from(text))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(strings.builder_editor_title),
            )
            .wrap(ratatui::widgets::Wrap { trim: false }),
        columns[1],
    );
}

#[allow(clippy::too_many_lines)]
fn render_run(frame: &mut Frame, app: &App, area: Rect) {
    let strings = app.strings();
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
                Span::styled(
                    node_status_label(status, app.lang),
                    Style::default().fg(color),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let title = if app.active_run.is_some() {
        strings.run_nodes_running
    } else {
        strings.run_nodes_idle
    };
    frame.render_widget(
        List::new(nodes).block(Block::default().borders(Borders::ALL).title(title)),
        columns[0],
    );

    let mut lines = vec![Line::from(Span::styled(
        strings.run_events,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];
    lines.extend(app.events.iter().cloned().map(Line::from));
    if let Some(run_id) = &app.last_run_id {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}: ", strings.run_dir_label),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(format!(".gloop/runs/{run_id}")),
        ]));
    }
    if let Some(summary) = &app.last_summary {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            strings.run_result,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(fill(
            strings.run_status_label,
            &[("status", &format!("{:?}", summary.status))],
        )));
        lines.push(Line::from(fill(
            strings.run_meta,
            &[
                ("id", &summary.run_id),
                ("ms", &summary.duration_ms.to_string()),
            ],
        )));
        if let Some(node) = app.graph.spec.nodes.get(app.selected_node)
            && let Some(outcome) = summary.nodes.get(&node.id)
        {
            lines.push(Line::from(fill(
                strings.run_selected,
                &[
                    ("id", &node.id),
                    ("status", &format!("{:?}", outcome.status)),
                ],
            )));
            if let Some(output) = &outcome.output {
                let rendered = serde_json::to_string(output).unwrap_or_else(|_| output.to_string());
                lines.push(Line::from(fill(
                    strings.run_output,
                    &[("output", &truncate_rendered(&rendered, 240))],
                )));
            }
            if let Some(error) = &outcome.error {
                lines.push(Line::from(fill(
                    strings.run_error,
                    &[("error", &truncate_rendered(error, 240))],
                )));
            }
            lines.push(Line::from(Span::styled(
                strings.run_open_output,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Runtime / results "),
            )
            .wrap(ratatui::widgets::Wrap { trim: false }),
        columns[1],
    );
}

fn truncate_rendered(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\u{2026}(+{} chars)", count - limit)
}

fn input_height(input: &InputState, area: Rect) -> u16 {
    let logical_lines = u16::try_from(input.value.split('\n').count()).unwrap_or(u16::MAX);
    let height = logical_lines.saturating_add(2);
    height.clamp(5, (area.height / 2).max(6))
}

#[allow(clippy::too_many_lines)]
fn render_input(frame: &mut Frame, input: &InputState, strings: &Strings, area: Rect) {
    let title = match input.target {
        InputTarget::Task => strings.input_task,
        InputTarget::Model => strings.input_model,
        InputTarget::NodePrompt => strings.input_prompt,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let (cursor_line, cursor_col) = line_col_at(&input.value, input.cursor);

    // Soft-wrap every logical line and remember which wrapped row carries the
    // cursor plus the cursor column within that row.
    let mut rows: Vec<(String, Option<usize>)> = Vec::new();
    for (line_index, line) in input.value.split('\n').enumerate() {
        let wrapped = wrap_line(line, width);
        let mut column = 0usize;
        for row in wrapped {
            let row_chars = row.text.chars().count();
            let cursor_here = line_index == cursor_line
                && cursor_col >= column
                && cursor_col <= column + row_chars;
            rows.push((row.text, cursor_here.then(|| cursor_col - column)));
            column += row_chars;
        }
    }
    if rows.is_empty() {
        rows.push((String::new(), Some(0)));
    }
    let cursor_row = rows
        .iter()
        .position(|(_, cursor)| cursor.is_some())
        .unwrap_or(0);
    let visible = inner.height as usize;
    let scroll = cursor_row.saturating_sub(visible.saturating_sub(1));

    let lines: Vec<Line> = rows
        .iter()
        .skip(scroll)
        .take(visible)
        .map(|(text, cursor_offset)| match cursor_offset {
            Some(offset) => {
                let mut before = String::new();
                let mut after = String::new();
                for (index, character) in text.chars().enumerate() {
                    if index < *offset {
                        before.push(character);
                    } else {
                        after.push(character);
                    }
                }
                Line::from(vec![
                    Span::raw(before),
                    Span::styled("\u{258f}", Style::default().fg(Color::Cyan)),
                    Span::raw(after),
                ])
            }
            None => Line::from(text.clone()),
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_scroll_overlay(
    frame: &mut Frame,
    title: &str,
    lines: &[String],
    offset: usize,
    area: Rect,
    border: Color,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(title.to_owned());
    frame.render_widget(Clear, area);
    let text: Vec<Line> = lines.iter().cloned().map(Line::from).collect();
    frame.render_widget(block.clone(), area);
    let inner = block.inner(area);
    frame.render_widget(
        Paragraph::new(Text::from(text))
            .scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

fn render_template_picker(
    frame: &mut Frame,
    app: &App,
    selected: usize,
    previews: &[TemplatePreview],
    area: Rect,
) {
    let strings = app.strings();
    let mut lines: Vec<Line> = Vec::new();
    for (index, preview) in previews.iter().enumerate() {
        let is_selected = index == selected;
        let marker = if is_selected { "\u{25b8}" } else { " " };
        let current = if preview.template == app.template {
            format!(" {}", strings.picker_current)
        } else {
            String::new()
        };
        let name_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled(
                format!(" {:<24} ", template_label(preview.template)),
                name_style,
            ),
            Span::raw(template_desc(strings, preview.template)),
            Span::styled(current, Style::default().fg(Color::Green)),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "   {} · {}",
                fill(
                    strings.nodes_edges,
                    &[
                        ("nodes", &preview.nodes.to_string()),
                        ("edges", &preview.edges.to_string()),
                    ],
                ),
                preview.shape
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    if app.manual_edits {
        lines.push(Line::from(Span::styled(
            strings.picker_discard_warning,
            Style::default().fg(Color::Yellow),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "{} · {} · {}",
                strings.key_move, strings.key_apply, strings.key_close
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(strings.template_picker_title),
            )
            .wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

fn render_profile_picker(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let strings = app.strings();
    let mut lines: Vec<Line> = Vec::new();
    if app.profiles.is_empty() {
        lines.push(Line::from(strings.status_profile_none));
    }
    for (index, profile) in app.profiles.iter().enumerate() {
        let is_selected = index == selected;
        let marker = if is_selected { "\u{25b8}" } else { " " };
        let current = if Some(index) == app.profile_index {
            format!(" {}", strings.picker_current)
        } else {
            String::new()
        };
        let enabled_note = if profile.enabled {
            String::new()
        } else {
            format!(" {}", strings.picker_disabled)
        };
        let source = match profile.source {
            wizard::ProfileSource::Builtin => strings.picker_source_builtin,
            wizard::ProfileSource::User => strings.picker_source_user,
            wizard::ProfileSource::Project => strings.picker_source_project,
        };
        let name_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled(format!(" {:<20} ", profile.name), name_style),
            Span::raw(format!("[{}] {source}", profile.kind)),
            Span::raw(
                profile
                    .default_model
                    .as_deref()
                    .map_or_else(String::new, |model| format!(" · {model}")),
            ),
            Span::styled(current, Style::default().fg(Color::Green)),
            Span::styled(enabled_note, Style::default().fg(Color::Red)),
        ]));
    }
    lines.push(Line::from(""));
    if app.manual_edits {
        lines.push(Line::from(Span::styled(
            strings.picker_discard_warning,
            Style::default().fg(Color::Yellow),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "{} · {} · {}",
                strings.key_move, strings.key_apply, strings.key_close
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(strings.profile_picker_title),
            )
            .wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

fn render_gate(frame: &mut Frame, gate: &GateEnvelope, strings: &Strings, area: Rect) {
    let default = match gate.request.default {
        GateDecision::Approve => strings.gate_approve,
        GateDecision::Reject => strings.gate_reject,
    };
    let content = vec![
        Line::from(Span::styled(
            fill(strings.gate_node, &[("node", &gate.request.node_id)]),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(gate.request.message.clone()),
        Line::from(""),
        Line::from(fill(strings.gate_keys, &[("default", default)])),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(strings.gate_title);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(content))
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let height = height.min(area.height.saturating_sub(2).max(3));
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
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => match app.handle_key(key) {
                Action::Continue => {}
                Action::Quit => break,
                Action::Save => {
                    if let Err(error) = app.save().await {
                        app.status = error.to_string();
                    }
                }
                Action::Run => {
                    let mut started = false;
                    if app.dirty {
                        match app.save().await {
                            Ok(()) => {
                                app.set_status(app.strings().status_autosave_before_run);
                                started = true;
                            }
                            Err(error) => app.status = error.to_string(),
                        }
                    } else {
                        started = true;
                    }
                    if started {
                        app.start_run();
                    }
                }
                Action::OpenOutput => app.prepare_output().await,
            },
            Event::Paste(text) => app.handle_paste(&text),
            _ => {}
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
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
        let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
    }
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

pub async fn launch(repo: PathBuf, trust_project_profiles: bool, lang: Language) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(anyhow!("graph TUI requires an interactive terminal"));
    }
    let guard = TerminalGuard;
    let mut terminal = setup_terminal()?;
    let result = match App::new(repo, trust_project_profiles, lang) {
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn temp_app() -> (tempfile::TempDir, App) {
        let repo = tempfile::tempdir().expect("create tempdir");
        let app = App::new(repo.path().to_path_buf(), false, Language::En)
            .expect("create app in temp repo");
        (repo, app)
    }

    #[test]
    fn template_labels_are_stable_for_help_and_status() {
        assert_eq!(template_label(GraphTemplate::Direct), "direct");
        assert_eq!(
            template_label(GraphTemplate::ReviewFixLoop),
            "review-fix-loop"
        );
        assert_eq!(template_label(GraphTemplate::Council), "council");
        assert_eq!(
            template_label(GraphTemplate::DecomposeFanoutReduce),
            "decompose-fanout-reduce"
        );
        assert_eq!(
            template_label(GraphTemplate::ImplementTestLoop),
            "implement-test-loop"
        );
    }

    fn assert_valid(graph: &Graph) {
        let errors = graph
            .validate()
            .iter()
            .filter(|issue| issue.severity == gloop_core::IssueSeverity::Error)
            .count();
        assert_eq!(errors, 0, "template graph must validate: {graph:?}");
    }

    #[test]
    fn council_template_has_blind_lanes_and_panel_review() {
        let council = wizard::template_graph(
            "work",
            "task",
            GraphTemplate::Council,
            Some("task".to_owned()),
            None,
            None,
        );
        assert_valid(&council);
        assert_eq!(council.spec.nodes.len(), 8);
        // Two blind design lanes fan into one design judgment.
        let into_design = council
            .spec
            .edges
            .iter()
            .filter(|edge| edge.to == "integrate_design")
            .count();
        assert_eq!(into_design, 2);
        // Three reviewers fan into the reconciled verdict.
        let into_verdict = council
            .spec
            .edges
            .iter()
            .filter(|edge| edge.to == "integrate_review")
            .count();
        assert_eq!(into_verdict, 3);
        let implement_inputs = council
            .spec
            .edges
            .iter()
            .filter(|edge| edge.to == "implement")
            .count();
        assert_eq!(implement_inputs, 1);
    }

    #[test]
    fn decompose_template_fans_out_to_worker_lanes_and_back() {
        let graph = wizard::template_graph(
            "work",
            "task",
            GraphTemplate::DecomposeFanoutReduce,
            Some("task".to_owned()),
            None,
            None,
        );
        assert_valid(&graph);
        assert_eq!(graph.spec.nodes.len(), 6);
        let to_workers = graph
            .spec
            .edges
            .iter()
            .filter(|edge| edge.from == "decompose")
            .count();
        assert_eq!(to_workers, 4);
        let into_integrate = graph
            .spec
            .edges
            .iter()
            .filter(|edge| edge.to == "integrate")
            .count();
        assert_eq!(into_integrate, 4);
    }

    #[test]
    fn implement_test_loop_template_routes_failures_to_the_fixer() {
        let graph = wizard::template_graph(
            "work",
            "task",
            GraphTemplate::ImplementTestLoop,
            Some("task".to_owned()),
            None,
            Some(5),
        );
        assert_valid(&graph);
        let loop_node = graph
            .spec
            .nodes
            .iter()
            .find(|node| node.id == "test_fix_loop")
            .expect("loop node exists");
        let NodeKind::Loop {
            graph: nested,
            until,
            max_iterations,
            ..
        } = &loop_node.kind
        else {
            panic!("test_fix_loop must be a loop node");
        };
        assert_eq!(until.node, "test");
        assert_eq!(until.status, NodeStatus::Succeeded);
        assert_eq!(*max_iterations, 5, "--loop-cap flows into the template");
        assert!(
            nested.spec.edges.iter().any(|edge| edge.from == "test"
                && edge.to == "fix"
                && edge.kind == gloop_core::EdgeKind::Failure),
            "failed verification must route to the fixer through a failure edge"
        );
        assert!(
            graph
                .spec
                .edges
                .iter()
                .any(|edge| edge.from == "implement" && edge.to == "test_fix_loop")
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

    #[tokio::test]
    async fn gate_wait_unblocks_when_run_is_cancelled() {
        let (requests, mut received) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let gate = TuiGate {
            requests,
            cancellation: cancellation.clone(),
        };
        let waiting = tokio::spawn(async move {
            gate.decide(GateRequest {
                run_id: "run".to_owned(),
                node_id: "approval".to_owned(),
                message: "continue?".to_owned(),
                default: GateDecision::Reject,
            })
            .await
        });
        let _envelope = received.recv().await.expect("gate request");
        cancellation.cancel();
        assert_eq!(
            waiting
                .await
                .expect("gate task")
                .expect_err("gate should cancel"),
            "TUI gate cancelled"
        );
    }

    #[tokio::test]
    async fn existing_work_graph_is_loaded_and_external_changes_are_not_overwritten() {
        let repo = tempfile::tempdir().expect("create tempdir");
        let graph_path = templates::graph_path(repo.path(), "work");
        fs::create_dir_all(graph_path.parent().expect("graph parent")).expect("create graph dir");
        let graph = Graph::new("work", "loaded task", vec![Node::agent("request", "do it")]);
        fs::write(
            &graph_path,
            graph.to_yaml().expect("serialize initial graph"),
        )
        .expect("write initial graph");

        let mut app =
            App::new(repo.path().to_path_buf(), false, Language::En).expect("load existing graph");
        assert_eq!(app.task, "loaded task");
        assert!(app.manual_edits, "loaded graphs count as user content");
        app.graph.spec.goal = "local edit".to_owned();
        app.dirty = true;

        let external = Graph::new(
            "work",
            "external edit",
            vec![Node::agent("request", "external")],
        );
        let external_yaml = external.to_yaml().expect("serialize external graph");
        fs::write(&graph_path, &external_yaml).expect("change graph on disk");

        let error = app
            .save()
            .await
            .expect_err("save must detect external change");
        assert!(
            error
                .to_string()
                .contains("output changed while it was being edited")
        );
        assert_eq!(
            fs::read_to_string(graph_path).expect("read graph"),
            external_yaml
        );
    }

    #[tokio::test]
    async fn task_input_commits_on_enter_and_alt_enter_inserts_newline() {
        let (_repo, mut app) = temp_app();
        app.task = String::new();
        app.begin_input(InputTarget::Task);
        for character in "analyze".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        // Alt+Enter inserts a newline instead of committing.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        for character in "then fix".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        let Some(Modal::Input(input)) = &app.modal else {
            panic!("input modal should still be open before commit");
        };
        assert_eq!(input.value, "analyze\nthen fix");
        // Plain Enter commits, like other AI TUIs.
        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.task, "analyze\nthen fix");
        assert_eq!(app.graph.spec.goal, "analyze\nthen fix");
    }

    #[tokio::test]
    async fn model_input_still_commits_on_plain_enter() {
        let (_repo, mut app) = temp_app();
        app.begin_input(InputTarget::Model);
        for character in "fast-model".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.model.as_deref(), Some("fast-model"));
    }

    #[tokio::test]
    async fn task_input_paste_inserts_text_without_committing() {
        let (_repo, mut app) = temp_app();
        app.task = String::new();
        app.begin_input(InputTarget::Task);
        app.handle_paste("line one\nline two");
        let Some(Modal::Input(input)) = &app.modal else {
            panic!("paste must keep the modal open");
        };
        assert_eq!(input.value, "line one\nline two");
        assert_eq!(input.cursor, input.value.len());
    }

    #[tokio::test]
    async fn task_edit_rebuilds_without_manual_edits_and_keeps_nodes_with_manual_edits() {
        let (_repo, mut app) = temp_app();
        // Fresh draft: commit rebuilds from the template.
        app.task = String::new();
        app.begin_input(InputTarget::Task);
        app.handle_paste("first version");
        app.handle_key(ctrl(KeyCode::Char('s')));
        assert_eq!(app.graph.spec.nodes.len(), 1);
        assert!(app.dirty);
        assert!(!app.manual_edits);

        // Manual edit: the next task commit updates the goal only.
        app.add_node();
        assert!(app.manual_edits);
        assert_eq!(app.graph.spec.nodes.len(), 2);
        app.begin_input(InputTarget::Task);
        // Clear the prefilled value and type a new one.
        if let Some(Modal::Input(input)) = &mut app.modal {
            input.value.clear();
            input.cursor = 0;
        }
        app.handle_paste("second version");
        app.handle_key(ctrl(KeyCode::Char('s')));
        assert_eq!(app.graph.spec.nodes.len(), 2, "manual nodes survive");
        assert_eq!(app.graph.spec.goal, "second version");
        assert_eq!(app.task, "second version");
    }

    #[tokio::test]
    async fn template_picker_applies_selection_and_rebuilds() {
        let (_repo, mut app) = temp_app();
        app.open_template_picker();
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.template, GraphTemplate::PlanImplementVerify);
        assert!(app.dirty);
        assert!(!app.manual_edits);
        assert_eq!(graph_shape(&app.graph), "plan -> implement -> verify");
        assert!(
            app.status.contains("plan-implement-verify"),
            "status explains the applied template: {}",
            app.status
        );
    }

    #[tokio::test]
    async fn template_picker_esc_cancels_without_changes() {
        let (_repo, mut app) = temp_app();
        let before = app.graph.clone();
        app.open_template_picker();
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.template, GraphTemplate::Direct);
        assert_eq!(app.graph, before);
    }

    #[test]
    fn graph_shape_reports_chains_and_edge_lists() {
        let direct = wizard::template_graph(
            "work",
            "task",
            GraphTemplate::Direct,
            Some("task".to_owned()),
            None,
            None,
        );
        assert_eq!(graph_shape(&direct), "request");
        let piv = wizard::template_graph(
            "work",
            "task",
            GraphTemplate::PlanImplementVerify,
            Some("task".to_owned()),
            None,
            None,
        );
        assert_eq!(graph_shape(&piv), "plan -> implement -> verify");
        let parallel = wizard::template_graph(
            "work",
            "task",
            GraphTemplate::ParallelResearchReduce,
            Some("task".to_owned()),
            None,
            None,
        );
        let shape = graph_shape(&parallel);
        assert!(shape.contains("->"), "fan-out shape lists edges: {shape}");
    }

    #[test]
    fn move_vertical_keeps_column_across_lines() {
        let value = "ab\ncdef\ngh";
        let mut preferred = None;
        let cursor = value.len();
        let up_once = move_vertical(value, cursor, -1, &mut preferred);
        assert_eq!(value.chars().nth(up_once), Some('e'));
        let up_twice = move_vertical(value, up_once, -1, &mut preferred);
        assert_eq!(up_twice, 2);
        let down = move_vertical(value, up_twice, 1, &mut preferred);
        assert_eq!(value.chars().nth(down), Some('e'));
    }

    #[test]
    fn wrap_line_counts_cjk_width_as_two_columns() {
        let rows = wrap_line("日本語", 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "日本");
        assert_eq!(rows[1].text, "語");
    }

    #[tokio::test]
    async fn language_toggle_switches_ui_strings() {
        let (_repo, mut app) = temp_app();
        assert_eq!(app.strings().screen_run, "Run Monitor");
        app.toggle_language();
        assert_eq!(app.lang, Language::Ja);
        assert_eq!(app.strings().screen_run, "実行モニター");
    }
}
