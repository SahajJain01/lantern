//! `~/.config/lantern/settings.toml`: Lantern's user settings.
//!
//! The tray menu writes the file through [`set`]; hand edits are picked up by
//! [`SettingsHandle::spawn_poll`] within [`POLL`]. Readers hold a `watch` of the typed values.
//! - A missing file or key means the default (every setting on).
//! - A known key with a wrong-typed value gets its default plus a warning; unknown keys are
//!   ignored when reading and kept when writing.
//! - A file that doesn't parse gives the defaults and is never overwritten: [`set`] refuses.
//! - Writing regenerates [`HEADER`] and drops any other comments.

use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::watch;
use toml::{Table, Value};

/// The comment at the top of every file Lantern writes.
pub const HEADER: &str = "\
# Lantern settings. Lantern's tray menu changes this file; edits here apply within a few seconds.
# clipboard_sync: copy text between the PC and the phone (true/false).
# lantern_keyboard: type on the phone through the Lantern keyboard (true/false; applies at the next connection).
";

/// How often [`SettingsHandle::spawn_poll`] checks the file for hand edits.
pub const POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Copy text between the PC and the phone; applies at once.
    pub clipboard_sync: bool,
    /// Type through the Lantern keyboard; applies at the next connection.
    pub lantern_keyboard: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            clipboard_sync: true,
            lantern_keyboard: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    ClipboardSync,
    LanternKeyboard,
}

impl Key {
    const ALL: [Key; 2] = [Key::ClipboardSync, Key::LanternKeyboard];

    /// The key's name in the file.
    pub fn name(self) -> &'static str {
        match self {
            Key::ClipboardSync => "clipboard_sync",
            Key::LanternKeyboard => "lantern_keyboard",
        }
    }

    fn field(self, settings: &mut Settings) -> &mut bool {
        match self {
            Key::ClipboardSync => &mut settings.clipboard_sync,
            Key::LanternKeyboard => &mut settings.lantern_keyboard,
        }
    }
}

/// `$XDG_CONFIG_HOME/lantern/settings.toml`, else `~/.config/lantern/settings.toml`.
pub fn default_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("lantern").join("settings.toml"))
}

/// The file's table, or its syntax error as `line N: <message>`.
fn parse_table(text: &str) -> Result<Table, String> {
    text.parse::<Table>().map_err(|error| {
        let message = error.message().trim();
        match error.span() {
            Some(span) => {
                let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
                format!("line {line}: {message}")
            }
            None => message.to_owned(),
        }
    })
}

/// The settings a table gives, plus one warning per known key with a wrong-typed value.
fn from_table(table: &Table) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut warnings = Vec::new();
    for key in Key::ALL {
        let field = key.field(&mut settings);
        match table.get(key.name()) {
            None => {}
            Some(Value::Boolean(value)) => *field = *value,
            Some(_) => warnings.push(format!(
                "{} must be true or false; using {}",
                key.name(),
                *field
            )),
        }
    }
    (settings, warnings)
}

/// Parses a file's text. `Err` is a TOML syntax error (with its line); the `Vec` holds
/// per-key warnings.
pub fn parse(text: &str) -> Result<(Settings, Vec<String>), String> {
    parse_table(text).map(|table| from_table(&table))
}

/// Reads `path`: an absent file gives the defaults; one that can't be read or parsed gives
/// the defaults plus the error text. Logs its error or warnings; callers load once per file
/// change, so nothing repeats while the file stays the same.
pub fn load(path: &Path) -> (Settings, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return (Settings::default(), None),
        Err(error) => {
            let error = format!("cannot read {}: {error}", path.display());
            tracing::warn!(error, "settings file unreadable; using the defaults");
            return (Settings::default(), Some(error));
        }
    };
    match parse(&text) {
        Ok((settings, warnings)) => {
            for warning in warnings {
                tracing::warn!(path = %path.display(), "{warning}");
            }
            (settings, None)
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), error, "settings.toml has an error; using the defaults");
            (Settings::default(), Some(error))
        }
    }
}

/// Sets `key` in the file at `path`, keeping every other key (comments other than [`HEADER`]
/// are dropped). An absent file starts empty; one that doesn't parse is left as it is.
pub fn set(path: &Path, key: Key, value: bool) -> Result<Settings> {
    let mut table = match std::fs::read_to_string(path) {
        Ok(text) => parse_table(&text)
            .map_err(|error| anyhow!("settings.toml has an error: {error}; fix it first"))?,
        Err(error) if error.kind() == ErrorKind::NotFound => Table::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    table.insert(key.name().to_owned(), Value::Boolean(value));
    write(path, &table)?;
    Ok(from_table(&table).0)
}

/// Writes [`HEADER`] and the defaults to `path` if nothing is there.
pub fn ensure_file(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    let mut defaults = Settings::default();
    let mut table = Table::new();
    for key in Key::ALL {
        let value = *key.field(&mut defaults);
        table.insert(key.name().to_owned(), Value::Boolean(value));
    }
    write(path, &table)
}

/// [`HEADER`] + `table`, atomically: a 0600 temp file renamed over `path`, in a directory
/// created 0700 if missing.
fn write(path: &Path, table: &Table) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no directory", path.display()))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(format!("{HEADER}{table}").as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// What [`SettingsHandle::spawn_poll`] compares: the file's mtime and length (`None` if absent
/// or unreadable).
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The settings file plus a `watch` of its values, shared by everything in lanternd.
#[derive(Clone)]
pub struct SettingsHandle {
    path: Option<PathBuf>,
    tx: watch::Sender<Settings>,
    /// The file's read or parse error while it has one ([`load`]).
    error: watch::Sender<Option<String>>,
    /// The file's stamp when [`Self::new`] loaded it.
    loaded: Option<(SystemTime, u64)>,
    /// Held across [`Self::set`]: concurrent writes would share one temp file name.
    writing: Arc<Mutex<()>>,
}

impl SettingsHandle {
    /// Loads `path` once. With `None` (no config directory) the defaults hold and writes are
    /// refused.
    pub fn new(path: Option<PathBuf>) -> Self {
        let loaded = path.as_deref().and_then(stamp);
        let (settings, error) = path.as_deref().map(load).unwrap_or_default();
        Self {
            path,
            tx: watch::Sender::new(settings),
            error: watch::Sender::new(error),
            loaded,
            writing: Arc::default(),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Settings> {
        self.tx.subscribe()
    }

    pub fn get(&self) -> Settings {
        self.tx.borrow().clone()
    }

    /// The file's read or parse error, while the defaults stand in for it.
    pub fn error(&self) -> Option<String> {
        self.error.borrow().clone()
    }

    /// Woken whenever [`Self::error`] changes.
    pub fn subscribe_error(&self) -> watch::Receiver<Option<String>> {
        self.error.subscribe()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Writes `key` to the file ([`set`]) and publishes the result at once. Blocks on file IO
    /// (and on another thread's `set`).
    pub fn set(&self, key: Key, value: bool) -> Result<()> {
        let Some(path) = &self.path else {
            bail!("no settings file (no config directory)");
        };
        let _writing = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let settings = set(path, key, value)?;
        tracing::info!(key = key.name(), value, "setting changed");
        self.tx.send_replace(settings);
        // The file parsed, so any earlier error is gone.
        self.error.send_if_modified(|error| error.take().is_some());
        Ok(())
    }

    /// Reloads the file whenever its mtime or length changes, checking every [`POLL`].
    /// Subscribers are woken only when a value (or the error) actually changed.
    pub fn spawn_poll(&self) -> tokio::task::JoinHandle<()> {
        let (path, tx, errors, mut last) = (
            self.path.clone(),
            self.tx.clone(),
            self.error.clone(),
            self.loaded,
        );
        tokio::spawn(async move {
            let Some(path) = path else {
                return;
            };
            let mut tick = tokio::time::interval(POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // the first tick is immediate
            loop {
                tick.tick().await;
                let now = stamp(&path);
                if now == last {
                    continue;
                }
                last = now;
                let (settings, error) = load(&path);
                tx.send_if_modified(|current| {
                    if *current == settings {
                        return false;
                    }
                    tracing::info!(?settings, "settings file changed");
                    *current = settings;
                    true
                });
                // After the values, so an error watcher sees the values it came with.
                errors.send_if_modified(|current| {
                    let changed = *current != error;
                    *current = error;
                    changed
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DIRS: AtomicUsize = AtomicUsize::new(0);

    /// A fresh scratch directory (not created) for one test.
    fn scratch() -> PathBuf {
        let n = DIRS.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("lantern-settings-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn defaults_when_absent() {
        let path = scratch().join("lantern").join("settings.toml");
        assert_eq!(load(&path), (Settings::default(), None));
        assert_eq!(
            Settings::default(),
            Settings {
                clipboard_sync: true,
                lantern_keyboard: true
            }
        );
        let handle = SettingsHandle::new(Some(path.clone()));
        assert_eq!(handle.get(), Settings::default());
        assert_eq!(handle.path(), Some(path.as_path()));
        assert!(!path.exists(), "loading must not create the file");
        // No path at all: defaults, and writes are refused.
        let none = SettingsHandle::new(None);
        assert_eq!(none.get(), Settings::default());
        assert!(none.set(Key::ClipboardSync, false).is_err());
        assert_eq!(Key::ClipboardSync.name(), "clipboard_sync");
        assert_eq!(Key::LanternKeyboard.name(), "lantern_keyboard");
    }

    #[test]
    fn parses_both_keys() {
        let (s, warnings) =
            parse("clipboard_sync = false\nlantern_keyboard = false\nfuture = 3\n").unwrap();
        assert_eq!(
            s,
            Settings {
                clipboard_sync: false,
                lantern_keyboard: false
            }
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let (s, _) = parse("lantern_keyboard = false\n").unwrap();
        assert_eq!(
            s,
            Settings {
                clipboard_sync: true,
                lantern_keyboard: false
            }
        );
        assert_eq!(parse("").unwrap(), (Settings::default(), vec![]));
    }

    #[test]
    fn bad_value_type_uses_default() {
        let (s, warnings) = parse("clipboard_sync = \"no\"\nlantern_keyboard = false\n").unwrap();
        assert!(s.clipboard_sync);
        assert!(!s.lantern_keyboard);
        assert_eq!(
            warnings,
            vec!["clipboard_sync must be true or false; using true".to_owned()]
        );
    }

    #[test]
    fn unparseable_file_is_not_overwritten() {
        let dir = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        let bytes = b"clipboard_sync = \n";
        std::fs::write(&path, bytes).unwrap();
        let (s, err) = load(&path);
        assert_eq!(s, Settings::default());
        let err = err.expect("an unparseable file is an error");
        assert!(err.contains("line 1"), "{err}");
        let refused = set(&path, Key::ClipboardSync, false).unwrap_err();
        let msg = format!("{refused:#}");
        assert!(
            msg.starts_with("settings.toml has an error: ") && msg.ends_with("; fix it first"),
            "{msg}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(ensure_file(&path).is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn set_keeps_unknown_keys() {
        let dir = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, "# my note\nfuture = 3\nlantern_keyboard = false\n").unwrap();
        let s = set(&path, Key::ClipboardSync, false).unwrap();
        assert_eq!(
            s,
            Settings {
                clipboard_sync: false,
                lantern_keyboard: false
            }
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        let table = text.parse::<Table>().unwrap();
        assert_eq!(table.get("future"), Some(&Value::Integer(3)));
        assert_eq!(table.get("clipboard_sync"), Some(&Value::Boolean(false)));
        assert_eq!(table.get("lantern_keyboard"), Some(&Value::Boolean(false)));
        assert_eq!(load(&path), (s, None));
    }

    #[test]
    fn set_creates_dir_and_file_0600() {
        let root = scratch();
        let path = root.join("lantern").join("settings.toml");
        let handle = SettingsHandle::new(Some(path.clone()));
        let mut rx = handle.subscribe();
        handle.set(Key::LanternKeyboard, false).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(rx.has_changed().unwrap());
        assert!(!rx.borrow_and_update().lantern_keyboard);
        assert!(!handle.get().lantern_keyboard);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        // No temp file is left next to it.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("settings.toml")]);
    }

    #[test]
    fn ensure_file_writes_header_and_defaults_once() {
        let path = scratch().join("lantern").join("settings.toml");
        ensure_file(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        assert_eq!(parse(&text).unwrap(), (Settings::default(), vec![]));
        assert!(text.contains("clipboard_sync = true"), "{text}");
        assert!(text.contains("lantern_keyboard = true"), "{text}");
        assert_eq!(mode(&path), 0o600);
        set(&path, Key::ClipboardSync, false).unwrap();
        ensure_file(&path).unwrap();
        assert!(
            !load(&path).0.clipboard_sync,
            "ensure_file rewrote an existing file"
        );
    }

    #[tokio::test]
    async fn poll_picks_up_hand_edit() {
        let dir = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, "clipboard_sync = true\n").unwrap();
        let handle = SettingsHandle::new(Some(path.clone()));
        let mut rx = handle.subscribe();
        let poll = handle.spawn_poll();
        // A different length, so the change is seen even with a coarse mtime.
        std::fs::write(&path, "clipboard_sync = false\n").unwrap();
        tokio::time::timeout(Duration::from_secs(3), rx.changed())
            .await
            .expect("the hand edit was not picked up within 3 s")
            .unwrap();
        assert!(!rx.borrow().clipboard_sync);
        assert!(!handle.get().clipboard_sync);
        poll.abort();
    }

    #[tokio::test]
    async fn error_is_published_and_cleared() {
        let dir = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, "clipboard_sync = \n").unwrap();
        let handle = SettingsHandle::new(Some(path.clone()));
        assert!(handle.error().unwrap().contains("line 1"));
        let mut errors = handle.subscribe_error();
        assert!(errors.borrow_and_update().is_some());
        // The tray's set is refused and the error stays.
        assert!(handle.set(Key::ClipboardSync, false).is_err());
        assert!(handle.error().is_some());
        let poll = handle.spawn_poll();
        std::fs::write(&path, "clipboard_sync = false\n").unwrap();
        tokio::time::timeout(Duration::from_secs(3), errors.changed())
            .await
            .expect("the fixed file was not picked up within 3 s")
            .unwrap();
        assert_eq!(*errors.borrow(), None);
        assert_eq!(handle.error(), None);
        assert!(!handle.get().clipboard_sync);
        // Broken again by hand: the error comes back.
        std::fs::write(&path, "clipboard_sync = = =\n").unwrap();
        tokio::time::timeout(Duration::from_secs(3), errors.changed())
            .await
            .expect("the broken file was not picked up within 3 s")
            .unwrap();
        assert!(handle.error().is_some());
        poll.abort();
    }

    #[test]
    fn concurrent_sets_all_land() {
        let path = scratch().join("lantern").join("settings.toml");
        let handle = SettingsHandle::new(Some(path.clone()));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let handle = handle.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let key = if n % 2 == 0 {
                            Key::ClipboardSync
                        } else {
                            Key::LanternKeyboard
                        };
                        handle.set(key, n % 4 < 2).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(load(&path).1.is_none());
    }
}
