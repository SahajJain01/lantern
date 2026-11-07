use anyhow::{bail, Result};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let filter = EnvFilter::try_from_env("LANTERN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    match cli().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::from(2)
        }
    }
}

async fn cli() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("daemon") if args.next().is_none() => lantern::daemon::run().await,
        Some("daemon") => bail!("usage: lantern daemon"),
        Some(other) => bail!("unknown subcommand {other:?}; usage: lantern daemon"),
        None => bail!("usage: lantern daemon"),
    }
}
