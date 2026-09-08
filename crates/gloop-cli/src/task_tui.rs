//! Task-first terminal workspace. Workers and persisted results live in `jobs`.

use std::{
    fmt::Write as _,
    io::{self, IsTerminal},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use gloop_core::{Graph, NodeKind};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    i18n::Language,
    jobs::{self, StartRequest},
    templates, workspace,
};

mod planning_flow;
use planning_flow::Page as PlanPage;

const ACCENT: Color = Color::Rgb(102, 214, 164);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Plan(PlanPage),
    Home,
    Goal,
    Providers(bool),
    Models(bool),
    CustomModel(bool),
    Review,
    Confirm,
    Limits,
    Calls,
    History,
    Detail,
    Tools,
    Workflows,
}

enum Message {
    Setup(Result<Value>),
    History(Result<Value>),
    Submitted(Result<Value>),
    Stopped(Result<Value>),
    Detail(String, Result<Value>),
}

#[allow(clippy::struct_excessive_bools)]
struct App {
    repo: PathBuf,
    trusted: bool,
    lang: Language,
    screen: Screen,
    selected: usize,
    draft: StartRequest,
    plan: Option<crate::planning::DraftPlan>,
    input: String,
    cursor: usize,
    submission: Option<(StartRequest, String)>,
    profiles: Vec<Value>,
    setup_pending: bool,
    tasks: Vec<Value>,
    workflows: Vec<(PathBuf, Graph)>,
    manual_path: Option<PathBuf>,
    task: Value,
    message: String,
    pending: bool,
    detail_pending: bool,
    failures: u8,
    refresh_at: Instant,
    refresh_deadline: Instant,
    scroll: u16,
    scroll_page: u16,
    tx: mpsc::UnboundedSender<Message>,
}

fn blank_request() -> StartRequest {
    StartRequest {
        kind: jobs::TaskKind::Task,
        goal: String::new(),
        profile: None,
        model: None,
        review_profile: None,
        review_model: None,
        after: None,
        graph: None,
        timeout_seconds: jobs::default_timeout(),
        max_calls: 1,
    }
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

impl App {
    fn new(
        repo: PathBuf,
        trusted: bool,
        lang: Language,
        tx: mpsc::UnboundedSender<Message>,
    ) -> Self {
        let mut app = Self {
            repo,
            trusted,
            lang,
            screen: Screen::Home,
            selected: 0,
            draft: blank_request(),
            plan: None,
            input: String::new(),
            cursor: 0,
            submission: None,
            profiles: vec![],
            setup_pending: false,
            tasks: vec![],
            workflows: vec![],
            manual_path: None,
            task: Value::Null,
            message: String::new(),
            pending: false,
            detail_pending: false,
            failures: 0,
            refresh_at: Instant::now(),
            refresh_deadline: Instant::now(),
            scroll: 0,
            scroll_page: 1,
            tx,
        };
        app.load_workflows();
        app
    }

    fn load_workflows(&mut self) {
        self.workflows.clear();
        let paths = match templates::list_graph_files(&self.repo) {
            Ok(paths) => paths,
            Err(error) => {
                self.message = error;
                return;
            }
        };
        let mut paths = paths.into_iter().take(100).collect::<Vec<_>>();
        paths.sort_by_key(|path| {
            std::cmp::Reverse(std::fs::metadata(path).and_then(|m| m.modified()).ok())
        });
        let mut errors = Vec::new();
        for path in paths {
            match Graph::from_path(&path) {
                Ok(graph) => self.workflows.push((path, graph)),
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            }
        }
        self.message = errors.join("\n");
    }

    fn recent_count(&self) -> usize {
        self.workflows.len().min(5)
    }

    fn text<'a>(&self, en: &'a str, ja: &'a str) -> &'a str {
        if self.lang == Language::Ja { ja } else { en }
    }

    fn go(&mut self, screen: Screen) {
        self.screen = screen;
        self.selected = 0;
        self.scroll = 0;
    }

    fn setup(&mut self) {
        self.setup_pending = true;
        let (repo, trusted, tx) = (self.repo.clone(), self.trusted, self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Message::Setup(
                workspace::provider_setup(&repo, trusted).await,
            ));
        });
    }

    fn available(&self) -> Vec<&Value> {
        self.profiles
            .iter()
            .filter(|p| p["enabled"] == true && p["available"] == true)
            .collect()
    }

    fn models(&self, review: bool) -> Vec<String> {
        let profile = if review {
            &self.draft.review_profile
        } else {
            &self.draft.profile
        };
        self.catalog_models(profile.as_deref())
    }

    fn catalog_models(&self, profile: Option<&str>) -> Vec<String> {
        self.profiles
            .iter()
            .find(|p| Some(string(p, "name")) == profile)
            .and_then(|p| p["models"].as_array())
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| {
                        model["id"]
                            .as_str()
                            .or_else(|| model.as_str())
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn history(&mut self) {
        self.go(Screen::History);
        self.message = self
            .text("Loading saved tasks…", "保存済みのタスクを読み込み中…")
            .to_owned();
        let (repo, tx) = (self.repo.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Message::History(jobs::list(&repo)));
        });
    }

    fn open_task(&mut self, task: Value) {
        self.task = task;
        self.go(Screen::Detail);
        self.failures = 0;
        self.refresh_deadline = Instant::now()
            + Duration::from_secs(
                self.task["job"]["request"]["timeout_seconds"]
                    .as_u64()
                    .unwrap_or(1800)
                    + 45,
            );
        self.refresh_at = Instant::now();
        self.detail_pending = false;
        self.refresh();
    }

    fn refresh(&mut self) {
        if self.detail_pending {
            return;
        }
        let id = string(&self.task, "id").to_owned();
        if id.is_empty() {
            return;
        }
        self.detail_pending = true;
        let (repo, tx) = (self.repo.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = jobs::detail(&repo, &id).await;
            let _ = tx.send(Message::Detail(id, result));
        });
    }

    fn receive(&mut self, message: Message) {
        match message {
            Message::Setup(result) => {
                self.setup_pending = false;
                match result {
                    Ok(value) => {
                        self.profiles = value["profiles"].as_array().cloned().unwrap_or_default();
                    }
                    Err(error) => self.message = format!("{error:#}"),
                }
            }
            Message::History(result) => match result {
                Ok(value) => {
                    self.tasks = value["tasks"].as_array().cloned().unwrap_or_default();
                    self.message = value["errors"]
                        .as_array()
                        .filter(|errors| !errors.is_empty())
                        .map_or_else(String::new, |errors| {
                            format!(
                                "{}: {errors:?}",
                                self.text("Unreadable tasks", "読み込めないタスク")
                            )
                        });
                }
                Err(error) => self.message = format!("{error:#}"),
            },
            Message::Submitted(result) => {
                self.pending = false;
                match result {
                    Ok(task) => {
                        self.message = self
                            .text(
                                "Started independently. Closing this view keeps the task running.",
                                "独立したプロセスで開始しました。画面を閉じてもタスクは続きます。",
                            )
                            .to_owned();
                        self.open_task(task);
                    }
                    Err(error) => self.message = format!("{error:#}"),
                }
            }
            Message::Stopped(result) => {
                self.pending = false;
                match result {
                    Ok(task) => {
                        self.message = self
                            .text(
                                "Stop requested. Waiting for the saved terminal result.",
                                "停止を要求しました。終了結果の保存を待っています。",
                            )
                            .to_owned();
                        self.open_task(task);
                    }
                    Err(error) => self.message = format!("{error:#}"),
                }
            }
            Message::Detail(id, result) if id == string(&self.task, "id") => {
                self.detail_pending = false;
                self.refresh_at = Instant::now() + Duration::from_secs(2);
                match result {
                    Ok(task) => {
                        self.task = task;
                        self.failures = 0;
                        if self.screen == Screen::Detail
                            && self.task["job"]["request"]["kind"] == "planning"
                            && self.task["status"] == "completed"
                            && let Err(error) = self.open_plan()
                        {
                            self.message = format!("{error:#}");
                        }
                    }
                    Err(error) => {
                        self.failures += 1;
                        self.message = format!("{error:#}");
                    }
                }
            }
            Message::Detail(_, _) => {}
        }
    }

    #[allow(clippy::too_many_lines)] // Keep the menu labels together by screen.
    fn choices(&self) -> Vec<String> {
        let t = |en, ja| self.text(en, ja).to_owned();
        match self.screen {
            Screen::Plan(page) => self.plan_choices(page),
            Screen::Home => {
                let mut choices = self
                    .workflows
                    .iter()
                    .take(self.recent_count())
                    .map(|(_, graph)| {
                        format!(
                            "▦ {}  · {} {}\n  {}",
                            safe_text(&graph.metadata.name),
                            graph.spec.nodes.len(),
                            self.text("steps", "作業"),
                            safe_text(graph.spec.goal.lines().next().unwrap_or_default())
                        )
                    })
                    .collect::<Vec<_>>();
                choices.extend([
                    t("+ Graph · Manual", "+ グラフ · Manual"),
                    t("✦ Auto", "✦ Auto"),
                    t("1 AI", "1 AI"),
                    t("History", "履歴"),
                    t("Background run", "バックグラウンド実行"),
                    t("Settings", "設定"),
                ]);
                choices
            }
            Screen::Providers(_) | Screen::Review => {
                let mut choices = Vec::new();
                if self.screen == Screen::Review {
                    choices.push(t(
                        "No second opinion — run once",
                        "追加の確認なし — 1回だけ実行",
                    ));
                }
                choices.extend(
                    self.available()
                        .iter()
                        .map(|p| format!("{}   · {}", string(p, "name"), string(p, "kind"))),
                );
                choices
            }
            Screen::Models(review) => {
                let mut choices = vec![t("Use this tool's default", "このツールの既定モデル")];
                choices.extend(self.models(review));
                choices.push(t("Enter a model id / alias…", "モデルID・別名を直接入力…"));
                choices
            }
            Screen::Confirm => vec![
                if self.draft.kind == jobs::TaskKind::Planning {
                    t(
                        "Create a plan · 1 AI call",
                        "手順案を作る · AIを1回呼び出す",
                    )
                } else {
                    t("Start task", "この内容で開始")
                },
                t("Time limit", "制限時間を変更"),
                if self.draft.kind == jobs::TaskKind::Planning {
                    t("AI calls: 1 (fixed)", "AI呼び出し: 1回（固定）")
                } else {
                    t("Maximum AI calls", "AI呼び出し上限を変更")
                },
                if self.draft.graph.is_none() {
                    t("Edit request", "依頼を編集")
                } else if self.plan.is_some() {
                    t("Back to the steps", "手順の編集に戻る")
                } else {
                    t("Choose another workflow", "別のワークフローを選ぶ")
                },
            ],
            Screen::Limits => [300, 900, 1800, 3600]
                .map(|seconds| format!("{} {}", seconds / 60, self.text("minutes", "分")))
                .to_vec(),
            Screen::Calls => [1, 2, 3, 5, 10, 32]
                .map(|count| format!("{count} {}", self.text("calls maximum", "回まで")))
                .to_vec(),
            Screen::History => self
                .tasks
                .iter()
                .map(|task| {
                    format!(
                        "[{}{}] {}  · {}",
                        self.status(string(task, "status")),
                        if task["job"]["request"]["kind"] == "planning" {
                            self.text(" · plan", " · 手順案")
                        } else {
                            ""
                        },
                        string(&task["job"]["request"], "goal").replace('\n', " "),
                        string(&task["job"]["request"], "profile")
                    )
                })
                .collect(),
            Screen::Detail => self.detail_choices(),
            Screen::Tools => self
                .profiles
                .iter()
                .map(|p| {
                    format!(
                        "{}  {}  {}",
                        if p["available"] == true && p["enabled"] == true {
                            "✓"
                        } else {
                            "×"
                        },
                        string(p, "name"),
                        string(p, "reason")
                    )
                })
                .collect(),
            Screen::Workflows => self
                .workflows
                .iter()
                .map(|(path, graph)| format!("{}  · {}", graph.metadata.name, path.display()))
                .collect(),
            _ => vec![],
        }
    }

    fn detail_choices(&self) -> Vec<String> {
        let t = |en, ja| self.text(en, ja).to_owned();
        let primary =
            if self.task["finished"] == true && self.task["job"]["request"]["kind"] == "planning" {
                if self.task["status"] == "completed" {
                    t("Review these steps", "手順案を確認する")
                } else {
                    t("Edit the request and try again", "依頼文を確認して作り直す")
                }
            } else if self.task["finished"] == true {
                t("Ask another model / follow up", "結果を渡して、次の依頼へ")
            } else {
                t("Stop this task", "このタスクを停止")
            };
        vec![
            primary,
            t("Tasks & results", "タスク・結果を見る"),
            t("New task", "新しい依頼"),
        ]
    }

    fn status<'a>(&self, value: &'a str) -> &'a str {
        if self.lang != Language::Ja {
            return value;
        }
        match value {
            "starting" => "開始中",
            "running" => "実行中",
            "completed" => "完了",
            "failed" => "失敗",
            "cancelled" => "停止済み",
            "stopping" => "停止中",
            "budget_exhausted" => "上限に到達",
            "blocked" => "要確認",
            "interrupted" => "中断",
            _ => value,
        }
    }

    fn start_draft(&mut self, after: Option<String>) {
        self.draft = blank_request();
        self.plan = None;
        self.draft.after = after;
        self.input.clear();
        self.cursor = 0;
        self.submission = None;
        self.message.clear();
        self.go(Screen::Goal);
    }

    fn model_chosen(&mut self, review: bool, model: Option<String>) {
        if review {
            self.draft.review_model = model;
            self.go(Screen::Confirm);
        } else {
            self.draft.model = model;
            self.go(if self.draft.kind == jobs::TaskKind::Planning {
                Screen::Confirm
            } else {
                Screen::Review
            });
        }
    }

    fn back(&mut self) {
        if let Screen::Plan(page) = self.screen {
            self.back_plan(page);
            return;
        }
        match self.screen {
            Screen::Confirm if self.plan.is_some() && self.draft.graph.is_some() => {
                self.go(Screen::Plan(PlanPage::Overview));
            }
            Screen::Confirm if self.draft.kind == jobs::TaskKind::Planning => {
                self.go(Screen::Models(false));
            }
            Screen::Providers(false) => {
                self.input.clone_from(&self.draft.goal);
                self.cursor = self.input.len();
                self.go(Screen::Goal);
            }
            Screen::Models(false) => self.go(Screen::Providers(false)),
            Screen::Models(true) | Screen::Providers(true) => self.go(Screen::Review),
            Screen::CustomModel(review) => self.go(Screen::Models(review)),
            Screen::Review => self.go(Screen::Models(false)),
            Screen::Limits | Screen::Calls => self.go(Screen::Confirm),
            Screen::Confirm if self.draft.graph.is_none() => self.go(Screen::Review),
            _ => self.go(Screen::Home),
        }
    }

    // Returns true only when the user requests the advanced editor.
    #[allow(clippy::too_many_lines)]
    fn enter(&mut self) -> Result<bool> {
        let selected = self.selected;
        match self.screen {
            Screen::Plan(page) => self.enter_plan(page)?,
            Screen::Home if selected < self.recent_count() => {
                self.manual_path = Some(self.workflows[selected].0.clone());
                return Ok(true);
            }
            Screen::Home => match selected.saturating_sub(self.recent_count()) {
                0 => {
                    self.manual_path = None;
                    return Ok(true);
                }
                1 => self.start_planning(),
                2 => self.start_draft(None),
                3 => self.history(),
                4 => {
                    self.plan = None;
                    self.load_workflows();
                    self.go(Screen::Workflows);
                }
                5 => self.go(Screen::Tools),
                _ => {}
            },
            Screen::Goal => {
                if self.input.trim().is_empty() {
                    self.message = self
                        .text("Enter a task first.", "依頼内容を入力してください。")
                        .to_owned();
                } else {
                    self.draft.goal = self.input.trim().to_owned();
                    self.message.clear();
                    self.go(Screen::Providers(false));
                }
            }
            Screen::Providers(review) => {
                if let Some(profile) = self.available().get(selected) {
                    let name = Some(string(profile, "name").to_owned());
                    if review {
                        self.draft.review_profile = name;
                    } else {
                        self.draft.profile = name;
                    }
                    self.go(Screen::Models(review));
                }
            }
            Screen::Models(review) => {
                let models = self.models(review);
                if selected == 0 {
                    self.model_chosen(review, None);
                } else if let Some(model) = models.get(selected - 1) {
                    self.model_chosen(review, Some(model.clone()));
                } else {
                    self.input.clear();
                    self.cursor = 0;
                    self.go(Screen::CustomModel(review));
                }
            }
            Screen::CustomModel(review) => {
                if !self.input.trim().is_empty() {
                    self.model_chosen(review, Some(self.input.trim().to_owned()));
                }
            }
            Screen::Review => {
                if selected == 0 {
                    self.draft.review_profile = None;
                    self.draft.review_model = None;
                    self.draft.max_calls = 1;
                    self.go(Screen::Confirm);
                } else if let Some(profile) = self.available().get(selected - 1) {
                    self.draft.review_profile = Some(string(profile, "name").to_owned());
                    self.draft.max_calls = 2;
                    self.go(Screen::Models(true));
                }
            }
            Screen::Confirm => match selected {
                0 if !self.pending => {
                    self.pending = true;
                    self.message = self.text("Starting…", "開始しています…").to_owned();
                    let (repo, request, trusted, tx) = (
                        self.repo.clone(),
                        self.draft.clone(),
                        self.trusted,
                        self.tx.clone(),
                    );
                    if self
                        .submission
                        .as_ref()
                        .is_some_and(|(previous, _)| previous != &self.draft)
                    {
                        self.submission = None;
                    }
                    let id = self
                        .submission
                        .get_or_insert_with(|| {
                            (
                                self.draft.clone(),
                                ulid::Ulid::new().to_string().to_lowercase(),
                            )
                        })
                        .1
                        .clone();
                    tokio::spawn(async move {
                        let _ = tx.send(Message::Submitted(
                            jobs::start(&repo, request, trusted, Some(id)).await,
                        ));
                    });
                }
                1 => self.go(Screen::Limits),
                2 if self.draft.kind != jobs::TaskKind::Planning => self.go(Screen::Calls),
                3 if self.draft.graph.is_none() => {
                    self.input.clone_from(&self.draft.goal);
                    self.cursor = self.input.len();
                    self.submission = None;
                    self.go(Screen::Goal);
                }
                3 if self.plan.is_some() => self.go(Screen::Plan(PlanPage::Overview)),
                3 => self.go(Screen::Workflows),
                _ => {}
            },
            Screen::Limits => {
                self.draft.timeout_seconds = [300, 900, 1800, 3600][selected];
                self.submission = None;
                self.go(Screen::Confirm);
            }
            Screen::Calls => {
                self.draft.max_calls = [1, 2, 3, 5, 10, 32][selected];
                self.submission = None;
                self.go(Screen::Confirm);
            }
            Screen::History => {
                if let Some(task) = self.tasks.get(selected) {
                    self.open_task(task.clone());
                }
            }
            Screen::Detail => match selected {
                0 if self.task["finished"] == true
                    && self.task["job"]["request"]["kind"] == "planning" =>
                {
                    if self.task["status"] == "completed" {
                        self.open_plan()?;
                    } else {
                        let goal = string(&self.task["job"]["request"], "goal").to_owned();
                        self.start_planning();
                        self.input = goal;
                        self.cursor = self.input.len();
                    }
                }
                0 if self.task["finished"] == true => {
                    self.start_draft(Some(string(&self.task, "id").to_owned()));
                }
                0 if !self.pending => {
                    self.pending = true;
                    let (repo, id, tx) = (
                        self.repo.clone(),
                        string(&self.task, "id").to_owned(),
                        self.tx.clone(),
                    );
                    tokio::spawn(async move {
                        let _ = tx.send(Message::Stopped(jobs::cancel(&repo, &id).await));
                    });
                }
                1 => self.history(),
                2 => self.start_draft(None),
                _ => {}
            },
            Screen::Tools => {
                if let Some(profile) = self.profiles.get(selected) {
                    self.message = format!("{}: {}\n{}", string(profile, "name"), string(profile, "executable"), self.text("Run this CLI in a terminal to sign in, then press r here to check again. API profiles: gloop provider doctor.", "端末でこのCLIを起動してログイン後、ここで r を押して再確認できます。API設定の診断: gloop provider doctor"));
                }
            }
            Screen::Workflows => {
                if let Some((_, graph)) = self.workflows.get(selected) {
                    self.draft = blank_request();
                    self.submission = None;
                    self.draft.goal.clone_from(&graph.spec.goal);
                    self.draft.graph = Some(graph.clone());
                    self.draft.max_calls = graph.spec.budgets.model_calls.unwrap_or(3).clamp(1, 32);
                    self.draft.timeout_seconds = graph
                        .spec
                        .budgets
                        .wall_time_seconds
                        .unwrap_or(jobs::default_timeout())
                        .clamp(10, 3600);
                    self.go(Screen::Confirm);
                }
            }
        }
        Ok(false)
    }

    #[allow(clippy::too_many_lines)]
    fn key(&mut self, key: KeyEvent) -> Result<Action> {
        if key.kind == KeyEventKind::Release {
            return Ok(Action::Continue);
        }
        if self.pending {
            return Ok(Action::Continue);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(Action::Quit);
        }
        if self.is_input() {
            match key.code {
                KeyCode::Esc => self.back(),
                KeyCode::Enter
                    if key.modifiers.contains(KeyModifiers::ALT)
                        && (self.screen == Screen::Goal
                            || matches!(self.screen, Screen::Plan(page) if page.multiline())) =>
                {
                    self.insert("\n");
                }
                KeyCode::Enter => {
                    self.enter()?;
                }
                KeyCode::Left => {
                    self.cursor = self.input[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                }
                KeyCode::Right => {
                    if let Some(c) = self.input[self.cursor..].chars().next() {
                        self.cursor += c.len_utf8();
                    }
                }
                KeyCode::Home => {
                    self.cursor = self.input[..self.cursor]
                        .rfind('\n')
                        .map_or(0, |index| index + 1);
                }
                KeyCode::End => {
                    self.cursor += self.input[self.cursor..]
                        .find('\n')
                        .unwrap_or(self.input.len() - self.cursor);
                }
                KeyCode::Backspace => {
                    if self.cursor > 0 {
                        let previous = self.input[..self.cursor]
                            .char_indices()
                            .next_back()
                            .map_or(0, |(index, _)| index);
                        self.input.drain(previous..self.cursor);
                        self.cursor = previous;
                    }
                }
                KeyCode::Delete => {
                    if let Some(c) = self.input[self.cursor..].chars().next() {
                        self.input.drain(self.cursor..self.cursor + c.len_utf8());
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear();
                    self.cursor = 0;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.insert(&c.to_string());
                }
                _ => {}
            }
            return Ok(Action::Continue);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(Action::Quit),
            KeyCode::Esc if !self.pending => self.back(),
            KeyCode::Enter if self.enter()? => return Ok(Action::Advanced),
            KeyCode::Tab | KeyCode::Down
                if self.screen != Screen::Detail || key.code == KeyCode::Tab =>
            {
                let count = self.choices().len();
                if count > 0 {
                    self.selected = (self.selected + 1) % count;
                }
            }
            KeyCode::BackTab | KeyCode::Up
                if self.screen != Screen::Detail || key.code == KeyCode::BackTab =>
            {
                let count = self.choices().len();
                if count > 0 {
                    self.selected = (self.selected + count - 1) % count;
                }
            }
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(self.scroll_page),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.scroll_page),
            KeyCode::Home => self.scroll = 0,
            KeyCode::Char('r') if self.screen == Screen::Detail => {
                self.failures = 0;
                self.refresh_deadline = Instant::now() + Duration::from_secs(3600);
                self.refresh();
            }
            KeyCode::Char('r')
                if !self.setup_pending
                    && matches!(
                        self.screen,
                        Screen::Tools | Screen::Providers(_) | Screen::Review
                    ) =>
            {
                self.setup();
            }
            _ => {}
        }
        Ok(Action::Continue)
    }

    fn insert(&mut self, text: &str) {
        let remaining = (32 * 1024_usize).saturating_sub(self.input.len());
        let text = safe_text(&text.replace("\r\n", "\n").replace('\r', "\n"));
        if text.len() > remaining {
            self.message = self
                .text(
                    "This text was not inserted. Shorten the request to fit within 32 KiB.",
                    "文字数の上限を超えるため入力できませんでした。全体を32 KiB以内に短くしてください。",
                )
                .to_owned();
            return;
        }
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    fn is_input(&self) -> bool {
        matches!(self.screen, Screen::Goal | Screen::CustomModel(_))
            || matches!(self.screen, Screen::Plan(page) if page.is_input())
    }

    fn heading(&self) -> &str {
        if self.draft.kind == jobs::TaskKind::Planning {
            match self.screen {
                Screen::Goal => {
                    return self.text(
                        "1 / 3  Describe what you want to achieve",
                        "1 / 3  やりたいことを書く",
                    );
                }
                Screen::Providers(_) | Screen::Models(_) | Screen::CustomModel(_) => {
                    return self.text(
                        "2 / 3  Choose the AI that proposes the steps",
                        "2 / 3  手順案を作るAIを選ぶ",
                    );
                }
                Screen::Confirm => {
                    return self.text(
                        "3 / 3  Create the proposal, then review it",
                        "3 / 3  手順案を作成して確認へ",
                    );
                }
                _ => {}
            }
        }
        match self.screen {
            Screen::Plan(page) => self.plan_heading(page),
            Screen::Home => self.text("Workspace", "ワークスペース"),
            Screen::Goal => self.text("1 / 4  Describe your task", "1 / 4  依頼内容を入力"),
            Screen::Providers(_) | Screen::Models(_) | Screen::CustomModel(_) => self.text(
                "2 / 4  Choose an AI tool and model",
                "2 / 4  AIツール・モデルを選択",
            ),
            Screen::Review => self.text(
                "3 / 4  Would you like a second opinion?",
                "3 / 4  別のAIにも結果を確認してもらう",
            ),
            Screen::Confirm | Screen::Limits | Screen::Calls => {
                if self.draft.graph.is_some() {
                    self.text(
                        "Review steps and limits, then start",
                        "手順・上限を確認して開始",
                    )
                } else {
                    self.text("4 / 4  Review and start", "4 / 4  内容を確認して開始")
                }
            }
            Screen::History => self.text("Tasks & results", "タスク・結果"),
            Screen::Detail => self.text("Task result", "タスクの実行状況・結果"),
            Screen::Tools => self.text("AI tools & setup", "AIツール・接続状況"),
            Screen::Workflows => self.text("Saved workflows", "保存したワークフロー"),
        }
    }

    fn description(&self) -> String {
        if self.screen == Screen::Confirm && self.draft.kind == jobs::TaskKind::Planning {
            return format!(
                "{}\n\n{} / {}\n{}\n{}\n{}: {}",
                self.draft.goal,
                self.draft.profile.as_deref().unwrap_or("—"),
                self.draft.model.as_deref().unwrap_or("default"),
                self.text(
                    "One AI call proposes a small set of steps and completion criteria.",
                    "AIを1回呼び出し、作業手順と完了条件を提案してもらいます。"
                ),
                self.text(
                    "Review or edit the proposal before starting any implementation.",
                    "提案を確認・修正した後に、実作業を開始できます。"
                ),
                self.text("Project", "対象プロジェクト"),
                self.repo.display()
            );
        }
        match self.screen {
            Screen::Plan(page) => self.plan_description(page),
            Screen::Goal => self.draft.after.as_ref().map_or_else(|| self.text("Describe the outcome you want. You can paste a multiline request.", "何をしてほしいか入力してください。複数行の貼り付けにも対応しています。").to_owned(), |id| format!("{} {id}\n{}", self.text("Previous result:", "引き継ぎ元:"), self.text("The saved result will be included. Enter your next request.", "保存された結果を渡します。次の依頼を入力してください。"))),
            Screen::Providers(_) | Screen::Review => {
                if self.setup_pending { self.text("Checking installed tools and model catalogs…", "インストール済みのツールとモデル一覧を確認中…").to_owned() }
                else if self.available().is_empty() { self.text("No tools are available. Esc → AI tools & setup shows what to configure. Press r to check again.", "利用できるツールがありません。Esc → AIツール・接続状況で設定を確認できます。r で再確認。").to_owned() }
                else if self.screen == Screen::Review { self.text("The selected AI will inspect the first result. This adds one call.", "選んだAIが最初の結果を確認します。AI呼び出しが1回増えます。").to_owned() }
                else { self.text("Choose the CLI or API to use. Installed does not necessarily mean signed in.", "実行するCLI・APIを選びます。利用時には各ツールへのログインが必要です。").to_owned() }
            }
            Screen::Models(review) => format!("{}\n{}", if review { self.draft.review_profile.as_deref().unwrap_or("") } else { self.draft.profile.as_deref().unwrap_or("") }, self.text("Choose from this tool's catalog, or use its default.", "このツールのモデル一覧から選ぶか、既定モデルを使います。")),
            Screen::Confirm | Screen::Limits | Screen::Calls => {
                self.execution_description()
            }
            Screen::History => self.text("Enter opens a task, including work started from another terminal or AI.", "Enter で詳細を開きます。別の端末やAIが開始したタスクも確認できます。").to_owned(),
            Screen::Tools => self.text("Enter shows setup guidance. r checks tools again.\nAn installed CLI still requires its own login. No model call is made by this check.", "Enter で設定方法を表示。r で再確認。\nCLIは別途ログインが必要です。この確認ではモデルを呼び出しません。").to_owned(),
            Screen::Workflows => self.text("Select a saved graph to review its steps and run it independently.", "保存したグラフを選び、手順を確認して独立実行できます。").to_owned(),
            _ => String::new(),
        }
    }

    fn execution_description(&self) -> String {
        let mut description = String::new();
        let (mut calls, mut seconds) = (self.draft.max_calls, self.draft.timeout_seconds);
        if let Some(graph) = &self.draft.graph {
            calls = calls.min(graph.spec.budgets.model_calls.unwrap_or(calls));
            seconds = seconds.min(graph.spec.budgets.wall_time_seconds.unwrap_or(seconds));
            for (index, node) in graph.spec.nodes.iter().enumerate() {
                let name = node.label.as_deref().unwrap_or(&node.id);
                let _ = write!(description, "\n{}. {name}\n   ", index + 1);
                if let NodeKind::Command { argv, .. } = &node.kind {
                    let _ = write!(
                        description,
                        "command {}",
                        serde_json::to_string(argv).unwrap_or_default()
                    );
                } else {
                    let _ = write!(
                        description,
                        "{} / {}",
                        node.profile().unwrap_or("default"),
                        node.model().unwrap_or("default")
                    );
                }
            }
        } else {
            let _ = write!(
                description,
                "\n\n{}: {} / {}",
                self.text("AI", "実行AI"),
                self.draft.profile.as_deref().unwrap_or("—"),
                self.draft.model.as_deref().unwrap_or("default")
            );
            if let Some(profile) = &self.draft.review_profile {
                let _ = write!(
                    description,
                    "\n{}: {profile} / {}",
                    self.text("Second opinion", "追加確認"),
                    self.draft.review_model.as_deref().unwrap_or("default")
                );
            }
        }
        if let Some(after) = &self.draft.after {
            let _ = write!(
                description,
                "\n{}: {after}",
                self.text("Previous result", "引き継ぎ元")
            );
        }
        description = format!(
            "{}: {} min    {}: {calls}\n\n{}\n{description}\n{}: {}",
            self.text("Time limit", "制限時間"),
            seconds / 60,
            self.text("Maximum calls", "AI呼び出し上限"),
            self.draft.goal,
            self.text("Project", "対象プロジェクト"),
            self.repo.display()
        );
        description.push_str(self.text(
            "\n\nUses your selected tool's account and permissions in this project.",
            "\n\nこのプロジェクトで、選んだツールのアカウントと権限で実行します。",
        ));
        description
    }

    #[allow(clippy::too_many_lines)]
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 55 || area.height < 18 {
            frame.render_widget(Paragraph::new(self.text("Enlarge the terminal to at least 55 × 18. Ctrl-C closes the view; tasks continue.", "端末を55列×18行以上に広げてください。Ctrl-Cで画面を閉じても実行中のタスクは続きます。")), area);
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        " gloop ",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" · {}", self.repo.display())),
                ]),
                Line::from(format!(" {}", self.heading())),
            ]),
            rows[0],
        );
        if self.screen == Screen::Detail {
            self.draw_detail(frame, rows[1]);
        } else if self.is_input() {
            let parts =
                Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(rows[1]);
            frame.render_widget(
                Paragraph::new(self.description()).wrap(Wrap { trim: false }),
                parts[0],
            );
            let input = format!(
                "{}▏{}",
                &self.input[..self.cursor],
                &self.input[self.cursor..]
            );
            let paragraph = Paragraph::new(input).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT)),
            );
            let visible = parts[1].height.saturating_sub(2);
            let line_count = Paragraph::new(format!("{}▏", &self.input[..self.cursor]))
                .wrap(Wrap { trim: false })
                .line_count(parts[1].width.saturating_sub(2));
            let offset = u16::try_from(line_count)
                .unwrap_or(u16::MAX)
                .saturating_sub(visible);
            frame.render_widget(paragraph.scroll((offset, 0)), parts[1]);
        } else {
            let description = self.description();
            let preferred = if matches!(
                self.screen,
                Screen::Confirm | Screen::Limits | Screen::Calls | Screen::Plan(PlanPage::Step)
            ) {
                12
            } else {
                4
            };
            let height = if description.is_empty() {
                0
            } else {
                preferred.min(rows[1].height.saturating_sub(5))
            };
            let parts =
                Layout::vertical([Constraint::Length(height), Constraint::Min(3)]).split(rows[1]);
            self.scroll_page = parts[0].height.max(1);
            let paragraph = Paragraph::new(safe_text(&description)).wrap(Wrap { trim: false });
            let max_scroll = u16::try_from(paragraph.line_count(parts[0].width))
                .unwrap_or(u16::MAX)
                .saturating_sub(parts[0].height);
            self.scroll = self.scroll.min(max_scroll);
            frame.render_widget(paragraph.scroll((self.scroll, 0)), parts[0]);
            self.draw_choices(frame, parts[1]);
        }
        let paused = self.screen == Screen::Detail
            && self.task["finished"] != true
            && (self.failures >= 3 || Instant::now() >= self.refresh_deadline);
        let message = if paused {
            self.text(
                "Automatic refresh paused. Press r to retry, or reopen Tasks & results.",
                "自動更新を停止しました。r で再試行、またはタスク一覧から開き直せます。",
            )
        } else {
            &self.message
        };
        frame.render_widget(
            Paragraph::new(safe_text(message))
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: false }),
            rows[2],
        );
        let help = if self.is_input() {
            self.text(
                "Enter next · Alt+Enter newline · Ctrl+U clear · Esc back · Ctrl+C close",
                "Enter 次へ · Alt+Enter 改行 · Ctrl+U 消去 · Esc 戻る · Ctrl+C 閉じる",
            )
        } else if self.screen == Screen::Detail {
            self.text(
                "↑↓ scroll · Tab action · Enter select · r refresh · Esc home · q close",
                "↑↓ スクロール · Tab 操作選択 · Enter 決定 · r 更新 · Esc ホーム · q 閉じる",
            )
        } else {
            self.text(
                "↑↓ / Tab choose · Enter select · PgUp/PgDn scroll · Esc back · q close",
                "↑↓ / Tab 選択 · Enter 決定 · PgUp/PgDn スクロール · Esc 戻る · q 閉じる",
            )
        };
        frame.render_widget(
            Paragraph::new(help)
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: false }),
            rows[3],
        );
    }

    fn draw_choices(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let choices = self.choices();
        if choices.is_empty() {
            frame.render_widget(
                Paragraph::new(self.text("Nothing here yet.", "まだ項目がありません。")),
                area,
            );
            return;
        }
        self.selected = self.selected.min(choices.len() - 1);
        let items: Vec<_> = choices
            .iter()
            .map(|choice| ListItem::new(safe_text(choice)))
            .collect();
        let list = List::new(items).highlight_symbol("› ").highlight_style(
            Style::default()
                .bg(Color::Rgb(27, 58, 45))
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(
            list,
            area,
            &mut ListState::default().with_selected(Some(self.selected)),
        );
    }

    fn draw_detail(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let parts = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);
        let header = format!(
            "[{}] {}\n{}",
            self.status(string(&self.task, "status")),
            string(&self.task, "id"),
            string(&self.task["job"]["request"], "goal")
        );
        frame.render_widget(
            Paragraph::new(safe_text(&header))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(ACCENT)),
            parts[0],
        );
        let mut result = String::new();
        if let Some(nodes) = self.task["nodes"].as_object() {
            for node in nodes.values() {
                let hint = match string(node, "failure_class") {
                    "provider_authentication" => self.text("Sign in to this AI tool again, then submit a follow-up.", "このAIツールにログインし直してから、続けて依頼してください。"),
                    "provider_rate_limit" => self.text("Check this tool's usage limit, or continue with another model.", "ツール側の利用上限を確認するか、別のモデルへ引き継いでください。"),
                    "provider_timeout" | "budget" => self.text("Review saved output and changes. Split the task or increase its limit before continuing.", "保存された結果と変更内容を確認し、依頼を小さくするか上限を増やして続けてください。"),
                    "provider_unavailable" | "provider_profile_not_found" | "provider_configuration" | "provider_process" => self.text("Check the tool from AI tools & setup. Execution details are below.", "AIツール・接続状況から設定を確認してください。実行の詳細は以下に表示しています。"),
                    _ => "",
                };
                if !hint.is_empty() {
                    let _ = writeln!(result, "{hint}\n");
                }
            }
        }
        result.push_str(&jobs::format_task(&self.task));
        if self.task["finished"] != true {
            result.push_str(self.text(
                "\nWorking… You can close this screen and return from Tasks & results.\n",
                "\n実行中… 画面を閉じても、タスク・結果から戻れます。\n",
            ));
        }
        let paragraph = Paragraph::new(safe_text(&result))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .title(self.text(" Output & saved artifacts ", " 出力・保存先 ")),
            );
        let max_scroll = u16::try_from(paragraph.line_count(parts[1].width))
            .unwrap_or(u16::MAX)
            .saturating_sub(parts[1].height.saturating_sub(1));
        self.scroll = self.scroll.min(max_scroll);
        self.scroll_page = parts[1].height.saturating_sub(1).max(1);
        frame.render_widget(paragraph.scroll((self.scroll, 0)), parts[1]);
        self.draw_choices(frame, parts[2]);
    }
}

enum Action {
    Continue,
    Quit,
    Advanced,
}

enum SessionExit {
    Quit,
    Manual(Option<PathBuf>),
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub async fn launch(repo: PathBuf, trusted: bool, language: Language) -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "The TUI needs a terminal. Use `gloop start TASK --profile TOOL --json` from scripts, or `gloop ui` for the optional GUI."
    );
    let repo = std::fs::canonicalize(repo).context("open project directory")?;
    loop {
        match terminal_session(repo.clone(), trusted, language).await? {
            SessionExit::Quit => break,
            SessionExit::Manual(path) => {
                crate::tui::launch_manual(repo.clone(), trusted, language, path).await?;
            }
        }
    }
    Ok(())
}

async fn terminal_session(repo: PathBuf, trusted: bool, lang: Language) -> Result<SessionExit> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(repo, trusted, lang, tx);
    app.setup();
    loop {
        while let Ok(message) = rx.try_recv() {
            app.receive(message);
        }
        if app.screen == Screen::Detail
            && app.task["finished"] != true
            && app.failures < 3
            && Instant::now() < app.refresh_deadline
            && Instant::now() >= app.refresh_at
        {
            app.refresh();
        }
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key) => match app.key(key) {
                    Ok(Action::Quit) => return Ok(SessionExit::Quit),
                    Ok(Action::Advanced) => return Ok(SessionExit::Manual(app.manual_path.take())),
                    Ok(Action::Continue) => {}
                    Err(error) => app.message = format!("{error:#}"),
                },
                Event::Paste(text) if app.is_input() => {
                    app.insert(&text);
                }
                _ => {}
            }
        }
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn app() -> App {
        let (tx, _) = mpsc::unbounded_channel();
        let mut app = App::new(PathBuf::from("/project"), false, Language::Ja, tx);
        app.profiles = vec![
            json!({"name":"codex", "kind":"cli", "enabled":true, "available":true, "models":[{"id":"small-model", "label":"Small model"}]}),
            json!({"name":"claude", "kind":"cli", "enabled":true, "available":true, "models":[{"id":"haiku", "label":"Haiku"}]}),
            json!({"name":"missing", "kind":"cli", "enabled":true, "available":false}),
        ];
        app
    }

    fn key(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..height {
            let mut x = 0;
            while x < width {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x += u16::try_from(unicode_width::UnicodeWidthStr::width(symbol))
                    .unwrap()
                    .max(1);
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn beginner_keyboard_flow_reaches_reviewable_request_without_invoking_ai() {
        let mut app = app();
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down); // Direct task follows Manual and Auto.
        key(&mut app, KeyCode::Enter);
        app.insert("変更を調べて\n根拠を示してください");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.available().len(), 2);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Review);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        assert_eq!(app.draft.profile.as_deref(), Some("codex"));
        assert_eq!(app.draft.model.as_deref(), Some("small-model"));
        assert_eq!(app.draft.review_profile.as_deref(), Some("claude"));
        assert_eq!(app.draft.review_model.as_deref(), Some("haiku"));
        assert_eq!(app.draft.max_calls, 2);
        assert!(!app.pending);
        assert!(app.submission.is_none());
        let screen = render(&mut app, 90, 28);
        for required in [
            "この内容で開始",
            "small-model",
            "haiku",
            "制限時間",
            "呼び出し上限",
            "/project",
        ] {
            assert!(screen.contains(required), "missing {required}");
        }
    }

    fn planned_task() -> Value {
        json!({"id":"plan-test", "finished":true, "status":"completed", "job":{"request":{"kind":"planning", "goal":"Translate and check", "profile":"codex", "model":"small-model", "max_calls":1}}, "nodes":{"plan":{"status":"succeeded", "output":{"title":"Translate and verify", "steps":[
            {"id":"translate", "title":"Translate", "instructions":"Translate Hello into Japanese", "completion_criteria":"Japanese translation returned", "owned_files":[], "depends_on":[]},
            {"id":"check", "title":"Check", "instructions":"Check the translation", "completion_criteria":"A correctness verdict is returned", "owned_files":[], "depends_on":["translate"]}
        ]}}}})
    }

    #[test]
    fn default_home_opens_manual_graph_authoring_without_submitting_a_task() {
        let mut app = app();
        assert!(app.choices()[0].contains("Manual"));
        assert!(app.enter().unwrap());
        assert!(app.submission.is_none());
        assert!(!app.pending);
        assert!(app.plan.is_none());
    }

    #[test]
    fn saved_graph_opens_directly_from_home_without_a_task_wizard() {
        let repo = tempfile::tempdir().unwrap();
        let path = templates::graph_path(repo.path(), "daily");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let graph = Graph::new(
            "daily",
            "My daily workflow",
            vec![gloop_core::Node::command("check", vec!["true".into()])],
        );
        std::fs::write(&path, graph.to_yaml().unwrap()).unwrap();
        let (tx, _) = mpsc::unbounded_channel();
        let mut app = App::new(repo.path().to_owned(), false, Language::Ja, tx);
        assert!(app.choices()[0].contains("daily"));
        assert!(app.enter().unwrap());
        assert_eq!(app.manual_path.as_ref(), Some(&path));
        assert!(app.submission.is_none());
        assert_eq!(app.workflows[0].1, graph);
        app.selected = app.recent_count();
        assert!(app.enter().unwrap());
        assert!(app.manual_path.is_none());
    }

    #[test]
    fn default_flow_proposes_a_plan_and_stops_before_execution() {
        let mut app = app();
        key(&mut app, KeyCode::Down); // Auto explicitly requests AI planning.
        key(&mut app, KeyCode::Enter);
        app.insert("Translate and check");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        assert_eq!(app.draft.kind, jobs::TaskKind::Planning);
        assert_eq!(app.draft.max_calls, 1);
        assert!(render(&mut app, 100, 28).contains("手順案を作る"));
        assert!(!app.pending);
        app.task = json!({"id":"plan-test"});
        app.go(Screen::Detail);
        app.receive(Message::Detail("plan-test".into(), Ok(planned_task())));
        assert_eq!(app.screen, Screen::Plan(PlanPage::Overview));
        assert!(!app.pending);
        assert!(app.submission.is_none());
        let screen = render(&mut app, 100, 28);
        assert!(screen.contains("Translate"));
        assert!(screen.contains("実作業はまだ開始していません"));
    }

    #[test]
    fn plan_editor_changes_one_models_binding_before_confirming_execution() {
        let mut app = app();
        app.task = planned_task();
        app.open_plan().unwrap();
        app.selected = 1;
        key(&mut app, KeyCode::Enter);
        app.selected = 3;
        key(&mut app, KeyCode::Enter); // Change this step's tool.
        app.selected = 1;
        key(&mut app, KeyCode::Enter); // Claude.
        app.selected = 1;
        key(&mut app, KeyCode::Enter); // Haiku.
        assert_eq!(app.plan.as_ref().unwrap().bindings[0].profile, "codex");
        assert_eq!(app.plan.as_ref().unwrap().bindings[1].profile, "claude");
        assert_eq!(
            app.plan.as_ref().unwrap().bindings[1].model.as_deref(),
            Some("haiku")
        );
        key(&mut app, KeyCode::Esc);
        app.selected = 2;
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        assert!(!app.pending);
        assert_eq!(app.draft.kind, jobs::TaskKind::Task);
        assert_eq!(app.draft.max_calls, 2);
        let graph = app.draft.graph.as_ref().unwrap();
        assert_eq!(graph.spec.nodes[1].model(), Some("haiku"));
        assert_eq!(
            graph.spec.edges,
            vec![gloop_core::Edge::data("translate", "check")]
        );
    }

    #[test]
    fn plan_editor_rejects_dependency_cycles_and_saves_without_overwrite() {
        let repo = tempfile::tempdir().unwrap();
        let mut app = app();
        app.repo = repo.path().to_owned();
        app.task = planned_task();
        app.open_plan().unwrap();
        app.go(Screen::Plan(PlanPage::Dependencies));
        assert!(app.enter_plan(PlanPage::Dependencies).is_err());
        assert!(
            app.plan.as_ref().unwrap().plan.steps[0]
                .depends_on
                .is_empty()
        );
        app.go(Screen::Plan(PlanPage::Save));
        app.input = "my-workflow".to_owned();
        app.cursor = app.input.len();
        app.enter_plan(PlanPage::Save).unwrap();
        let path = repo.path().join(".gloop/graphs/my-workflow.yaml");
        let before = std::fs::read(&path).unwrap();
        assert_eq!(Graph::from_path(&path).unwrap().spec.nodes.len(), 2);
        app.go(Screen::Plan(PlanPage::Save));
        assert!(app.enter_plan(PlanPage::Save).is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        app.go(Screen::Home);
        app.selected = 4;
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Workflows);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Confirm);
        assert_eq!(app.draft.max_calls, 2);
        app.draft.max_calls = 10;
        assert!(app.execution_description().contains("AI呼び出し上限: 2"));
        assert!(!app.pending);
    }

    #[test]
    fn small_terminal_keeps_long_step_descriptions_reachable() {
        let mut app = app();
        app.task = planned_task();
        app.open_plan().unwrap();
        app.plan.as_mut().unwrap().plan.steps[0].instructions = (0..30)
            .map(|index| format!("DETAIL_{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.go(Screen::Plan(PlanPage::Step));
        let mut seen = String::new();
        for _ in 0..12 {
            seen.push_str(&render(&mut app, 80, 24));
            key(&mut app, KeyCode::PageDown);
        }
        for index in 0..30 {
            assert!(seen.contains(&format!("DETAIL_{index:02}")));
        }
        key(&mut app, KeyCode::Esc);
        app.selected = 2;
        key(&mut app, KeyCode::Enter);
        assert!(render(&mut app, 80, 24).contains("AI呼び出し上限: 2"));
        assert!(!app.pending);
    }

    #[test]
    fn editing_and_paste_preserve_unicode_and_remove_terminal_controls() {
        let mut app = app();
        app.start_draft(None);
        app.insert("日本語");
        key(&mut app, KeyCode::Left);
        key(&mut app, KeyCode::Backspace);
        app.insert("本当に");
        assert_eq!(app.input, "日本当に語");
        key(&mut app, KeyCode::Delete);
        app.insert("\r\n確認\u{1b}\u{7}");
        assert_eq!(app.input, "日本当に\n確認");
        app.insert(&"あ".repeat(20000));
        assert!(app.input.len() <= 32 * 1024);
        assert!(app.input.is_char_boundary(app.cursor));
        render(&mut app, 80, 24);
    }

    #[test]
    fn followup_has_previous_id_and_new_explicit_model_selection() {
        let mut app = app();
        app.task = json!({"id":"first", "finished":true});
        app.go(Screen::Detail);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Goal);
        assert_eq!(app.draft.after.as_deref(), Some("first"));
        assert_eq!(app.draft.profile, None);
        assert!(app.draft.goal.is_empty());
    }

    #[test]
    fn late_result_does_not_replace_selected_task_and_refresh_stops_after_three_errors() {
        let mut app = app();
        app.task = json!({"id":"second", "finished":false});
        app.go(Screen::Detail);
        app.receive(Message::Detail("first".into(), Ok(json!({"id":"first"}))));
        assert_eq!(app.task["id"], "second");
        for _ in 0..3 {
            app.receive(Message::Detail(
                "second".into(),
                Err(anyhow::anyhow!("read failed")),
            ));
        }
        assert_eq!(app.failures, 3);
        assert!(render(&mut app, 90, 28).contains("自動更新を停止"));
    }

    #[test]
    fn completed_result_keeps_output_identity_artifacts_and_followup_visible() {
        let mut app = app();
        app.task = json!({"id":"done", "finished":true, "status":"completed", "job":{"request":{"goal":"確認"}}, "run_dir":"/project/.gloop/runs/done", "nodes":{"work":{"status":"succeeded", "profile":"claude", "model":"claude-haiku-4-5", "output":"HANDOFF_OK"}}});
        app.go(Screen::Detail);
        let screen = render(&mut app, 100, 30);
        for required in [
            "HANDOFF_OK",
            "claude-haiku-4-5",
            "/project/.gloop/runs/done",
            "結果を渡して、次の依頼へ",
        ] {
            assert!(screen.contains(required), "missing {required}");
        }
    }
}
