//! Keeps the companion app (`dev.lantern.companion`: keyboard and app list) installed on the
//! phone at exactly the build embedded in `lantern`, and tells the user the first time Lantern
//! installs something on a phone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::adb::{Adb, ShellOutput, ADB_CALL_TIMEOUT, ADB_INSTALL_TIMEOUT};
use crate::daemon::session::hex;

pub static COMPANION_APK: &[u8] = include_bytes!("../../assets/companion.apk");

/// The companion's package name.
pub const PACKAGE: &str = "dev.lantern.companion";

/// What [`ensure`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// The installed APK already is the embedded one.
    Current,
    /// The companion was not installed; now it is.
    Installed,
    /// Another build was installed; it was replaced.
    Updated,
}

/// The embedded companion APK's SHA-256 (lowercase hex).
pub fn companion_sha() -> String {
    hex(&Sha256::digest(COMPANION_APK))
}

/// The installed APK path from `pm path` output (`package:<path>` per split; the base APK is
/// preferred), or `None` if there is none.
pub fn parse_pm_path(output: &str) -> Option<String> {
    let paths: Vec<&str> = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .filter(|path| !path.is_empty())
        .collect();
    paths
        .iter()
        .find(|path| path.ends_with("/base.apk"))
        .or(paths.first())
        .map(|path| (*path).to_owned())
}

/// The companion's installed APK path for user 0, or `None` when it is not installed. A failure
/// that is not "not installed" (`pm path` exits 1 with no output) is an error.
pub async fn installed_path(adb: &Adb) -> Result<Option<String>> {
    let out = adb
        .shell_output(&["pm", "path", "--user", "0", PACKAGE], ADB_CALL_TIMEOUT)
        .await
        .context("pm path")?;
    match parse_pm_path(&out.text) {
        Some(path) => Ok(Some(path)),
        None if out.text.trim().is_empty() => Ok(None),
        None => bail!("pm path {PACKAGE}: {}", out.text.trim()),
    }
}

fn install_ok(out: &ShellOutput) -> bool {
    out.success && out.text.lines().any(|line| line.trim() == "Success")
}

async fn pm_install(adb: &Adb, remote: &str) -> Result<ShellOutput> {
    adb.shell_output(
        &["pm", "install", "-r", "-d", "--user", "0", remote],
        ADB_INSTALL_TIMEOUT,
    )
    .await
    .context("pm install")
}

/// `pm uninstall --user 0` the companion. "Not installed" counts as success.
pub async fn uninstall(adb: &Adb) -> Result<()> {
    let out = adb
        .shell_output(
            &["pm", "uninstall", "--user", "0", PACKAGE],
            ADB_INSTALL_TIMEOUT,
        )
        .await
        .context("pm uninstall")?;
    if install_ok(&out) || out.text.contains("not installed for") {
        return Ok(());
    }
    bail!("pm uninstall {PACKAGE} failed: {}", out.text.trim())
}

/// Installs the pushed APK at `remote`. A companion signed with another key can't be updated
/// in place (`INSTALL_FAILED_UPDATE_INCOMPATIBLE`): it is uninstalled and the install retried
/// once.
async fn install(adb: &Adb, remote: &str) -> Result<()> {
    let first = pm_install(adb, remote).await?;
    if install_ok(&first) {
        return Ok(());
    }
    if !first.text.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") {
        bail!("pm install failed: {}", first.text.trim());
    }
    tracing::warn!(
        output = first.text.trim(),
        "installed companion has another signature; uninstalling it"
    );
    uninstall(adb).await?;
    let second = pm_install(adb, remote).await?;
    if install_ok(&second) {
        return Ok(());
    }
    bail!(
        "pm install failed after uninstalling the old companion: {}",
        second.text.trim()
    )
}

/// Makes the installed companion (user 0) the embedded build: nothing if its APK's SHA-256
/// already matches, else push + `pm install -r -d`. The pushed APK is always removed.
pub async fn ensure(adb: &Adb) -> Result<Install> {
    let sha = companion_sha();
    let existing = installed_path(adb).await?;
    if let Some(path) = &existing {
        match adb.shell(&["sha256sum", path]).await {
            Ok(out) if out.split_whitespace().next() == Some(sha.as_str()) => {
                return Ok(Install::Current)
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, path, "could not hash the installed companion"),
        }
    }
    let remote = format!("/data/local/tmp/lantern-companion-{}.apk", &sha[..16]);
    let result = async {
        adb.push_bytes(COMPANION_APK, "lantern-companion", &remote)
            .await
            .context("push the companion apk")?;
        install(adb, &remote).await
    }
    .await;
    if let Err(error) = adb.shell(&["rm", "-f", &remote]).await {
        tracing::warn!(%error, remote, "could not remove the pushed companion apk");
    }
    result?;
    Ok(if existing.is_some() {
        Install::Updated
    } else {
        Install::Installed
    })
}

/// Lantern's state directory, `$XDG_STATE_HOME/lantern` (`~/.local/state/lantern`).
pub fn state_dir() -> Option<PathBuf> {
    Some(dirs::state_dir()?.join("lantern"))
}

/// The device record file of `serial` under `state_dir`. Characters outside `[A-Za-z0-9._:-]`
/// (and a leading `.`) become `_`, so a serial can never name a path outside `devices/`.
pub fn record_path(state_dir: &Path, serial: &str) -> PathBuf {
    let mut name: String = serial
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._:-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || name.starts_with('.') {
        name.insert(0, '_');
    }
    state_dir.join("devices").join(format!("{name}.json"))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Records that Lantern set up `serial` (`devices/<serial>.json` =
/// `{"companion":true,"since":<unix s>}`). Returns true if this is the first record for that
/// serial; an existing record is left as it is.
pub fn record_setup(state_dir: &Path, serial: &str, i: Install) -> Result<bool> {
    let path = record_path(state_dir, serial);
    if path.exists() {
        return Ok(false);
    }
    let dir = path.parent().expect("record has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let body = serde_json::json!({"companion": true, "since": unix_now()}).to_string();
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    // A hard link never replaces an existing file, so of two racing writers exactly one wins.
    let linked = std::fs::hard_link(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            tracing::debug!(serial, install = ?i, path = %path.display(), "device record written");
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).with_context(|| format!("write {}", path.display())),
    }
}

/// The summary of the first-setup notice.
pub const SETUP_SUMMARY: &str = "Lantern set up your phone";
/// The summary of the notice that the companion could not be installed.
pub const FAILURE_SUMMARY: &str = "Lantern couldn't install its companion app";

fn setup_body(serial: &str) -> String {
    format!(
        "Installed the Lantern companion app (keyboard and app list) on {serial}. \
         Run `lantern undo-setup` to remove it."
    )
}

type Notifier = Box<dyn Fn(&str, &str) + Send + Sync>;

/// The daemon's companion step before every session start, with its once-per-run state.
pub struct Setup {
    state_dir: Option<PathBuf>,
    notify: Notifier,
    failure_notified: AtomicBool,
}

impl Setup {
    /// `notify(summary, body)` shows a desktop notification (tests record instead).
    pub fn new(
        state_dir: Option<PathBuf>,
        notify: impl Fn(&str, &str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            state_dir,
            notify: Box::new(notify),
            failure_notified: AtomicBool::new(false),
        }
    }

    /// The daemon's setup: `~/.local/state/lantern`, notifications through `notify-send`.
    pub fn desktop() -> Self {
        Self::new(state_dir(), |summary, body| {
            crate::notify::send(summary, body, None)
        })
    }

    /// Runs [`ensure`] and records the device; never fails the session start. The first record
    /// for a serial notifies the user. A failure is logged in full and notified (first line)
    /// once per daemon run; the session then starts without the companion.
    pub async fn prepare(&self, adb: &Adb) -> Option<Install> {
        let serial = adb.serial();
        match ensure(adb).await {
            Ok(install) => {
                match install {
                    Install::Current => tracing::info!(serial, "companion current"),
                    Install::Installed => tracing::info!(serial, "companion installed"),
                    Install::Updated => tracing::info!(serial, "companion updated"),
                }
                let Some(state_dir) = &self.state_dir else {
                    tracing::warn!("no state directory; device record not written");
                    return Some(install);
                };
                match record_setup(state_dir, serial, install) {
                    Ok(true) => (self.notify)(SETUP_SUMMARY, &setup_body(serial)),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(serial, error = format!("{error:#}"), "device record")
                    }
                }
                Some(install)
            }
            Err(error) => {
                let full = format!("{error:#}");
                tracing::error!(
                    serial,
                    error = full,
                    "could not install the companion; starting without it"
                );
                if !self.failure_notified.swap(true, Ordering::SeqCst) {
                    let first = full.lines().next().unwrap_or_default();
                    (self.notify)(FAILURE_SUMMARY, first);
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lantern-companion-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn record_setup_first_time_only() {
        let state = scratch("record");
        assert!(record_setup(&state, "20e9318", Install::Installed).unwrap());
        let first = std::fs::read_to_string(record_path(&state, "20e9318")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(json["companion"], serde_json::json!(true));
        assert!(json["since"].is_u64(), "{first}");
        assert_eq!(json.as_object().unwrap().len(), 2, "{first}");
        assert!(!record_setup(&state, "20e9318", Install::Current).unwrap());
        assert!(!record_setup(&state, "20e9318", Install::Updated).unwrap());
        // The existing record is kept as it was.
        assert_eq!(
            std::fs::read_to_string(record_path(&state, "20e9318")).unwrap(),
            first
        );
        assert!(record_setup(&state, "192.168.1.5:5555", Install::Current).unwrap());
        // No temp files are left behind.
        let names: Vec<String> = std::fs::read_dir(state.join("devices"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn record_path_stays_inside_devices() {
        let state = Path::new("/s");
        assert_eq!(
            record_path(state, "20e9318"),
            Path::new("/s/devices/20e9318.json")
        );
        assert_eq!(
            record_path(state, "192.168.1.5:5555"),
            Path::new("/s/devices/192.168.1.5:5555.json")
        );
        assert_eq!(
            record_path(state, "../../x"),
            Path::new("/s/devices/_.._.._x.json")
        );
        assert_eq!(record_path(state, ""), Path::new("/s/devices/_.json"));
    }

    #[test]
    fn pm_path_prefers_the_base_apk() {
        assert_eq!(parse_pm_path(""), None);
        assert_eq!(
            parse_pm_path("package:/data/app/a/base.apk\r\n"),
            Some("/data/app/a/base.apk".into())
        );
        assert_eq!(
            parse_pm_path("package:/data/app/a/split_x.apk\npackage:/data/app/a/base.apk\n"),
            Some("/data/app/a/base.apk".into())
        );
        assert_eq!(parse_pm_path("error: device offline\n"), None);
    }

    #[test]
    fn setup_body_names_the_serial_and_the_undo_command() {
        assert_eq!(
            setup_body("20e9318"),
            "Installed the Lantern companion app (keyboard and app list) on 20e9318. \
             Run `lantern undo-setup` to remove it."
        );
    }
}
