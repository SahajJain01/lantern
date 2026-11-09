use anyhow::{bail, Result};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: lantern daemon | lantern open [--user <android-user>] <package> \
     | lantern status | lantern undo-setup [--serial <serial>]";

/// Exit statuses: 0 ok, 1 runtime failure, 2 usage error, 3 (`daemon` only) helper protocol
/// mismatch, which `lanternd.service` does not restart.
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

/// A subcommand's failure is printed and becomes `code(error)`. Only usage errors reach
/// `main`'s exit 2.
fn runtime(result: Result<u8>, code: impl FnOnce(&anyhow::Error) -> u8) -> u8 {
    match result {
        Ok(ok) => ok,
        Err(error) => {
            eprintln!("lantern: {error:#}");
            code(&error)
        }
    }
}

fn cli() -> Result<u8> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["daemon"] => {
            let run = || -> Result<u8> {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(lantern::daemon::run())?;
                Ok(0)
            };
            Ok(runtime(run(), lantern::daemon::exit_code))
        }
        // The window owns the main thread (winit); it runs no tokio runtime. Its failures
        // (daemon unreachable, event loop) exit 1 like its runtime errors.
        ["open", rest @ ..] => {
            let id = lantern::window::identity::Identity::parse_args(rest)
                .map_err(|e| anyhow::anyhow!("lantern open: {e}; {USAGE}"))?;
            let run = lantern::window::run(&id).map(|code| code.clamp(0, 255) as u8);
            Ok(runtime(run, |_| 1))
        }
        ["status"] => {
            let run = || -> Result<u8> {
                use lantern::daemon::ipc;
                let report = ipc::query_status(&ipc::socket_path_in(&ipc::runtime_dir()?))?;
                println!("{}", serde_json::to_string(&report)?);
                Ok(0)
            };
            Ok(runtime(run(), |_| 1))
        }
        ["undo-setup", rest @ ..] => {
            let explicit = match rest {
                [] => None,
                ["--serial", serial] => Some(*serial),
                [flag] if flag.starts_with("--serial=") => Some(&flag["--serial=".len()..]),
                _ => bail!("{USAGE}"),
            };
            if explicit == Some("") {
                bail!("--serial needs a serial; {USAGE}");
            }
            Ok(runtime(undo_setup(explicit), |_| 1))
        }
        ["daemon", ..] | ["status", ..] => bail!("{USAGE}"),
        [other, ..] => bail!("unknown subcommand {other:?}; {USAGE}"),
        [] => bail!("{USAGE}"),
    }
}

/// `lantern undo-setup`: exit 0 only if every step succeeded.
fn undo_setup(explicit: Option<&str>) -> Result<u8> {
    use anyhow::Context;
    use lantern::daemon::{companion, ipc};
    let env = std::env::var("LANTERN_SERIAL").ok();
    let serial = lantern::undo::resolve_serial(explicit, env.as_deref(), || {
        let output = std::process::Command::new("adb")
            .arg("devices")
            .stdin(std::process::Stdio::null())
            .output()
            .context("run adb devices")?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })?;
    let sock = ipc::socket_path_in(&ipc::runtime_dir()?);
    let state_dir = companion::state_dir().context("no home directory for the state dir")?;
    let rt = tokio::runtime::Runtime::new()?;
    let adb = lantern::adb::Adb::new(serial);
    let ok = rt.block_on(lantern::undo::run(
        &adb,
        &sock,
        &state_dir,
        &mut std::io::stdout(),
    ))?;
    Ok(if ok { 0 } else { 1 })
}
