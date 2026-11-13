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

/// Writes `bytes` to `path` through a temp file in the same directory (created if needed),
/// fsyncs it, renames it over `path` and fsyncs the directory. The temp file never outlives a
/// failure.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}-{}",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    if let Err(error) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::debug!(%error, dir = %dir.display(), "directory fsync failed");
    }
    Ok(())
}

/// A phone's device record (`devices/<serial>.json`), as far as Lantern reads it. The file may
/// hold other keys; they are kept by every write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    /// Lantern set up this phone (installed the companion) and has not undone it.
    pub companion: bool,
    /// When Lantern first set it up (unix seconds).
    pub since: Option<u64>,
    /// Its friendly name.
    pub name: Option<String>,
    /// The user switched it off in Lantern.
    pub off: bool,
    /// It was the connected phone when lanternd last stopped (or still is).
    pub live: bool,
    /// When it last became ready (unix seconds).
    pub connected_at: Option<u64>,
}

/// `serial`'s device record; `None` when there is none or it is not a JSON object.
pub fn read_record(state_dir: &Path, serial: &str) -> Option<Record> {
    let text = std::fs::read_to_string(record_path(state_dir, serial)).ok()?;
    let serde_json::Value::Object(map) = serde_json::from_str(&text).ok()? else {
        return None;
    };
    let flag = |key: &str| map.get(key).and_then(|v| v.as_bool()) == Some(true);
    Some(Record {
        companion: flag("companion"),
        since: map.get("since").and_then(|v| v.as_u64()),
        name: map
            .get("name")
            .and_then(|v| v.as_str())
            .and_then(clean_name),
        off: flag("off"),
        live: flag("live"),
        connected_at: map.get("connected_at").and_then(|v| v.as_u64()),
    })
}

/// Edits `serial`'s device record with `f`. Creates `{}` if absent; keeps unknown keys; always
/// writes `"serial"`; temp + fsync + rename. Returns whether the file changed (nothing is
/// written when it would not). A record that isn't a JSON object is an error and stays as it is.
///
/// The read-modify-write holds an flock on `devices/.lock`, so a concurrent writer can't lose
/// this change (an `off` flag lost that way would connect a phone without the user's say-so).
/// Callers are lanternd's run loop (and the undo it runs) or the CLI while lanternd is down.
pub fn update_record(
    state_dir: &Path,
    serial: &str,
    f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<bool> {
    use std::os::fd::AsRawFd;
    let path = record_path(state_dir, serial);
    let dir = path.parent().context("record path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let lock_path = dir.join(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    // SAFETY: flock on a file descriptor we own; released when `lock` closes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("lock {}", lock_path.display()));
    }
    let changed = update_record_locked(&path, serial, f);
    drop(lock);
    changed
}

/// [`update_record`]'s read-modify-write, with `devices/.lock` held.
fn update_record_locked(
    path: &Path,
    serial: &str,
    f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<bool> {
    let path = path.to_path_buf();
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => {
            match serde_json::from_str(&text)
                .with_context(|| format!("parse {}", path.display()))?
            {
                serde_json::Value::Object(map) => Some(map),
                _ => bail!("{} is not a JSON object", path.display()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut map = before.clone().unwrap_or_default();
    f(&mut map);
    map.insert("serial".to_owned(), serial.into());
    if before.as_ref() == Some(&map) {
        return Ok(false);
    }
    write_atomic(&path, serde_json::Value::Object(map).to_string().as_bytes())?;
    Ok(true)
}

/// Sets `serial`'s `off` flag (creating its record if needed). Returns whether it changed.
pub fn set_off(state_dir: &Path, serial: &str, off: bool) -> Result<bool> {
    update_record(state_dir, serial, |map| {
        map.insert("off".to_owned(), off.into());
    })
}

/// Sets `serial`'s `live` flag (creating its record if needed); `true` also sets
/// `connected_at` to now. Returns whether the record changed.
pub fn set_live(state_dir: &Path, serial: &str, live: bool) -> Result<bool> {
    update_record(state_dir, serial, |map| {
        map.insert("live".to_owned(), live.into());
        if live {
            map.insert("connected_at".to_owned(), unix_now().into());
        }
    })
}

/// Every phone with a device record: its `"serial"` key, else (records from before that key)
/// the file stem. Sorted, without duplicates; temp files are skipped.
pub fn known_serials(state_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(state_dir.join("devices")) else {
        return Vec::new();
    };
    let mut serials: Vec<String> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let file = path.file_name()?.to_str()?;
            let stem = file.strip_suffix(".json")?;
            if file.starts_with('.') || stem.is_empty() {
                return None;
            }
            let from_key = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|json| json.get("serial")?.as_str().map(str::to_owned))
                .filter(|serial| !serial.is_empty());
            Some(from_key.unwrap_or_else(|| stem.to_owned()))
        })
        .collect();
    serials.sort();
    serials.dedup();
    serials
}

/// Records that Lantern set up `serial`: sets `companion:true` and `since` (if missing) in its
/// device record, keeping every other key. Returns true when the record lacked
/// `companion:true` (no record, one written by the flags, or one whose setup was undone): the
/// caller then tells the user. A record that already says so is left untouched.
pub fn record_setup(state_dir: &Path, serial: &str, i: Install) -> Result<bool> {
    if read_record(state_dir, serial).is_some_and(|r| r.companion) {
        return Ok(false);
    }
    update_record(state_dir, serial, |map| {
        map.insert("companion".to_owned(), true.into());
        if !map.get("since").is_some_and(|v| v.is_u64()) {
            map.insert("since".to_owned(), unix_now().into());
        }
    })?;
    tracing::debug!(serial, install = ?i, path = %record_path(state_dir, serial).display(), "device record written");
    Ok(true)
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

/// Sets `"name"` in `serial`'s existing device record, keeping every other key (through
/// [`update_record`]). Returns whether the record changed. Without a record it does nothing
/// (`Ok(false)`): the name is recorded once Lantern set the phone up.
pub fn record_name(state_dir: &Path, serial: &str, name: &str) -> Result<bool> {
    if !record_path(state_dir, serial).exists() {
        return Ok(false);
    }
    update_record(state_dir, serial, |map| {
        map.insert("name".to_owned(), name.into());
    })
}

/// The recorded name of `serial`, if its device record has a non-empty one.
pub fn device_name(state_dir: &Path, serial: &str) -> Option<String> {
    read_record(state_dir, serial)?.name
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
        assert_eq!(json["serial"], serde_json::json!("20e9318"));
        assert_eq!(json.as_object().unwrap().len(), 3, "{first}");
        assert!(!record_setup(&state, "20e9318", Install::Current).unwrap());
        assert!(!record_setup(&state, "20e9318", Install::Updated).unwrap());
        // The existing record is kept as it was.
        assert_eq!(
            std::fs::read_to_string(record_path(&state, "20e9318")).unwrap(),
            first
        );
        assert!(record_setup(&state, "192.168.1.5:5555", Install::Current).unwrap());
        // No temp files are left behind.
        let names = files_in(&state.join("devices"));
        assert_eq!(names.len(), 2, "{names:?}");
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The files in `dir`, sorted, without `update_record`'s `.lock`.
    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != ".lock")
            .collect();
        names.sort();
        names
    }

    fn raw(state: &Path, serial: &str) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(record_path(state, serial)).unwrap()).unwrap()
    }

    /// R3: two writers' read-modify-writes of one record never lose each other's flag (lanternd's
    /// loop and its undo, or the CLI): `devices/.lock` serializes them.
    #[test]
    fn concurrent_flag_updates_both_survive() {
        let state = scratch("lock");
        for round in 0..200u32 {
            let (off, live) = (round % 2 == 0, round % 3 == 0);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let writers: Vec<_> = [true, false]
                .into_iter()
                .map(|is_off| {
                    let (state, barrier) = (state.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        if is_off {
                            set_off(&state, "dev", off).unwrap();
                        } else {
                            set_live(&state, "dev", live).unwrap();
                        }
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            let record = read_record(&state, "dev").unwrap();
            assert_eq!((record.off, record.live), (off, live), "round {round}");
        }
        // The lock file is not a record.
        assert!(state.join("devices").join(".lock").exists());
        assert_eq!(known_serials(&state), vec!["dev".to_owned()]);
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn record_flags_roundtrip_keep_unknown_keys() {
        let state = scratch("flags");
        let path = record_path(&state, "20e9318");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"companion":true,"since":42,"extra":[1]}"#).unwrap();
        assert_eq!(
            read_record(&state, "20e9318"),
            Some(Record {
                companion: true,
                since: Some(42),
                ..Record::default()
            })
        );
        assert!(set_off(&state, "20e9318", true).unwrap());
        assert!(!set_off(&state, "20e9318", true).unwrap(), "no change");
        assert!(set_live(&state, "20e9318", true).unwrap());
        let json = raw(&state, "20e9318");
        assert_eq!(json["extra"], serde_json::json!([1]));
        assert_eq!(json["since"], serde_json::json!(42));
        assert_eq!(json["serial"], serde_json::json!("20e9318"));
        assert_eq!(json["companion"], serde_json::json!(true));
        assert!(json["connected_at"].is_u64(), "{json}");
        let record = read_record(&state, "20e9318").unwrap();
        assert!(record.companion && record.off && record.live);
        assert_eq!(record.since, Some(42));
        assert!(record.connected_at.is_some());
        // live:false keeps the last connected_at.
        assert!(set_live(&state, "20e9318", false).unwrap());
        assert!(set_off(&state, "20e9318", false).unwrap());
        let after = read_record(&state, "20e9318").unwrap();
        assert!(!after.live && !after.off);
        assert_eq!(after.connected_at, record.connected_at);
        assert_eq!(files_in(&state.join("devices")), ["20e9318.json"]);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn set_flags_create_a_record_that_is_not_set_up() {
        let state = scratch("create");
        assert_eq!(read_record(&state, "a"), None);
        assert!(set_off(&state, "a", true).unwrap());
        let record = read_record(&state, "a").unwrap();
        assert!(record.off && !record.companion && record.since.is_none());
        assert_eq!(raw(&state, "a")["serial"], serde_json::json!("a"));
        // Not set up yet: the first setup still reports (and notifies) once.
        assert!(record_setup(&state, "a", Install::Installed).unwrap());
        assert!(!record_setup(&state, "a", Install::Current).unwrap());
        let record = read_record(&state, "a").unwrap();
        assert!(record.companion && record.off && record.since.is_some());
        assert_eq!(files_in(&state.join("devices")), ["a.json"]);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn record_setup_after_undo_notifies_again() {
        let state = scratch("undo");
        let path = record_path(&state, "a");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"companion":false,"since":42,"name":"N","off":true,"serial":"a"}"#,
        )
        .unwrap();
        assert!(record_setup(&state, "a", Install::Installed).unwrap());
        let json = raw(&state, "a");
        assert_eq!(
            json,
            serde_json::json!({"companion": true, "since": 42, "name": "N", "off": true, "serial": "a"})
        );
        assert!(!record_setup(&state, "a", Install::Current).unwrap());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn known_serials_reads_serial_key_and_stems() {
        let state = scratch("known");
        assert!(known_serials(&state).is_empty());
        let devices = state.join("devices");
        std::fs::create_dir_all(&devices).unwrap();
        std::fs::write(devices.join("_.._x.json"), r#"{"serial":"../x"}"#).unwrap();
        std::fs::write(devices.join("20e9318.json"), r#"{"companion":true}"#).unwrap();
        // Temp files and other files are not records.
        std::fs::write(devices.join(".20e9318.json.tmp-1"), "{}").unwrap();
        std::fs::write(devices.join("notes.txt"), "").unwrap();
        assert_eq!(known_serials(&state), ["../x", "20e9318"]);
        // A record written by set_off is known by its serial.
        set_off(&state, "192.168.1.5:5555", true).unwrap();
        assert_eq!(
            known_serials(&state),
            ["../x", "192.168.1.5:5555", "20e9318"]
        );
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
            serde_json::json!({"companion": true, "since": 42, "extra": [1], "name": "Xiaomi 11T Pro", "serial": "20e9318"})
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
        assert_eq!(files_in(&state.join("devices")), ["20e9318.json"]);
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
