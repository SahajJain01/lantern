//! Keeps the companion app (`dev.lantern.companion`: keyboard and app list) installed on the
//! phone at exactly the build embedded in `lantern`, and tells the user the first time Lantern
//! installs something on a phone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// The longest phone name kept, in chars.
const MAX_NAME: usize = 64;

/// A phone name as read from a setting or property: trimmed, control characters dropped, at
/// most [`MAX_NAME`] chars. Empty and `null` (an unset setting) give `None`.
pub fn clean_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed == "null" {
        return None;
    }
    let name: String = trimmed
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// One `adb shell` read for the phone name, cleaned; `None` on any failure.
async fn read_name_part(adb: &Adb, args: &[&str]) -> Option<String> {
    match adb.shell_output(args, ADB_CALL_TIMEOUT).await {
        Ok(out) if out.success => clean_name(&out.text),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(
                serial = adb.serial(),
                ?args,
                error = format!("{error:#}"),
                "phone name read failed"
            );
            None
        }
    }
}

/// The phone's friendly name: `settings get global device_name` ("Xiaomi 11T Pro"), else
/// `ro.product.marketname`, else `<manufacturer> <model>`.
pub async fn read_phone_name(adb: &Adb) -> Option<String> {
    if let Some(name) = read_name_part(adb, &["settings", "get", "global", "device_name"]).await {
        return Some(name);
    }
    if let Some(name) = read_name_part(adb, &["getprop", "ro.product.marketname"]).await {
        return Some(name);
    }
    let maker = read_name_part(adb, &["getprop", "ro.product.manufacturer"]).await;
    let model = read_name_part(adb, &["getprop", "ro.product.model"]).await;
    let both: Vec<String> = [maker, model].into_iter().flatten().collect();
    clean_name(&both.join(" "))
}

/// Sets `"name"` in `serial`'s existing device record, keeping every other key (temp file +
/// rename). Returns whether the record changed. Without a record it does nothing (`Ok(false)`):
/// a record still means "Lantern set up this phone".
pub fn record_name(state_dir: &Path, serial: &str, name: &str) -> Result<bool> {
    let path = record_path(state_dir, serial);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut record: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let Some(object) = record.as_object_mut() else {
        bail!("{} is not a JSON object", path.display());
    };
    if object.get("name").and_then(|n| n.as_str()) == Some(name) {
        return Ok(false);
    }
    object.insert("name".to_owned(), name.into());
    let dir = path.parent().expect("record has a parent");
    let tmp = dir.join(format!(
        ".{}.name-{}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, record.to_string()).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(error) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("replace {}", path.display()));
    }
    Ok(true)
}

/// The recorded name of `serial`, if its device record has a non-empty one.
pub fn device_name(state_dir: &Path, serial: &str) -> Option<String> {
    let text = std::fs::read_to_string(record_path(state_dir, serial)).ok()?;
    let record: serde_json::Value = serde_json::from_str(&text).ok()?;
    clean_name(record.get("name")?.as_str()?)
}

/// Looks up a phone's display name by serial.
pub type Names = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// [`Names`] over the device records in `state_dir`.
pub fn names_in(state_dir: PathBuf) -> Names {
    Arc::new(move |serial| device_name(&state_dir, serial))
}

/// [`Names`] over the device records in [`state_dir`] (none without one).
pub fn names() -> Names {
    match state_dir() {
        Some(dir) => names_in(dir),
        None => Arc::new(|_| None),
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

    /// Reads the phone's name into its device record (right after [`record_setup`]); a failure
    /// is only logged.
    async fn record_name(&self, adb: &Adb, state_dir: &Path) {
        let serial = adb.serial();
        let Some(name) = read_phone_name(adb).await else {
            tracing::info!(serial, "phone name unknown");
            return;
        };
        match record_name(state_dir, serial, &name) {
            Ok(changed) => tracing::info!(serial, name, changed, "phone name"),
            Err(error) => tracing::warn!(
                serial,
                error = format!("{error:#}"),
                "phone name not recorded"
            ),
        }
    }

    /// Runs [`ensure`] and records the device and its name; never fails the session start. The
    /// first record for a serial notifies the user. A failure is logged in full and notified (first line)
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
                    Ok(first) => {
                        if first {
                            (self.notify)(SETUP_SUMMARY, &setup_body(serial));
                        }
                        self.record_name(adb, state_dir).await;
                    }
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
    fn clean_name_trims_and_rejects_null() {
        assert_eq!(
            clean_name(" Xiaomi 11T Pro\n"),
            Some("Xiaomi 11T Pro".into())
        );
        assert_eq!(clean_name("null"), None);
        assert_eq!(clean_name(" null\r\n"), None);
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name(" \n"), None);
        assert_eq!(clean_name(&"x".repeat(100)), Some("x".repeat(64)));
        assert_eq!(clean_name("a\u{7}b"), Some("ab".into()));
        assert_eq!(clean_name(&"é".repeat(70)), Some("é".repeat(64)));
    }

    #[test]
    fn record_name_keeps_other_keys_and_needs_a_record() {
        let state = scratch("name");
        // No record: nothing is written, so the first-setup notice still fires later.
        assert!(!record_name(&state, "20e9318", "Xiaomi 11T Pro").unwrap());
        assert!(!record_path(&state, "20e9318").exists());
        assert_eq!(device_name(&state, "20e9318"), None);

        let path = record_path(&state, "20e9318");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"companion":true,"since":42,"extra":[1]}"#).unwrap();
        assert!(record_name(&state, "20e9318", "Xiaomi 11T Pro").unwrap());
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"companion": true, "since": 42, "extra": [1], "name": "Xiaomi 11T Pro"})
        );
        assert_eq!(
            device_name(&state, "20e9318"),
            Some("Xiaomi 11T Pro".into())
        );
        // The same name again changes nothing; a new one replaces it.
        assert!(!record_name(&state, "20e9318", "Xiaomi 11T Pro").unwrap());
        assert!(record_name(&state, "20e9318", "Work phone").unwrap());
        assert_eq!(device_name(&state, "20e9318"), Some("Work phone".into()));
        assert_eq!(
            names_in(state.clone())("20e9318"),
            Some("Work phone".into())
        );
        assert_eq!(names_in(state.clone())("other"), None);
        // No temp files are left behind.
        let files: Vec<String> = std::fs::read_dir(state.join("devices"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, ["20e9318.json"]);
        // A record without a usable name has none.
        std::fs::write(&path, r#"{"companion":true,"name":""}"#).unwrap();
        assert_eq!(device_name(&state, "20e9318"), None);
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
