use anyhow::{bail, Result};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: lantern daemon | lantern open <package>";

fn main() -> std::process::ExitCode {
    let filter = EnvFilter::try_from_env("LANTERN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    match cli() {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::from(2)
        }
    }
}

fn cli() -> Result<u8> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["daemon"] => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(lantern::daemon::run())?;
            Ok(0)
        }
        // The window owns the main thread (winit); it runs no tokio runtime. Its failures
        // (daemon unreachable, event loop) exit 1 like its runtime errors; 2 is usage only.
        ["open", pkg] if !pkg.is_empty() && !pkg.starts_with('-') => {
            match lantern::window::run(pkg) {
                Ok(code) => Ok(code.clamp(0, 255) as u8),
                Err(error) => {
                    eprintln!("lantern: {error:#}");
                    Ok(1)
                }
            }
        }
        ["daemon", ..] | ["open", ..] => bail!("{USAGE}"),
        [other, ..] => bail!("unknown subcommand {other:?}; {USAGE}"),
        [] => bail!("{USAGE}"),
    }
}
