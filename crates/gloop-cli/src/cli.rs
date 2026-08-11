use anyhow::Result;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::commands::{
    RenderFormat, graph_explain, graph_init, graph_new, graph_render, graph_schema,
    graph_validate, inspect_run_at, present, provider_add, provider_doctor, provider_list,
    provider_probe, replay_run, run_foreground, run_logs,
};

#[derive(Parser)]
#[command(name = "gloop", version = env!("CARGO_PKG_VERSION"), about = "Gloop CLI")]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Load project provider profiles from <repo>/.gloop/profiles.toml"
    )]
    trust_project_profiles: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a graph or create a one-node graph from a goal.
    Run(RunCommand),
    /// Create, validate, explain, render, or inspect the graph schema.
    #[command(subcommand)]
    Graph(GraphCommand),
    /// Configure and diagnose model/harness profiles.
    #[command(subcommand)]
    Provider(ProviderCommand),
    /// Inspect a completed run directory.
    Inspect(InspectCommand),
    /// Print journal events from a run directory.
    Logs(LogsCommand),
    /// Replay scheduler state from a run journal.
    Replay(ReplayCommand),
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

    #[arg(long = "repo", value_name = "PATH", default_value = ".")]
    repo: PathBuf,

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
}

#[derive(Subcommand)]
enum GraphCommand {
    New(GraphNew),
    Init(GraphInit),
    Validate(GraphFile),
    Explain(GraphFile),
    Render(GraphRender),
    Schema(GraphSchema),
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

    #[arg(
        long,
        default_value = "direct",
        conflicts_with = "interactive"
    )]
    template: String,

    #[arg(long = "repo", value_name = "PATH", default_value = ".")]
    repo: PathBuf,

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
struct GraphInit {
    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
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
    list: bool,

    #[arg(long)]
    force: bool,

    #[arg(long = "repo", value_name = "PATH", default_value = ".")]
    repo: PathBuf,

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
    List(ProviderVerbose),
    Probe(ProviderProbe),
    Add(ProviderAdd),
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
struct InspectCommand {
    #[arg(default_value = ".", value_name = "PATH")]
    path: PathBuf,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct LogsCommand {
    #[arg(default_value = ".", value_name = "PATH")]
    path: PathBuf,

    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ReplayCommand {
    #[arg(default_value = ".", value_name = "PATH")]
    path: PathBuf,

    #[arg(long)]
    json: bool,
}

impl Command {
    fn json_mode(&self) -> bool {
        match self {
            Self::Run(c) => c.json,
            Self::Graph(GraphCommand::New(c)) => c.json,
            Self::Graph(GraphCommand::Init(c)) => c.json,
            Self::Graph(GraphCommand::Validate(c) | GraphCommand::Explain(c)) => c.json,
            Self::Graph(GraphCommand::Render(c)) => c.json,
            Self::Graph(GraphCommand::Schema(c)) => c.json,
            Self::Provider(ProviderCommand::List(c) | ProviderCommand::Doctor(c)) => c.json,
            Self::Provider(ProviderCommand::Probe(c)) => c.json,
            Self::Provider(ProviderCommand::Add(c)) => c.json,
            Self::Inspect(c) => c.json,
            Self::Logs(c) => c.json,
            Self::Replay(c) => c.json,
        }
    }
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let json_mode = cli.command.json_mode();

    let result = match cli.command {
        Command::Run(cmd) => {
            run_foreground(
                cmd.goal,
                cmd.graph,
                cmd.profile,
                cmd.model,
                cmd.repo,
                cmd.json,
                cmd.dry_run,
                cmd.non_interactive,
                cmd.max_parallel,
                cli.trust_project_profiles,
                cmd.interactive,
            )
            .await
        }
        Command::Graph(cmd) => match cmd {
            GraphCommand::New(c) => {
                graph_new(
                    c.name,
                    c.goal,
                    c.template,
                    c.repo,
                    c.request,
                    c.provider_profiles,
                    c.loop_cap,
                    c.interactive,
                    c.path,
                    c.force,
                    c.json,
                )
                .await
            }
            GraphCommand::Init(c) => {
                graph_init(
                    c.name,
                    c.from,
                    c.description,
                    c.request,
                    c.provider_profiles,
                    c.loop_cap,
                    c.list,
                    c.force,
                    c.repo,
                    c.json,
                )
                .await
            }
            GraphCommand::Validate(c) => graph_validate(c.path, c.json).await,
            GraphCommand::Explain(c) => graph_explain(c.path, c.json).await,
            GraphCommand::Render(c) => graph_render(c.path, c.format, c.json).await,
            GraphCommand::Schema(c) => graph_schema(c.json),
        },
        Command::Provider(cmd) => match cmd {
            ProviderCommand::List(c) => provider_list(c.json, cli.trust_project_profiles).await,
            ProviderCommand::Probe(c) => {
                provider_probe(c.profile, c.json, cli.trust_project_profiles).await
            }
            ProviderCommand::Add(c) => {
                provider_add(c.profile, c.definition, c.json, cli.trust_project_profiles).await
            }
            ProviderCommand::Doctor(c) => provider_doctor(c.json, cli.trust_project_profiles).await,
        },
        Command::Inspect(c) => inspect_run_at(c.path, c.json).await,
        Command::Logs(c) => run_logs(c.path, c.json).await,
        Command::Replay(c) => replay_run(c.path, c.json).await,
    };

    let code = present(result, json_mode)?;
    std::process::exit(code);
}
