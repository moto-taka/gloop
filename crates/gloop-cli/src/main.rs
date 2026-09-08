mod atomic_write;
mod cli;
mod commands;
mod gui;
pub(crate) mod i18n;
mod jobs;
mod planning;
mod task_tui;
mod templates;
mod tui;
mod wizard;
mod workspace;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
