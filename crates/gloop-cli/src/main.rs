mod atomic_write;
mod cli;
mod commands;
mod templates;
mod wizard;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
