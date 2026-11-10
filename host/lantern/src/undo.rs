//! `lantern undo-setup`: removes what Lantern put on a phone and what it recorded about it.
//!
//! Steps (one output line each; the run fails if any step fails, but every step runs):
//! 1. `helper --forget`: restores a journal a dead session left behind and deletes it and every
//!    Lantern file in `/data/local/tmp`. `--forget` deletes the jar it runs from, so the
//!    embedded jar is pushed first whenever it is missing.
//! 2. `pm uninstall --user 0 dev.lantern.companion` (not installed is fine).
//! 3. The serial's menu entries and icons (see [`remove_catalogue_entries`]).
//! 4. The device record, only if the phone steps succeeded (it stays true otherwise).

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::adb::{parse_device_list, Adb, ADB_CALL_TIMEOUT};
use crate::daemon::catalog::{self, Catalog, Dirs};
use crate::daemon::{companion, ipc, session};

/// Why undo-setup refuses while lanternd runs.
pub const STOP_FIRST: &str = "stop lanternd first (systemctl --user stop lanternd), or use \
     \"Undo phone setup…\" in Lantern's tray menu";

/// True if `error` (from [`ipc::query_status`]) means no lanternd is listening.
fn daemon_absent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
    })
}

/// Fails with [`STOP_FIRST`] if lanternd answers on `sock` in any state: even idle it may be
/// waiting to retry a session, which would race the undo (reinstall the companion, write a
/// journal, have its helper killed).
pub fn check_daemon(sock: &Path) -> Result<()> {
    match ipc::query_status(sock) {
        Ok(_) => bail!(STOP_FIRST),
        Err(error) if daemon_absent(&error) => Ok(()),
        Err(error) => Err(error.context("could not ask lanternd whether a session is live")),
    }
}

/// The serial to undo: `--serial`, else `$LANTERN_SERIAL`, else the only device in
/// `adb devices` output.
pub fn resolve_serial(
    explicit: Option<&str>,
    env: Option<&str>,
    adb_devices: impl FnOnce() -> Result<String>,
) -> Result<String> {
    if let Some(serial) = explicit.or(env).filter(|s| !s.is_empty()) {
        return Ok(serial.to_owned());
    }
    let list = adb_devices()?;
    let online: Vec<String> = parse_device_list(&list)
        .into_iter()
        .filter(|entry| entry.state == "device")
        .map(|entry| entry.serial)
        .collect();
    match online.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!("no phone connected; pass --serial <serial>"),
        _ => bail!(
            "several phones connected ({}); pass --serial <serial>",
            online.join(", ")
        ),
    }
}

/// Removes `serial`'s menu entries and icons (and the Phone submenu once no Lantern entry is
/// left), then rebuilds the KDE menu cache. Returns how many entries there were.
pub fn remove_catalogue_entries(serial: &str) -> Result<usize> {
    let dirs = Dirs::from_env()?;
    remove_catalogue_entries_with(dirs, serial, std::sync::Arc::new(catalog::run_kbuildsycoca))
}

/// [`remove_catalogue_entries`] under `dirs`, with `rebuild` instead of kbuildsycoca6.
pub fn remove_catalogue_entries_with(
    dirs: Dirs,
    serial: &str,
    rebuild: catalog::Rebuild,
) -> Result<usize> {
    // Dropping the catalogue runs its pending rebuild before returning.
    Catalog::with_rebuild(dirs, rebuild).remove_serial(serial)
}

async fn forget(adb: &Adb) -> Result<String> {
    // A previous session's helper or its Cleanup may still be restoring the phone.
    session::wait_for_previous_helpers(adb, session::PREVIOUS_HELPER_WAIT).await?;
    let (_, remote) = session::deploy_helper(adb)
        .await
        .context("push the helper jar")?;
    let cmd = format!("CLASSPATH={remote} app_process / dev.lantern.helper.Main --forget");
    let out = adb.shell_output(&[&cmd], ADB_CALL_TIMEOUT).await?;
    let text = out.text.trim();
    if !out.success {
        bail!(
            "{}",
            if text.is_empty() {
                "--forget failed"
            } else {
                text
            }
        );
    }
    Ok(text
        .lines()
        .rev()
        .find(|line| line.starts_with("forgot "))
        .unwrap_or(text)
        .to_owned())
}

async fn remove_companion(adb: &Adb) -> Result<&'static str> {
    if companion::installed_path(adb).await?.is_none() {
        return Ok("not installed");
    }
    companion::uninstall(adb).await?;
    Ok("uninstalled")
}

fn first_line(error: &anyhow::Error) -> String {
    format!("{error:#}").replace('\n', " ")
}

/// Runs undo-setup for `adb`'s serial, printing one line per step to `out`. `Err` means it
/// refused (lanternd running) or could not check; otherwise `Ok(all steps ok)`.
pub async fn run(
    adb: &Adb,
    sock: &Path,
    state_dir: &Path,
    out: &mut (dyn Write + Send),
) -> Result<bool> {
    check_daemon(sock)?;
    run_steps(adb, state_dir, &remove_catalogue_entries, out).await
}

/// Steps 1–4 for `adb`'s serial, one line each to `out` (see the module doc); `remove_entries`
/// removes the serial's menu entries. lanternd runs this itself (tray "Undo phone setup…")
/// once its session on the phone is closed. `Err` only if writing to `out` fails; otherwise
/// `Ok(all steps ok)`. (`Sync`/`Send` so lanternd can run it on its multi-threaded runtime.)
pub async fn run_steps(
    adb: &Adb,
    state_dir: &Path,
    remove_entries: &(dyn Fn(&str) -> Result<usize> + Sync),
    out: &mut (dyn Write + Send),
) -> Result<bool> {
    let serial = adb.serial().to_owned();
    let mut phone_ok = true;

    match forget(adb).await {
        Ok(result) => writeln!(out, "helper --forget: {result}")?,
        Err(error) => {
            phone_ok = false;
            writeln!(out, "helper --forget: FAILED: {}", first_line(&error))?;
        }
    }
    match remove_companion(adb).await {
        Ok(result) => writeln!(out, "companion: {result}")?,
        Err(error) => {
            phone_ok = false;
            writeln!(out, "companion: FAILED: {}", first_line(&error))?;
        }
    }
    let mut ok = phone_ok;
    match remove_entries(&serial) {
        Ok(n) => writeln!(out, "catalogue: removed {n} entries")?,
        Err(error) => {
            ok = false;
            writeln!(out, "catalogue: FAILED: {}", first_line(&error))?;
        }
    }
    let record = companion::record_path(state_dir, &serial);
    if !phone_ok {
        writeln!(out, "device record: kept (phone steps failed)")?;
    } else {
        match std::fs::remove_file(&record) {
            Ok(()) => writeln!(out, "device record: removed {}", record.display())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                writeln!(out, "device record: none")?
            }
            Err(error) => {
                ok = false;
                writeln!(
                    out,
                    "device record: FAILED: remove {}: {error}",
                    record.display()
                )?;
            }
        }
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_entries_of_the_serial_are_removed() {
        use crate::daemon::catalog::{AppEntry, Catalog, Dirs};
        let root = std::env::temp_dir().join(format!("lantern-undo-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dirs = Dirs {
            applications: root.join("applications"),
            icons: root.join("icons"),
            menus: root.join("menus"),
            directories: root.join("directories"),
            exe: "/usr/bin/lantern".into(),
        };
        let mut cat = Catalog::with_rebuild(dirs.clone(), std::sync::Arc::new(|| {}));
        for (serial, pkg) in [
            ("mine", "com.a"),
            ("mine", "com.s"),
            ("other", "com.s"),
            ("other", "com.c"),
        ] {
            cat.start_session(serial);
            cat.upsert(&AppEntry {
                pkg: pkg.into(),
                user: 0,
                label: pkg.into(),
                work: false,
                icon_png: Vec::new(),
            })
            .unwrap();
        }
        drop(cat);
        let removed =
            remove_catalogue_entries_with(dirs.clone(), "mine", std::sync::Arc::new(|| {}));
        let mut left: Vec<String> = std::fs::read_dir(&dirs.applications)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let shared =
            Catalog::with_rebuild(dirs.clone(), std::sync::Arc::new(|| {})).phones("com.s", 0);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(removed.unwrap(), 2);
        // The shared entry stays, listing only the other phone.
        assert_eq!(left, ["lantern.com.c.desktop", "lantern.com.s.desktop"]);
        assert_eq!(shared, ["other"]);
    }

    #[test]
    fn stop_first_points_at_the_tray() {
        assert_eq!(
            STOP_FIRST,
            "stop lanternd first (systemctl --user stop lanternd), or use \"Undo phone setup…\" in Lantern's tray menu"
        );
    }

    #[test]
    fn no_daemon_does_not_block() {
        let dir = std::env::temp_dir().join(format!("lantern-undo-{}", std::process::id()));
        check_daemon(&dir.join("absent.sock")).unwrap();
    }

    #[test]
    fn serial_comes_from_flag_env_or_the_only_device() {
        let never = || -> Result<String> { panic!("adb devices not needed") };
        assert_eq!(resolve_serial(Some("x"), Some("y"), never).unwrap(), "x");
        assert_eq!(resolve_serial(None, Some("y"), never).unwrap(), "y");
        let one = || Ok("List of devices attached\n20e9318\tdevice\nzz\toffline\n\n".to_owned());
        assert_eq!(resolve_serial(None, None, one).unwrap(), "20e9318");
        let none = || Ok("List of devices attached\n\n".to_owned());
        assert!(resolve_serial(None, None, none).is_err());
        let two = || Ok("List of devices attached\na\tdevice\nb\tdevice\n".to_owned());
        let error = resolve_serial(None, None, two).unwrap_err().to_string();
        assert!(error.contains("--serial"), "{error}");
    }
}
