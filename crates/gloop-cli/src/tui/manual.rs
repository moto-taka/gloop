//! Named actions for authoring the existing Graph IR without a planning call.

use super::{
    Action, App, BindingTarget, InputTarget, Modal, Screen, binds_provider, centered_rect,
    file_sha256, node_kind_label, node_prompt,
};
use crate::{
    atomic_write,
    i18n::Language,
    templates,
    wizard::{self, EditorState},
};
use anyhow::{Context, Result, anyhow, ensure};
use crossterm::event::{KeyCode, KeyEvent};
use gloop_core::{Edge, EdgeKind, GateDefault, Graph, IssueSeverity, Node, NodeKind, PromptSpec};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use serde_yaml_ng as serde_yaml;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    NewAi(bool),
    Goal,
    Title,
    Body,
    NodeYaml,
    EdgeYaml(usize),
    SaveAs,
    OpenPath,
    Calls,
    Time,
    Parallel,
}

impl Field {
    pub(super) const fn multiline(self) -> bool {
        matches!(
            self,
            Self::NewAi(_) | Self::Goal | Self::Body | Self::NodeYaml | Self::EdgeYaml(_)
        )
    }
}

#[derive(Debug, Clone)]
enum Destination {
    New,
    Open(PathBuf),
    Quit,
}

#[derive(Debug, Clone)]
enum Page {
    Start,
    Actions,
    Node,
    Add { after: bool },
    Connections,
    Source,
    Target(String),
    ConnectionKind { from: String, to: String },
    Edge(usize),
    DeleteNode,
    Open(Vec<PathBuf>),
    Limits,
    Run,
    Discard(Destination),
}

#[derive(Debug, Clone)]
pub(super) struct Menu {
    page: Page,
    selected: usize,
}

pub(super) fn fresh_path(repo: &Path) -> Result<PathBuf> {
    for index in 1..=1000 {
        let path = templates::graph_path(repo, &format!("workflow-{index}"));
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    Err(anyhow!(
        "choose a new workflow name; workflow-1 through workflow-1000 exist"
    ))
}

fn shell_argv(script: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), script.into()]
    } else {
        vec!["sh".into(), "-c".into(), script.into()]
    }
}

fn is_shell(argv: &[String]) -> bool {
    argv.len() == 3
        && ((argv[0] == "sh" && argv[1] == "-c") || (argv[0] == "cmd" && argv[1] == "/C"))
}

impl App {
    pub(super) fn manual_text<'a>(&self, en: &'a str, ja: &'a str) -> &'a str {
        if self.lang == Language::Ja { ja } else { en }
    }

    fn menu(&mut self, page: Page) {
        self.modal = Some(Modal::Manual(Menu { page, selected: 0 }));
    }

    pub(super) fn open_manual_start(&mut self) {
        self.screen = Screen::Builder;
        self.modal = None;
        self.status.clear();
    }

    fn step_name(&self, id: &str) -> &str {
        self.graph
            .spec
            .nodes
            .iter()
            .find(|node| node.id == id)
            .map_or("?", |node| node.label.as_deref().unwrap_or(&node.id))
    }

    #[allow(clippy::too_many_lines)] // One table of visible actions for each menu.
    fn manual_choices(&self, page: &Page) -> Vec<String> {
        let t = |en, ja| self.manual_text(en, ja).to_owned();
        match page {
            Page::Start => vec![
                t("Create a graph yourself", "空のグラフから作る"),
                t("Open a saved graph", "保存したグラフを開く"),
                t("Open a YAML file…", "YAMLファイルを指定して開く…"),
                t("Back to home", "ホームに戻る"),
            ],
            Page::Actions => vec![
                t("+ AI", "+ AI"),
                t(
                    "+ Command / test / approval",
                    "+ コマンド・テスト・確認待ち",
                ),
                t("Connections", "接続"),
                t("Goal", "目的"),
                t("Execution settings…", "実行設定…"),
                t("Save", "保存"),
                t("Save as…", "名前を付けて保存…"),
                t("Open", "開く"),
                t("Templates", "ひな形"),
                t("Limits", "上限"),
                t("Validate", "検証"),
                t("Node settings", "ノード設定"),
                t("Back", "戻る"),
            ],
            Page::Node => {
                let mut choices = vec![
                    t("Edit this step's work", "作業内容を編集"),
                    t("Change the step's name", "作業の名前を変更"),
                ];
                if self
                    .graph
                    .spec
                    .nodes
                    .get(self.selected_node)
                    .is_some_and(binds_provider)
                {
                    choices.extend([
                        t("Choose AI tool", "担当AIを選ぶ"),
                        t("Choose model", "モデルを選ぶ"),
                    ]);
                }
                choices.extend([
                    t("Add the following step", "この作業の後に追加"),
                    t("Manage connections", "接続を管理"),
                    t("Edit node settings (YAML)", "ノードの詳細設定（YAML）"),
                    t("Remove this step…", "この作業を削除…"),
                    t("Back", "戻る"),
                ]);
                choices
            }
            Page::Add { .. } => vec![
                t("AI", "AI"),
                t(">_ Command", ">_ コマンド"),
                t("✓ Test command", "✓ テスト"),
                t("⏸ Approval", "⏸ 確認待ち"),
            ],
            Page::Connections => {
                let mut choices = vec![t("+ Connect two steps", "+ 作業どうしを接続")];
                choices.extend(self.graph.spec.edges.iter().map(|edge| {
                    format!(
                        "{} → {}  ({:?})",
                        self.step_name(&edge.from),
                        self.step_name(&edge.to),
                        edge.kind
                    )
                }));
                choices.push(t("Back", "戻る"));
                choices
            }
            Page::Source => self
                .graph
                .spec
                .nodes
                .iter()
                .map(|node| self.step_name(&node.id).to_owned())
                .collect(),
            Page::Target(from) => self
                .graph
                .spec
                .nodes
                .iter()
                .filter(|node| &node.id != from)
                .map(|node| self.step_name(&node.id).to_owned())
                .collect(),
            Page::ConnectionKind { .. } => vec![
                t(
                    "Pass the result and wait for completion",
                    "結果を渡す・完了を待つ",
                ),
                t(
                    "Wait for completion without passing output",
                    "順番だけ指定（結果は渡さない）",
                ),
                t(
                    "Continue when the preceding step fails",
                    "前の作業が失敗したときに進む",
                ),
            ],
            Page::Edge(_) => vec![
                t("Edit connection settings (YAML)", "接続条件を編集（YAML）"),
                t("Remove this connection", "この接続を削除"),
                t("Back", "戻る"),
            ],
            Page::DeleteNode => vec![
                t(
                    "Remove step and its connections",
                    "作業と、その作業への接続を削除",
                ),
                t("Keep the step", "削除せず戻る"),
            ],
            Page::Open(paths) => {
                let mut choices = paths
                    .iter()
                    .map(|path| {
                        path.strip_prefix(&self.repo)
                            .unwrap_or(path)
                            .display()
                            .to_string()
                    })
                    .collect::<Vec<_>>();
                choices.push(t("Enter another file path…", "別のファイルパスを入力…"));
                choices
            }
            Page::Limits => vec![
                t("Maximum AI calls", "AI呼び出し上限"),
                t("Time limit in seconds", "制限時間（秒）"),
                t("Maximum parallel steps", "同時に実行する作業数"),
                t("Back", "戻る"),
            ],
            Page::Run => {
                let mut choices = vec![
                    t("Run exactly this graph", "このグラフの内容で開始"),
                    t("Back to editing", "編集に戻る"),
                ];
                choices.extend(self.graph.spec.nodes.iter().map(|node| {
                    format!(
                        "{}\n  {} · {} / {}",
                        self.step_name(&node.id),
                        node_kind_label(node),
                        node.profile().unwrap_or("—"),
                        node.model().unwrap_or("default")
                    )
                }));
                choices
            }
            Page::Discard(_) => vec![
                t(
                    "Discard unsaved edits and continue",
                    "未保存の編集を破棄して進む",
                ),
                t("Return to editing", "編集に戻る"),
            ],
        }
    }

    fn manual_heading(&self, page: &Page) -> &str {
        match page {
            Page::Start => {
                self.manual_text("Manual · your own graph", "Manual · 自分でグラフを組む")
            }
            Page::Actions => self.manual_text("Graph actions", "グラフの操作"),
            Page::Node => self.manual_text("Edit the selected step", "選択した作業を編集"),
            Page::Add { .. } => self.manual_text("Add", "追加"),
            Page::Connections => self.manual_text(
                "Connections define the execution flow",
                "接続が実行の流れを決めます",
            ),
            Page::Source => self.manual_text("Choose the preceding step", "先に実行する作業を選ぶ"),
            Page::Target(_) => {
                self.manual_text("Choose the following step", "次に実行する作業を選ぶ")
            }
            Page::ConnectionKind { .. } => {
                self.manual_text("How should these steps connect?", "どう接続しますか？")
            }
            Page::Edge(_) => self.manual_text("Edit a connection", "接続を編集"),
            Page::DeleteNode => self.manual_text("Remove this step?", "この作業を削除しますか？"),
            Page::Open(_) => self.manual_text("Open your graph", "自分のグラフを開く"),
            Page::Limits => self.manual_text("Execution limits", "実行時の上限"),
            Page::Run => self.manual_text(
                "Review your graph before running",
                "自分のグラフを確認して実行",
            ),
            Page::Discard(_) => {
                self.manual_text("There are unsaved edits", "未保存の編集があります")
            }
        }
    }

    fn manual_description(&self, page: &Page) -> String {
        match page {
            Page::Add { after } if *after => format!(
                "{} → +",
                self.selected_node_id().map_or("—", |id| self.step_name(id))
            ),
            Page::Connections | Page::Source | Page::Target(_) | Page::ConnectionKind { .. } => {
                "A → B    A → B, C    B, C → D".to_owned()
            }
            Page::Node | Page::DeleteNode => self
                .graph
                .spec
                .nodes
                .get(self.selected_node)
                .map_or_else(String::new, |node| self.step_name(&node.id).to_owned()),
            Page::Edge(index) => {
                self.graph
                    .spec
                    .edges
                    .get(*index)
                    .map_or_else(String::new, |edge| {
                        format!(
                            "{} → {} ({:?})",
                            self.step_name(&edge.from),
                            self.step_name(&edge.to),
                            edge.kind
                        )
                    })
            }
            Page::Run | Page::Limits => format!(
                "{} {} · {}s · {} {}",
                self.graph
                    .spec
                    .budgets
                    .model_calls
                    .map_or_else(|| "∞".to_owned(), |v| v.to_string()),
                self.manual_text("AI calls max", "AI呼び出しまで"),
                self.graph
                    .spec
                    .budgets
                    .wall_time_seconds
                    .map_or_else(|| "∞".to_owned(), |v| v.to_string()),
                self.manual_text("parallel", "並列"),
                self.graph.spec.policies.max_parallel
            ),
            Page::Discard(_) => self
                .manual_text("Unsaved changes", "未保存の変更があります")
                .to_owned(),
            _ => String::new(),
        }
    }

    pub(super) fn handle_manual_menu_key(&mut self, key: KeyEvent) -> Action {
        let Some(Modal::Manual(menu)) = &self.modal else {
            return Action::Continue;
        };
        let mut menu = menu.clone();
        let count = self.manual_choices(&menu.page).len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modal = None;
                return Action::Continue;
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                menu.selected = menu.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                menu.selected = (menu.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Enter => match self.manual_select(menu.page.clone(), menu.selected) {
                Ok(action) => return action,
                Err(error) => self.status = format!("{error:#}"),
            },
            _ => {}
        }
        self.modal = Some(Modal::Manual(menu));
        Action::Continue
    }

    pub(super) fn handle_manual_shortcut(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Tab => self.menu(Page::Actions),
            KeyCode::Enter if self.screen == Screen::Builder && self.connect_from.is_none() => {
                self.begin_input(InputTarget::Manual(if self.graph.spec.nodes.is_empty() {
                    Field::NewAi(false)
                } else {
                    Field::Body
                }));
            }
            KeyCode::Char('a') if self.screen == Screen::Builder => {
                self.begin_input(InputTarget::Manual(Field::NewAi(true)));
            }
            KeyCode::Char('A') if self.screen == Screen::Builder => {
                self.begin_input(InputTarget::Manual(Field::NewAi(false)));
            }
            KeyCode::Char('O') => {
                if let Err(error) = self.open_manual_files() {
                    self.status = error.to_string();
                }
            }
            KeyCode::Char('n') => self.menu(Page::Start),
            KeyCode::Char('c') if self.screen == Screen::Builder => {
                self.begin_connection();
                if self.graph.spec.nodes.len() > 1 {
                    self.selected_node = (self.selected_node + 1) % self.graph.spec.nodes.len();
                }
            }
            KeyCode::Esc if self.connect_from.is_some() => self.connect_from = None,
            KeyCode::Esc if self.screen == Screen::Run => self.screen = Screen::Builder,
            KeyCode::Char('x') if self.screen == Screen::Builder => self.menu(Page::DeleteNode),
            KeyCode::Char('r') => {
                self.review_manual_run();
                if matches!(
                    self.modal,
                    Some(Modal::Manual(Menu {
                        page: Page::Run,
                        ..
                    }))
                ) {
                    self.modal = None;
                    return Some(Action::Run);
                }
            }
            KeyCode::Char('e') if self.screen == Screen::Builder => {
                self.begin_input(InputTarget::Manual(Field::Body));
            }
            KeyCode::Char('q') => return Some(self.manual_destination(Destination::Quit)),
            _ => return None,
        }
        Some(Action::Continue)
    }

    #[allow(clippy::too_many_lines)] // Menu dispatch, with edits delegated below.
    fn manual_select(&mut self, page: Page, selected: usize) -> Result<Action> {
        match page {
            Page::Start => match selected {
                0 => return Ok(self.manual_destination(Destination::New)),
                1 => self.open_manual_files()?,
                2 => self.begin_input(InputTarget::Manual(Field::OpenPath)),
                _ => return Ok(self.manual_destination(Destination::Quit)),
            },
            Page::Actions => match selected {
                0 => self.begin_input(InputTarget::Manual(Field::NewAi(true))),
                1 => self.menu(Page::Add { after: true }),
                2 => self.menu(Page::Connections),
                3 => self.begin_input(InputTarget::Manual(Field::Goal)),
                4 => self.review_manual_run(),
                5 => {
                    self.modal = None;
                    return Ok(Action::Save);
                }
                6 => self.begin_input(InputTarget::Manual(Field::SaveAs)),
                7 => self.open_manual_files()?,
                8 => self.open_template_picker(),
                9 => self.menu(Page::Limits),
                10 => self.validate_and_show_issues(),
                11 => self.menu(Page::Node),
                _ => self.modal = None,
            },
            Page::Node => {
                let selected = if selected >= 2
                    && !self
                        .graph
                        .spec
                        .nodes
                        .get(self.selected_node)
                        .is_some_and(binds_provider)
                {
                    selected + 2
                } else {
                    selected
                };
                match selected {
                    0 => self.begin_input(InputTarget::Manual(Field::Body)),
                    1 => self.begin_input(InputTarget::Manual(Field::Title)),
                    2 => self.open_profile_picker(BindingTarget::Node),
                    3 => self.open_model_picker(BindingTarget::Node),
                    4 => self.menu(Page::Add { after: true }),
                    5 => self.menu(Page::Connections),
                    6 => self.begin_input(InputTarget::Manual(Field::NodeYaml)),
                    7 => self.menu(Page::DeleteNode),
                    _ => self.modal = None,
                }
            }
            Page::Add { after } => self.manual_add_node([0, 3, 4, 5][selected], after)?,
            Page::Connections => {
                if selected == 0 {
                    ensure!(
                        self.graph.spec.nodes.len() >= 2,
                        "{}",
                        self.manual_text(
                            "Add two steps before connecting them.",
                            "先に作業を2つ以上追加してください。"
                        )
                    );
                    self.menu(Page::Source);
                } else if selected <= self.graph.spec.edges.len() {
                    self.menu(Page::Edge(selected - 1));
                } else {
                    self.modal = None;
                }
            }
            Page::Source => {
                let from = self
                    .graph
                    .spec
                    .nodes
                    .get(selected)
                    .context("select a source step")?
                    .id
                    .clone();
                self.menu(Page::Target(from));
            }
            Page::Target(from) => {
                let to = self
                    .graph
                    .spec
                    .nodes
                    .iter()
                    .filter(|node| node.id != from)
                    .nth(selected)
                    .context("select a destination step")?
                    .id
                    .clone();
                self.menu(Page::ConnectionKind { from, to });
            }
            Page::ConnectionKind { from, to } => {
                let kind = [EdgeKind::Data, EdgeKind::Control, EdgeKind::Failure][selected];
                let state = EditorState::from_graph(self.graph.clone(), 0);
                self.graph = wizard::add_edge_to_editor(
                    &state,
                    Edge {
                        from,
                        to,
                        kind,
                        when: None,
                    },
                )?
                .graph;
                self.dirty = true;
                self.manual_edits = true;
                self.menu(Page::Connections);
            }
            Page::Edge(index) => match selected {
                0 => self.begin_input(InputTarget::Manual(Field::EdgeYaml(index))),
                1 => {
                    let edge = self
                        .graph
                        .spec
                        .edges
                        .get(index)
                        .context("connection no longer exists")?;
                    let state = EditorState::from_graph(self.graph.clone(), 0);
                    self.graph =
                        wizard::remove_edge_from_editor(&state, &edge.from, &edge.to, edge.kind)?
                            .graph;
                    self.dirty = true;
                    self.manual_edits = true;
                    self.menu(Page::Connections);
                }
                _ => self.menu(Page::Connections),
            },
            Page::DeleteNode => {
                if selected == 0 {
                    let node = self
                        .graph
                        .spec
                        .nodes
                        .get(self.selected_node)
                        .context("select a step")?;
                    let state = EditorState::from_graph(self.graph.clone(), 0);
                    self.graph = wizard::remove_node_from_editor(&state, &node.id)?.graph;
                    self.selected_node = self
                        .selected_node
                        .min(self.graph.spec.nodes.len().saturating_sub(1));
                    self.dirty = true;
                    self.manual_edits = true;
                }
                self.modal = None;
            }
            Page::Open(paths) => {
                if let Some(path) = paths.get(selected) {
                    return Ok(self.manual_destination(Destination::Open(path.clone())));
                }
                self.begin_input(InputTarget::Manual(Field::OpenPath));
            }
            Page::Limits => match selected {
                0 => self.begin_input(InputTarget::Manual(Field::Calls)),
                1 => self.begin_input(InputTarget::Manual(Field::Time)),
                2 => self.begin_input(InputTarget::Manual(Field::Parallel)),
                _ => self.modal = None,
            },
            Page::Run => {
                self.modal = None;
                if selected == 0 {
                    return Ok(Action::Run);
                } else if selected >= 2 {
                    self.selected_node = selected - 2;
                    self.menu(Page::Node);
                }
            }
            Page::Discard(destination) => {
                if selected == 0 {
                    return self.apply_manual_destination(destination);
                }
                self.modal = None;
            }
        }
        Ok(Action::Continue)
    }

    fn review_manual_run(&mut self) {
        if let Some(index) = self.graph.spec.nodes.iter().position(|node| matches!(&node.kind, NodeKind::Command { argv, .. } | NodeKind::Verify { argv, .. } if is_shell(argv) && argv[2].trim().is_empty())) {
            self.selected_node = index;
            self.status = self.manual_text("Enter the command for this step before running.", "実行する前に、この作業のコマンドを入力してください。").to_owned();
            self.begin_input(InputTarget::Manual(Field::Body));
            return;
        }
        if self
            .graph
            .validate()
            .iter()
            .any(|issue| issue.severity == IssueSeverity::Error)
        {
            self.validate_and_show_issues();
        } else {
            self.menu(Page::Run);
        }
    }

    fn open_manual_files(&mut self) -> Result<()> {
        let paths = templates::list_graph_files(&self.repo)
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .take(100)
            .collect();
        self.menu(Page::Open(paths));
        Ok(())
    }

    fn manual_destination(&mut self, destination: Destination) -> Action {
        if self.dirty {
            self.menu(Page::Discard(destination));
            return Action::Continue;
        }
        match self.apply_manual_destination(destination) {
            Ok(action) => action,
            Err(error) => {
                self.status = format!("{error:#}");
                Action::Continue
            }
        }
    }

    fn apply_manual_destination(&mut self, destination: Destination) -> Result<Action> {
        match destination {
            Destination::Quit => return Ok(Action::Quit),
            Destination::New => {
                let path = fresh_path(&self.repo)?;
                let name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .context("invalid graph name")?;
                let mut graph = Graph::new(name, "", vec![]);
                graph.spec.budgets.model_calls = Some(16);
                graph.spec.budgets.wall_time_seconds = Some(1800);
                self.graph = graph;
                self.graph_path = path;
                self.expected_sha256 = None;
                self.create_only = true;
                self.task.clear();
                self.dirty = false;
                self.selected_node = 0;
                self.manual_edits = true;
                self.clear_manual_run();
                self.modal = None;
            }
            Destination::Open(path) => self.open_manual_path(&path)?,
        }
        self.screen = Screen::Builder;
        self.connect_from = None;
        Ok(Action::Continue)
    }

    fn open_manual_path(&mut self, path: &Path) -> Result<()> {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            self.repo.join(path)
        };
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "graph must be a regular, non-symlink file"
        );
        let graph = Graph::from_path(&path)?;
        let sha = file_sha256(&path)?;
        self.task.clone_from(&graph.spec.goal);
        self.graph = graph;
        self.graph_path = path;
        self.expected_sha256 = Some(sha);
        self.create_only = false;
        self.dirty = false;
        self.manual_edits = true;
        self.selected_node = 0;
        self.modal = None;
        self.clear_manual_run();
        self.status = self.manual_text("Opened", "開きました").to_owned();
        Ok(())
    }

    fn clear_manual_run(&mut self) {
        self.last_summary = None;
        self.last_run_id = None;
        self.announced_run = false;
        self.node_status.clear();
        self.events.clear();
        self.pending_gates.clear();
    }

    fn manual_add_node(&mut self, kind: usize, after: bool) -> Result<()> {
        let id = (1..=self.graph.spec.nodes.len() + 1)
            .map(|index| format!("step_{index}"))
            .find(|id| self.graph.spec.nodes.iter().all(|node| &node.id != id))
            .context("no free node id")?;
        let mut node = Node::agent(&id, "");
        if let NodeKind::Agent { profile, model, .. } = &mut node.kind {
            if let Some(previous) = self
                .graph
                .spec
                .nodes
                .get(self.selected_node)
                .filter(|node| binds_provider(node))
            {
                *profile = previous.profile().map(str::to_owned);
                *model = previous.model().map(str::to_owned);
            } else {
                *profile = self.selected_profile().map(str::to_owned);
                model.clone_from(&self.model);
            }
        }
        node.label = Some(format!(
            "{} {}",
            self.manual_text("Step", "作業"),
            self.graph.spec.nodes.len() + 1
        ));
        match kind {
            1 | 2 => {
                if let NodeKind::Agent {
                    prompt,
                    profile,
                    model,
                    output,
                    ..
                } = node.kind
                {
                    node.kind = if kind == 1 {
                        NodeKind::Reduce {
                            prompt,
                            profile,
                            model,
                            output,
                        }
                    } else {
                        NodeKind::Synthesize {
                            prompt,
                            profile,
                            model,
                            output,
                        }
                    };
                }
            }
            3 => node.kind = Node::command(&id, shell_argv("")).kind,
            4 => {
                if let NodeKind::Command { argv, env, output } =
                    Node::command(&id, shell_argv("")).kind
                {
                    node.kind = NodeKind::Verify { argv, env, output };
                }
            }
            5 => {
                node.kind = NodeKind::Gate {
                    message: String::new(),
                    default: GateDefault::Reject,
                }
            }
            _ => {}
        }
        if after && let Some(from) = self.graph.spec.nodes.get(self.selected_node) {
            self.graph.spec.edges.push(Edge::data(&from.id, &id));
        }
        // An empty instruction is a draft; runtime validation blocks execution
        // until it is filled. No model is used to author or rewrite the graph.
        self.graph.spec.nodes.push(node);
        self.selected_node = self.graph.spec.nodes.len() - 1;
        self.dirty = true;
        self.manual_edits = true;
        self.begin_input(InputTarget::Manual(Field::Body));
        Ok(())
    }

    pub(super) fn manual_value(&self, field: Field) -> Result<String> {
        let node = || {
            self.graph
                .spec
                .nodes
                .get(self.selected_node)
                .context("select a step first")
        };
        Ok(match field {
            Field::NewAi(_) | Field::OpenPath => String::new(),
            Field::Goal => self.task.clone(),
            Field::Title => node()?.label.clone().unwrap_or_default(),
            Field::Body => match &node()?.kind {
                NodeKind::Command { argv, .. } | NodeKind::Verify { argv, .. } => {
                    if is_shell(argv) {
                        argv[2].clone()
                    } else {
                        serde_json::to_string(argv)?
                    }
                }
                NodeKind::Gate { message, .. } => message.clone(),
                _ => node_prompt(node()?)
                    .context("use node settings to edit a nested graph")?
                    .to_owned(),
            },
            Field::NodeYaml => serde_yaml::to_string(node()?)?,
            Field::EdgeYaml(index) => serde_yaml::to_string(
                self.graph
                    .spec
                    .edges
                    .get(index)
                    .context("select a connection")?,
            )?,
            Field::SaveAs => self.graph.metadata.name.clone(),
            Field::Calls => self
                .graph
                .spec
                .budgets
                .model_calls
                .map_or_else(String::new, |v| v.to_string()),
            Field::Time => self
                .graph
                .spec
                .budgets
                .wall_time_seconds
                .map_or_else(String::new, |v| v.to_string()),
            Field::Parallel => self.graph.spec.policies.max_parallel.to_string(),
        })
    }

    pub(super) fn manual_field_title(&self, field: Field) -> &str {
        match field {
            Field::NewAi(_) => self.manual_text("Ask AI", "AIへの依頼"),
            Field::Goal => {
                self.manual_text("Overall purpose of this graph", "グラフ全体で何をする？")
            }
            Field::Title => self.manual_text("Name this step", "作業の名前"),
            Field::Body => match self
                .graph
                .spec
                .nodes
                .get(self.selected_node)
                .map(|node| &node.kind)
            {
                Some(NodeKind::Command { argv, .. } | NodeKind::Verify { argv, .. })
                    if !is_shell(argv) =>
                {
                    self.manual_text(
                        "Command arguments (JSON array)",
                        "コマンドの引数（JSON配列）",
                    )
                }
                Some(NodeKind::Command { .. } | NodeKind::Verify { .. }) => self.manual_text(
                    "Shell command to run in this project",
                    "このプロジェクトで実行するシェルコマンド",
                ),
                Some(NodeKind::Gate { .. }) => self.manual_text(
                    "What should you approve before continuing?",
                    "先に進む前に何を確認する？",
                ),
                _ => self.manual_text(
                    "Instructions for this step",
                    "この作業でAIに何をしてもらう？",
                ),
            },
            Field::NodeYaml => self.manual_text(
                "Full node settings (YAML; keep its id)",
                "ノードの全設定（YAML・idは変更しない）",
            ),
            Field::EdgeYaml(_) => {
                self.manual_text("Connection conditions (YAML)", "接続条件（YAML）")
            }
            Field::SaveAs => self.manual_text(
                "Workflow name (letters, numbers, hyphens)",
                "保存名（英小文字・数字・ハイフン）",
            ),
            Field::OpenPath => self.manual_text(
                "Graph YAML path (relative to project or absolute)",
                "グラフのYAMLパス（プロジェクトからの相対・絶対）",
            ),
            Field::Calls => self.manual_text(
                "Maximum AI calls (1–1000000)",
                "AI呼び出し上限（1〜1000000）",
            ),
            Field::Time => self.manual_text(
                "Time limit in seconds (1–86400)",
                "制限時間・秒（1〜86400）",
            ),
            Field::Parallel => self.manual_text(
                "Maximum parallel steps (1–64)",
                "同時に実行する作業数（1〜64）",
            ),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn manual_commit(&mut self, field: Field, value: &str) -> Result<()> {
        ensure!(
            !value.is_empty(),
            "{}",
            self.manual_text(
                "Enter a value; Esc returns without applying it.",
                "内容を入力してください。Escで変更せず戻れます。"
            )
        );
        match field {
            Field::NewAi(after) => {
                let incoming = if after {
                    Vec::new()
                } else {
                    self.graph
                        .spec
                        .edges
                        .iter()
                        .filter(|edge| Some(edge.to.as_str()) == self.selected_node_id())
                        .cloned()
                        .collect::<Vec<_>>()
                };
                self.manual_add_node(0, after)?;
                let to = self.graph.spec.nodes[self.selected_node].id.clone();
                self.graph
                    .spec
                    .edges
                    .extend(incoming.into_iter().map(|mut edge| {
                        edge.to.clone_from(&to);
                        edge
                    }));
                return self.manual_commit(Field::Body, value);
            }
            Field::Goal => {
                value.clone_into(&mut self.task);
                value.clone_into(&mut self.graph.spec.goal);
            }
            Field::Title => {
                self.graph
                    .spec
                    .nodes
                    .get_mut(self.selected_node)
                    .context("select a step")?
                    .label = Some(value.to_owned());
            }
            Field::Body => {
                let node = self
                    .graph
                    .spec
                    .nodes
                    .get_mut(self.selected_node)
                    .context("select a step")?;
                if node_prompt(node).is_some_and(str::is_empty) {
                    node.label = Some(
                        value
                            .lines()
                            .next()
                            .unwrap_or(value)
                            .chars()
                            .take(40)
                            .collect(),
                    );
                }
                match &mut node.kind {
                    NodeKind::Agent { prompt, .. }
                    | NodeKind::Reduce { prompt, .. }
                    | NodeKind::Synthesize { prompt, .. } => {
                        *prompt = PromptSpec::Inline(value.to_owned());
                    }
                    NodeKind::Command { argv, .. } | NodeKind::Verify { argv, .. } => {
                        let parsed: Vec<String> = if is_shell(argv) {
                            shell_argv(value)
                        } else {
                            serde_json::from_str(value)?
                        };
                        ensure!(
                            !parsed.is_empty() && !parsed[0].trim().is_empty(),
                            "enter a command executable"
                        );
                        *argv = parsed;
                    }
                    NodeKind::Gate { message, .. } => value.clone_into(message),
                    _ => return Err(anyhow!("use node settings to edit a nested graph")),
                }
                if self.task.is_empty() {
                    value.clone_into(&mut self.task);
                    value.clone_into(&mut self.graph.spec.goal);
                }
            }
            Field::NodeYaml => {
                let node: Node = serde_yaml::from_str(value)?;
                let current = self
                    .graph
                    .spec
                    .nodes
                    .get(self.selected_node)
                    .context("select a step")?;
                ensure!(
                    node.id == current.id,
                    "keep the node id so its connections are preserved"
                );
                self.graph = wizard::replace_node_in_editor(
                    &EditorState::from_graph(self.graph.clone(), 0),
                    &current.id,
                    node,
                )?
                .graph;
            }
            Field::EdgeYaml(index) => {
                let edge: Edge = serde_yaml::from_str(value)?;
                let mut graph = self.graph.clone();
                *graph
                    .spec
                    .edges
                    .get_mut(index)
                    .context("connection no longer exists")? = edge;
                let errors = graph
                    .validate()
                    .into_iter()
                    .filter(|issue| issue.severity == IssueSeverity::Error)
                    .collect::<Vec<_>>();
                ensure!(errors.is_empty(), "{}", serde_json::to_string(&errors)?);
                self.graph = graph;
            }
            Field::SaveAs => {
                self.manual_save_as(value)?;
                return Ok(());
            }
            Field::OpenPath => {
                if self.dirty {
                    self.menu(Page::Discard(Destination::Open(PathBuf::from(value))));
                } else {
                    self.open_manual_path(Path::new(value))?;
                }
                return Ok(());
            }
            Field::Calls => {
                let count: u32 = value.parse()?;
                ensure!((1..=1_000_000).contains(&count), "calls must be 1–1000000");
                self.graph.spec.budgets.model_calls = Some(count);
            }
            Field::Time => {
                let seconds: u64 = value.parse()?;
                ensure!(
                    (1..=86400).contains(&seconds),
                    "time limit must be 1–86400 seconds"
                );
                self.graph.spec.budgets.wall_time_seconds = Some(seconds);
            }
            Field::Parallel => {
                let count: usize = value.parse()?;
                ensure!((1..=64).contains(&count), "parallelism must be 1–64");
                self.graph.spec.policies.max_parallel = count;
            }
        }
        self.dirty = true;
        self.manual_edits = true;
        match field {
            Field::EdgeYaml(_) => self.menu(Page::Connections),
            Field::Calls | Field::Time | Field::Parallel => self.menu(Page::Limits),
            _ => self.modal = None,
        }
        self.status.clear();
        Ok(())
    }

    fn manual_save_as(&mut self, name: &str) -> Result<()> {
        templates::validate_template_lookup_name(name).map_err(anyhow::Error::msg)?;
        templates::ensure_managed_directory(&self.repo, Path::new(templates::GRAPHS_DIR))?;
        fs::create_dir_all(templates::graphs_dir(&self.repo))?;
        let path = templates::graph_path(&self.repo, name);
        let mut graph = self.graph.clone();
        name.clone_into(&mut graph.metadata.name);
        atomic_write::write_text_no_replace_sync(&path, &graph.to_yaml()?)?;
        ensure!(
            Graph::from_path(&path)? == graph,
            "saved graph differs from the editor"
        );
        self.graph = graph;
        self.expected_sha256 = Some(file_sha256(&path)?);
        self.graph_path = path;
        self.create_only = false;
        self.dirty = false;
        self.modal = None;
        self.status = format!(
            "{}: {}",
            self.manual_text("Saved", "保存しました"),
            self.graph_path.display()
        );
        Ok(())
    }
}

pub(super) fn render_menu(frame: &mut Frame, app: &App, menu: &Menu, area: Rect) {
    // Keep the editor's status/error line and shortcuts visible below the menu.
    let area = centered_rect(94, area.height.saturating_sub(6), area);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(app.manual_heading(&menu.page))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Length(if app.manual_description(&menu.page).is_empty() {
            0
        } else {
            2.min(inner.height.saturating_sub(5))
        }),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(app.manual_description(&menu.page))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        rows[0],
    );
    let items = app
        .manual_choices(&menu.page)
        .into_iter()
        .map(ListItem::new)
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("› ").highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        rows[1],
        &mut ListState::default().with_selected(Some(menu.selected)),
    );
    frame.render_widget(
        Paragraph::new(app.manual_text(
            "↑↓ choose · Enter select · Esc back",
            "↑↓ 選択 · Enter 決定 · Esc 戻る",
        ))
        .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gloop_core::NodeStatus;
    use std::time::Duration;

    fn app(repo: &Path) -> App {
        App::new_with_graph(
            repo.to_owned(),
            false,
            Language::Ja,
            fresh_path(repo).unwrap(),
            true,
        )
        .unwrap()
    }

    #[test]
    fn quick_authoring_keeps_the_canvas_and_builds_the_requested_branch() {
        use crossterm::event::KeyModifiers;
        let repo = tempfile::tempdir().unwrap();
        let mut app = app(repo.path());
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.open_manual_start();
        assert!(app.modal.is_none());
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.graph.spec.nodes.is_empty());
        assert!(!app.dirty);
        for instruction in ["Translate Hello", "Check the translation"] {
            app.handle_key(key(KeyCode::Char('a')));
            app.handle_paste(instruction);
            app.handle_key(key(KeyCode::Enter));
            assert!(app.modal.is_none());
            if app.graph.spec.nodes.len() == 1 {
                super::super::set_node_profile(&mut app.graph.spec.nodes[0], Some("codex".into()));
                super::super::set_node_model(
                    &mut app.graph.spec.nodes[0],
                    Some("small-model".into()),
                );
            }
        }
        assert_eq!(app.graph.spec.goal, "Translate Hello");
        assert_eq!(app.graph.spec.nodes[1].model(), Some("small-model"));
        assert_eq!(app.graph.spec.nodes[1].profile(), Some("codex"));
        app.handle_key(key(KeyCode::Char('A')));
        app.handle_paste("Check the tone");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.graph.spec.edges,
            vec![
                Edge::data("step_1", "step_2"),
                Edge::data("step_1", "step_3")
            ]
        );
        let expected = app.graph.clone();
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), Action::Run);
        assert!(app.modal.is_none());
        assert_eq!(app.graph, expected);
        assert!(
            app.active_run.is_none(),
            "only the event loop launches the requested run"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| super::super::render(frame, &app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for required in ["→ 2", "→ 3", "Check the tone", "small-model", "r ▶"] {
            assert!(text.contains(required), "missing {required}");
        }
        for obsolete in ["前の結果をまとめる", "種類:", "リトライ回数:"] {
            assert!(!text.contains(obsolete));
        }
    }

    #[test]
    fn quick_run_blocks_empty_instructions_and_profile_switch_clears_old_model() {
        use crossterm::event::KeyModifiers;
        let repo = tempfile::tempdir().unwrap();
        let mut app = app(repo.path());
        app.manual_add_node(3, false).unwrap();
        app.modal = None;
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Action::Continue
        );
        assert!(matches!(app.modal, Some(Modal::Input(_))));
        app.graph = Graph::new("switch", "Run", vec![Node::agent("a", "Reply")]);
        app.selected_node = 0;
        super::super::set_node_model(
            &mut app.graph.spec.nodes[0],
            Some("another-tool-model".into()),
        );
        if let Some(index) = app.profiles.iter().position(|profile| profile.enabled) {
            app.apply_profile_to_node(Some(index));
            assert!(app.graph.spec.nodes[0].model().is_none());
        }
    }

    #[tokio::test]
    async fn manually_authored_diamond_is_saved_reopened_and_executed_without_planning() {
        let repo = tempfile::tempdir().unwrap();
        let mut app = app(repo.path());
        app.manual_commit(Field::Goal, "自分で組んだ分岐と合流を実行する")
            .unwrap();
        app.manual_add_node(3, false).unwrap();
        app.manual_commit(Field::Body, "printf start > start.txt")
            .unwrap();
        app.manual_add_node(3, true).unwrap();
        app.manual_commit(Field::Body, "test -f start.txt && printf left > left.txt")
            .unwrap();
        app.selected_node = 0;
        app.manual_add_node(3, true).unwrap();
        app.manual_commit(Field::Body, "test -f start.txt && printf right > right.txt")
            .unwrap();
        app.selected_node = 1;
        app.manual_add_node(4, true).unwrap();
        app.manual_commit(
            Field::Body,
            "test -f left.txt && test -f right.txt && printf MANUAL_DIAMOND_OK",
        )
        .unwrap();
        app.manual_select(
            Page::ConnectionKind {
                from: "step_3".into(),
                to: "step_4".into(),
            },
            0,
        )
        .unwrap();
        app.manual_commit(Field::Parallel, "2").unwrap();
        assert_eq!(
            app.graph.spec.edges,
            vec![
                Edge::data("step_1", "step_2"),
                Edge::data("step_1", "step_3"),
                Edge::data("step_2", "step_4"),
                Edge::data("step_3", "step_4")
            ]
        );
        assert!(!repo.path().join(".gloop/runs").exists());
        assert!(app.active_run.is_none());
        app.manual_save_as("diamond").unwrap();
        let expected = app.graph.clone();
        let path = app.graph_path.clone();
        app.open_manual_path(&path).unwrap();
        assert_eq!(app.graph, expected);
        app.review_manual_run();
        assert!(matches!(
            app.modal,
            Some(Modal::Manual(Menu {
                page: Page::Run,
                ..
            }))
        ));
        assert!(app.active_run.is_none());
        assert_eq!(app.manual_select(Page::Run, 0).unwrap(), Action::Run);
        app.start_run();
        tokio::time::timeout(Duration::from_secs(15), async {
            while app.active_run.is_some() {
                app.drain_run_events().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let summary = app.last_summary.as_ref().unwrap();
        assert_eq!(summary.status, gloop_core::FinalStatus::ReadyForHuman);
        assert!(
            summary
                .nodes
                .values()
                .all(|node| node.status == NodeStatus::Succeeded)
        );
        assert!(
            summary.nodes["step_4"]
                .output
                .as_ref()
                .unwrap()
                .to_string()
                .contains("MANUAL_DIAMOND_OK")
        );
        assert_eq!(app.graph, expected);
        assert_eq!(Graph::from_path(path).unwrap(), expected);
        assert!(!repo.path().join(".gloop/jobs").exists());
    }

    #[tokio::test]
    async fn opening_custom_graph_preserves_bindings_and_refuses_external_overwrite() {
        let repo = tempfile::tempdir().unwrap();
        let mut first = Node::agent("writer", "write the result");
        if let NodeKind::Agent {
            profile,
            model,
            fan_out,
            ..
        } = &mut first.kind
        {
            *profile = Some("codex".into());
            *model = Some("writer-model".into());
            *fan_out = 2;
        }
        let mut second = Node::agent("reviewer", "review preceding results");
        if let NodeKind::Agent { profile, model, .. } = &mut second.kind {
            *profile = Some("claude".into());
            *model = Some("haiku".into());
        }
        let mut graph = Graph::new("custom", "A user-owned graph", vec![first, second]);
        graph.spec.edges = vec![Edge::data("writer", "reviewer")];
        graph.spec.budgets.model_calls = Some(7);
        graph.spec.policies.max_parallel = 2;
        let path = repo.path().join("custom.yaml");
        fs::write(&path, graph.to_yaml().unwrap()).unwrap();
        let mut app = app(repo.path());
        app.open_manual_path(&path).unwrap();
        assert_eq!(app.graph, graph);
        app.manual_commit(Field::Goal, "Changed goal only").unwrap();
        assert_eq!(app.graph.spec.nodes, graph.spec.nodes);
        assert_eq!(app.graph.spec.edges, graph.spec.edges);
        app.save().await.unwrap();
        let saved = fs::read(&path).unwrap();
        assert_eq!(Graph::from_path(&path).unwrap(), app.graph);
        app.manual_commit(Field::Title, "Edited name").unwrap();
        fs::write(
            &path,
            format!("# changed elsewhere\n{}", String::from_utf8(saved).unwrap()),
        )
        .unwrap();
        assert!(app.save().await.is_err());
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .starts_with("# changed elsewhere")
        );
    }

    #[test]
    fn manual_connections_reject_cycles_and_preserve_other_edges_when_removed() {
        let repo = tempfile::tempdir().unwrap();
        let mut app = app(repo.path());
        app.manual_commit(Field::Goal, "Two user-authored steps")
            .unwrap();
        for after in [false, true] {
            app.manual_add_node(0, after).unwrap();
            app.manual_commit(Field::Body, "Reply to the user").unwrap();
        }
        let before = app.graph.clone();
        assert!(
            app.manual_select(
                Page::ConnectionKind {
                    from: "step_2".into(),
                    to: "step_1".into()
                },
                0
            )
            .is_err()
        );
        assert_eq!(app.graph, before);
        app.manual_select(Page::Edge(0), 1).unwrap();
        assert!(app.graph.spec.edges.is_empty());
        assert_eq!(app.graph.spec.nodes, before.spec.nodes);
        app.manual_save_as("own-graph").unwrap();
        assert!(app.manual_save_as("own-graph").is_err());
        app.begin_input(InputTarget::Manual(Field::Title));
        app.handle_key(KeyEvent::new(
            KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(
            matches!(&app.modal, Some(Modal::Input(input)) if input.value.is_empty() && input.cursor == 0)
        );
        app.handle_paste("Keep this edit");
        app.handle_key(KeyEvent::new(
            KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(
            app.graph.spec.nodes[app.selected_node].label.as_deref(),
            Some("Keep this edit")
        );
        app.manual_destination(Destination::New);
        assert!(matches!(
            app.modal,
            Some(Modal::Manual(Menu {
                page: Page::Discard(_),
                ..
            }))
        ));
        assert_eq!(app.graph.spec.nodes.len(), 2);
    }

    #[test]
    fn manual_menu_keeps_errors_visible_on_a_small_terminal() {
        let repo = tempfile::tempdir().unwrap();
        let mut app = app(repo.path());
        app.menu(Page::Connections);
        app.status = "CONNECTION_REJECTED".to_owned();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| super::super::render(frame, &app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("CONNECTION_REJECTED"));
    }
}
