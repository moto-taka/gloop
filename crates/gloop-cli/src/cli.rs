use anyhow::Result;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::commands::{
    CommandResult, RenderFormat, graph_edit, graph_explain, graph_init, graph_list, graph_new,
    graph_render, graph_schema, graph_validate, inspect_run_at, present, provider_add,
    provider_doctor, provider_list, provider_probe, replay_run, run_foreground, run_logs,
    run_status,
};
use crate::i18n::Language;
use crate::tui;

#[derive(Parser)]
#[command(
    name = "gloop",
    version = env!("CARGO_PKG_VERSION"),
    about = "Build and run AI workflows",
    after_help = "Start here:\n  gloop                    Open the workspace\n  gloop --lang ja           Open in Japanese\n  gloop run --graph FILE    Run a saved workflow\n\nUse 'gloop <command> --help' for details."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        default_value = ".",
        value_name = "PATH",
        help = "Project directory"
    )]
    repo: PathBuf,

    #[arg(
        long,
        alias = "language",
        global = true,
        value_enum,
        help = "Display language (default: system locale)"
    )]
    lang: Option<Language>,

    #[arg(
        long,
        global = true,
        help = "Load project provider profiles from <repo>/.gloop/profiles.toml"
    )]
    trust_project_profiles: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the workspace (same as plain gloop).
    #[command(hide = true)]
    Tui,
    /// Run a workflow or a single AI task in this terminal.
    Run(RunCommand),
    /// Start a task in the background.
    Start(StartCommand),
    /// List background tasks or read a task's result.
    Tasks(TasksCommand),
    /// Stop a background task.
    Stop(StopCommand),
    #[command(hide = true)]
    TaskWorker(StopCommand),
    /// Create, edit, and validate workflow files.
    Graph(GraphCommand),
    /// Configure AI tools and check their availability.
    #[command(subcommand)]
    Provider(ProviderCommand),
    /// Open the workspace in a browser.
    Ui(UiCommand),
    /// Inspect run status, output, and execution records.
    #[command(subcommand)]
    Debug(DebugCommand),
    // Published spellings remain accepted for scripts; dispatch is shared with debug.
    #[command(hide = true)]
    Status(StatusCommand),
    #[command(hide = true)]
    Inspect(RunDirectory),
    #[command(hide = true)]
    Logs(RunDirectory),
    #[command(hide = true)]
    Replay(RunDirectory),
}

#[derive(Subcommand)]
enum DebugCommand {
    /// Show live status; omit `RUN_ID` for the newest run.
    Status(StatusCommand),
    /// Read a completed run's result and node outputs.
    Inspect(RunDirectory),
    /// Read the execution journal.
    Logs(RunDirectory),
    /// Reconstruct scheduler state from the journal without running models.
    Replay(RunDirectory),
}

#[derive(Args, Default)]
struct UiCommand {
    #[arg(long, help = "Print the local URL without opening a browser")]
    no_open: bool,
}

#[derive(Args)]
struct StartCommand {
    #[arg(long, conflicts_with_all = ["graph", "review_profile", "after", "max_calls"], help = "Create a reviewable work plan with one AI call; do not execute the plan")]
    plan: bool,
    #[arg(required_unless_present = "graph", conflicts_with = "graph")]
    goal: Option<String>,
    #[arg(long, required_unless_present = "graph", conflicts_with = "graph")]
    profile: Option<String>,
    #[arg(long, conflicts_with = "graph")]
    model: Option<String>,
    #[arg(long, conflicts_with = "graph")]
    review_profile: Option<String>,
    #[arg(long, requires = "review_profile")]
    review_model: Option<String>,
    #[arg(
        long,
        conflicts_with = "graph",
        help = "Pass a finished task's bounded result to this new task"
    )]
    after: Option<String>,
    #[arg(long)]
    graph: Option<PathBuf>,
    #[arg(long, default_value_t = crate::jobs::default_timeout())]
    timeout: u64,
    #[arg(long, default_value_t = crate::jobs::default_calls())]
    max_calls: u32,
    #[arg(long, help = "Stable id for idempotent submission")]
    id: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TasksCommand {
    id: Option<String>,
    #[arg(long, requires = "id")]
    wait: bool,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StopCommand {
    id: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
struct RunCommand {
    #[arg(
        help = "Goal statement for inline run",
        conflicts_with_all = ["graph", "interactive"]
    )]
    goal: Option<String>,

    #[arg(
        long,
        short = 'g',
        value_name = "PATH",
        conflicts_with = "goal",
        conflicts_with_all = ["profile", "model", "interactive"],
    )]
    graph: Option<PathBuf>,

    #[arg(
        long,
        help = "Provider/harness profile for an inline goal",
        conflicts_with = "graph"
    )]
    profile: Option<String>,

    #[arg(
        long,
        help = "Model id or alias for an inline goal",
        conflicts_with = "graph"
    )]
    model: Option<String>,

    #[arg(long)]
    json: bool,

    #[arg(long = "dry-run")]
    dry_run: bool,

    #[arg(long = "non-interactive", conflicts_with = "interactive")]
    non_interactive: bool,

    #[arg(
        long = "interactive",
        conflicts_with_all = ["graph", "non_interactive"]
    )]
    interactive: bool,

    #[arg(long = "max-parallel")]
    max_parallel: Option<usize>,

    #[arg(
        long = "run-id",
        value_name = "ID",
        help = "Run id to use instead of a fresh ULID; inspect with 'gloop debug status <id>'"
    )]
    run_id: Option<String>,
}

#[derive(Args)]
struct GraphCommand {
    /// Start the resident graph TUI when no graph subcommand is supplied.
    #[command(subcommand)]
    command: Option<GraphSubcommand>,
}

#[derive(Subcommand)]
enum GraphSubcommand {
    /// Open the advanced graph editor.
    #[command(hide = true)]
    Tui,
    /// Show built-in templates, saved templates, and graph YAML files in this project.
    List(GraphList),
    /// Create a graph file from a template or interactively.
    New(GraphNew),
    /// Create a reusable project template.
    Init(GraphInit),
    /// Edit a graph file, saved template, or built-in template.
    Edit(GraphEdit),
    /// Edit a saved project template by name (prefer graph edit).
    #[command(hide = true)]
    Update(GraphEdit),
    /// Check one graph file.
    Validate(GraphFile),
    /// Explain the execution order of one graph file.
    Explain(GraphFile),
    /// Render one graph as Mermaid or DOT.
    Render(GraphRender),
    /// Print the machine-readable graph schema.
    Schema(GraphSchema),
}

#[derive(Args)]
struct GraphList {
    #[arg(long, help = "Print one JSON object for scripts and assistants")]
    json: bool,
}

#[derive(Args)]
struct GraphFile {
    #[arg(value_name = "PATH")]
    path: PathBuf,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GraphNew {
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    #[arg(long, default_value = "run")]
    name: String,

    #[arg(long, default_value = "work")]
    goal: String,

    #[arg(long, default_value = "direct", conflicts_with = "interactive")]
    template: String,

    #[arg(long, conflicts_with = "interactive")]
    request: Option<String>,

    #[arg(long, conflicts_with = "interactive")]
    provider_profiles: Option<String>,

    #[arg(long = "loop-cap", conflicts_with = "interactive")]
    loop_cap: Option<u32>,

    #[arg(long)]
    force: bool,

    #[arg(long)]
    interactive: bool,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
struct GraphInit {
    #[arg(long, help = "Reusable name for the new project template")]
    name: Option<String>,

    #[arg(
        long,
        help = "Built-in template to start from, such as direct or plan-implement-verify"
    )]
    from: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long, conflicts_with = "list")]
    request: Option<String>,

    #[arg(long, conflicts_with = "list")]
    provider_profiles: Option<String>,

    #[arg(long = "loop-cap", conflicts_with = "list")]
    loop_cap: Option<u32>,

    #[arg(long, conflicts_with_all = ["name", "from", "description", "request", "provider_profiles", "loop_cap"])]
    #[arg(hide = true)]
    list: bool,

    #[arg(
        long,
        conflicts_with = "list",
        help = "Open the browser editor instead of asking in the terminal"
    )]
    gui: bool,

    #[arg(long)]
    force: bool,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GraphEdit {
    #[arg(
        value_name = "PATH_OR_NAME",
        help = "Graph YAML path or a name shown by 'gloop graph list'"
    )]
    target: PathBuf,

    #[arg(long, help = "Open the local browser editor")]
    gui: bool,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GraphRender {
    #[arg(value_name = "PATH")]
    path: PathBuf,

    #[arg(value_enum, long, default_value = "mermaid")]
    format: RenderFormat,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GraphSchema {
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum ProviderCommand {
    /// List configured AI tools.
    List(ProviderVerbose),
    /// Check one AI tool's availability.
    Probe(ProviderProbe),
    /// Save a provider profile in the project's configuration.
    Add(ProviderAdd),
    /// Check all configured AI tools and report setup problems.
    Doctor(ProviderVerbose),
}

#[derive(Args)]
struct ProviderVerbose {
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ProviderProbe {
    profile: String,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ProviderAdd {
    profile: String,
    definition: String,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StatusCommand {
    #[arg(
        value_name = "RUN_ID",
        help = "Run id under <repo>/.gloop/runs; defaults to the newest run"
    )]
    run_id: Option<String>,

    #[arg(
        long,
        default_value_t = 10,
        help = "How many recent journal events to include"
    )]
    events: usize,

    #[arg(
        long,
        help = "Wait until the run finishes, then exit with the run's status code"
    )]
    wait: bool,

    #[arg(
        long = "interval-ms",
        default_value_t = 1000,
        help = "Polling interval in milliseconds for --wait"
    )]
    interval_ms: u64,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RunDirectory {
    #[arg(
        value_name = "PATH",
        help = "Run directory (default: project directory)"
    )]
    path: Option<PathBuf>,

    #[arg(long)]
    json: bool,
}

impl Command {
    fn json_mode(&self) -> bool {
        match self {
            Self::Tui | Self::Ui(_) => false,
            Self::Start(c) => c.json,
            Self::Tasks(c) => c.json,
            Self::Stop(c) | Self::TaskWorker(c) => c.json,
            Self::Run(c) => c.json,
            Self::Graph(c) => c.command.as_ref().is_some_and(|command| match command {
                GraphSubcommand::Tui => false,
                GraphSubcommand::List(c) => c.json,
                GraphSubcommand::New(c) => c.json,
                GraphSubcommand::Init(c) => c.json,
                GraphSubcommand::Edit(c) | GraphSubcommand::Update(c) => c.json,
                GraphSubcommand::Validate(c) | GraphSubcommand::Explain(c) => c.json,
                GraphSubcommand::Render(c) => c.json,
                GraphSubcommand::Schema(c) => c.json,
            }),
            Self::Provider(ProviderCommand::List(c) | ProviderCommand::Doctor(c)) => c.json,
            Self::Provider(ProviderCommand::Probe(c)) => c.json,
            Self::Provider(ProviderCommand::Add(c)) => c.json,
            Self::Status(c) | Self::Debug(DebugCommand::Status(c)) => c.json,
            Self::Inspect(c)
            | Self::Logs(c)
            | Self::Replay(c)
            | Self::Debug(
                DebugCommand::Inspect(c) | DebugCommand::Logs(c) | DebugCommand::Replay(c),
            ) => c.json,
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_workspace_command(
    command: Command,
    repo: PathBuf,
    lang: Language,
    trusted: bool,
    json_mode: bool,
) -> Result<()> {
    use crate::jobs;
    let mut wait_for_result = false;
    let result: Result<Option<serde_json::Value>> = async {
        Ok(match command {
            Command::Tui => {
                crate::task_tui::launch(repo, trusted, lang).await?;
                None
            }
            Command::Ui(c) => {
                crate::workspace::launch(repo, trusted, lang, c.no_open).await?;
                None
            }
            Command::Start(c) => {
                let graph = c.graph.map(gloop_core::Graph::from_path).transpose()?;
                let goal = c.goal.unwrap_or_else(|| {
                    graph
                        .as_ref()
                        .map_or_else(String::new, |graph| graph.spec.goal.clone())
                });
                Some(
                    jobs::start(
                        &repo,
                        jobs::StartRequest {
                            kind: if c.plan {
                                jobs::TaskKind::Planning
                            } else {
                                jobs::TaskKind::Task
                            },
                            goal,
                            profile: c.profile,
                            model: c.model,
                            review_profile: c.review_profile,
                            review_model: c.review_model,
                            after: c.after,
                            graph,
                            timeout_seconds: c.timeout,
                            max_calls: if c.plan { 1 } else { c.max_calls },
                        },
                        trusted,
                        c.id,
                    )
                    .await?,
                )
            }
            Command::Tasks(c) => {
                wait_for_result = c.wait;
                Some(if let Some(id) = c.id {
                    if c.wait {
                        jobs::wait(&repo, &id, std::time::Duration::from_secs(c.timeout)).await?
                    } else {
                        jobs::detail(&repo, &id).await?
                    }
                } else {
                    jobs::list(&repo)?
                })
            }
            Command::Stop(c) => Some(jobs::cancel(&repo, &c.id).await?),
            Command::TaskWorker(c) => {
                jobs::worker(&repo, &c.id).await?;
                None
            }
            _ => unreachable!("only workspace commands"),
        })
    }
    .await;
    match result {
        Ok(Some(value)) => {
            if json_mode {
                println!("{}", serde_json::to_string(&value)?);
            } else if let Some(tasks) = value["tasks"].as_array() {
                if tasks.is_empty() {
                    println!(
                        "No tasks yet. Open `gloop` or use `gloop start TASK --profile TOOL`."
                    );
                }
                for task in tasks {
                    print!("{}", jobs::format_task(task));
                }
                if let Some(errors) = value["errors"].as_array() {
                    for error in errors {
                        eprintln!("{}: {}", error["id"], error["error"]);
                    }
                }
            } else {
                print!("{}", jobs::format_task(&value));
            }
            if wait_for_result {
                let code =
                    value["exit_code"]
                        .as_i64()
                        .unwrap_or_else(|| match value["status"].as_str() {
                            Some("completed") => 0,
                            Some("blocked") => 2,
                            Some("budget_exhausted") => 5,
                            Some("cancelled") => 130,
                            _ => 3,
                        });
                std::process::exit(i32::try_from(code).unwrap_or(3));
            }
        }
        Ok(None) => {}
        Err(error) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"success": false, "error": format!("{error:#}")})
                );
                std::process::exit(1);
            }
            return Err(error);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let lang = cli.lang.unwrap_or_else(Language::detect);
    let repo = cli.repo;
    let command = cli.command.unwrap_or(Command::Tui);
    let json_mode = command.json_mode();

    if matches!(
        command,
        Command::Tui
            | Command::Ui(_)
            | Command::Start(_)
            | Command::Tasks(_)
            | Command::Stop(_)
            | Command::TaskWorker(_)
    ) {
        return run_workspace_command(command, repo, lang, cli.trust_project_profiles, json_mode)
            .await;
    }

    let result = match command {
        Command::Tui
        | Command::Ui(_)
        | Command::Start(_)
        | Command::Tasks(_)
        | Command::Stop(_)
        | Command::TaskWorker(_) => unreachable!("workspace commands dispatched above"),
        Command::Run(cmd) => {
            run_foreground(
                cmd.goal,
                cmd.graph,
                cmd.profile,
                cmd.model,
                repo,
                cmd.json,
                cmd.dry_run,
                cmd.non_interactive,
                cmd.max_parallel,
                cli.trust_project_profiles,
                cmd.interactive,
                cmd.run_id,
            )
            .await
        }
        Command::Graph(cmd) => match cmd.command {
            None | Some(GraphSubcommand::Tui) => {
                match tui::launch(repo, cli.trust_project_profiles, lang).await {
                    Ok(()) => CommandResult {
                        code: crate::commands::ExitCode::Success,
                        output: None,
                        text: None,
                    },
                    Err(error) => CommandResult::failure_text(
                        crate::commands::ExitCode::Internal,
                        format!("graph TUI failed: {error}"),
                    ),
                }
            }
            Some(GraphSubcommand::List(c)) => graph_list(repo, lang, c.json).await,
            Some(GraphSubcommand::New(c)) => {
                graph_new(
                    c.name,
                    c.goal,
                    c.template,
                    repo,
                    c.request,
                    c.provider_profiles,
                    c.loop_cap,
                    c.interactive,
                    c.path,
                    c.force,
                    c.json,
                    cli.trust_project_profiles,
                )
                .await
            }
            Some(GraphSubcommand::Init(c)) => {
                graph_init(
                    c.name,
                    c.from,
                    c.description,
                    c.request,
                    c.provider_profiles,
                    c.loop_cap,
                    c.list,
                    c.force,
                    repo,
                    c.json,
                    cli.trust_project_profiles,
                    c.gui,
                    lang,
                )
                .await
            }
            Some(GraphSubcommand::Edit(c)) => {
                graph_edit(
                    c.target,
                    repo,
                    c.gui,
                    lang,
                    c.json,
                    false,
                    cli.trust_project_profiles,
                )
                .await
            }
            Some(GraphSubcommand::Update(c)) => {
                graph_edit(
                    c.target,
                    repo,
                    c.gui,
                    lang,
                    c.json,
                    true,
                    cli.trust_project_profiles,
                )
                .await
            }
            Some(GraphSubcommand::Validate(c)) => graph_validate(c.path, c.json).await,
            Some(GraphSubcommand::Explain(c)) => graph_explain(c.path, c.json).await,
            Some(GraphSubcommand::Render(c)) => graph_render(c.path, c.format, c.json).await,
            Some(GraphSubcommand::Schema(c)) => graph_schema(c.json),
        },
        Command::Provider(cmd) => match cmd {
            ProviderCommand::List(c) => {
                provider_list(&repo, c.json, cli.trust_project_profiles).await
            }
            ProviderCommand::Probe(c) => {
                provider_probe(&repo, c.profile, c.json, cli.trust_project_profiles).await
            }
            ProviderCommand::Add(c) => {
                provider_add(
                    &repo,
                    c.profile,
                    c.definition,
                    c.json,
                    cli.trust_project_profiles,
                )
                .await
            }
            ProviderCommand::Doctor(c) => {
                provider_doctor(&repo, c.json, cli.trust_project_profiles).await
            }
        },
        Command::Status(c) | Command::Debug(DebugCommand::Status(c)) => {
            run_status(
                c.run_id,
                repo,
                c.events,
                c.wait,
                c.interval_ms,
                c.json,
                lang,
            )
            .await
        }
        Command::Inspect(c) | Command::Debug(DebugCommand::Inspect(c)) => {
            inspect_run_at(c.path.unwrap_or(repo), c.json).await
        }
        Command::Logs(c) | Command::Debug(DebugCommand::Logs(c)) => {
            run_logs(c.path.unwrap_or(repo), c.json).await
        }
        Command::Replay(c) | Command::Debug(DebugCommand::Replay(c)) => {
            replay_run(c.path.unwrap_or(repo), c.json).await
        }
    };

    let code = present(result, json_mode)?;
    std::process::exit(code);
}
