//! The phone's launcher apps as KDE menu entries.
//!
//! lanternd writes one `.desktop` file per launcher app and Android user into
//! `$XDG_DATA_HOME/applications` (named by [`app_id`], so the window's app_id matches
//! `StartupWMClass`), its icon into `$XDG_DATA_HOME/lantern/icons`, and a merged menu
//! (`lantern.menu` + `lantern-phone.directory`) that gathers `Categories=X-Lantern` into a
//! "Phone" submenu. Every file Lantern writes carries `X-Lantern-Serial=`; a file without it is
//! never modified or deleted. After changes, `kbuildsycoca6` runs once, 1 s after the last one.
//!
//! Full syncs (`begin_sync` … `end_sync(count)`) prune entries the phone no longer lists, but only
//! when every announced app arrived, and only for the Android users that appeared in the sync
//! (a paused work profile sends nothing and keeps its entries); entries of other serials go too.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub use crate::window::identity::app_id;
use crate::window::identity::{profile_suffix, MAX_LABEL, MAX_USER};

/// Where the catalogue lives. See [`Dirs::from_env`].
#[derive(Debug, Clone)]
pub struct Dirs {
    pub applications: PathBuf,
    pub icons: PathBuf,
    pub menus: PathBuf,
    pub directories: PathBuf,
    /// The `lantern` binary the entries run.
    pub exe: PathBuf,
}

/// An XDG base dir from the environment: only an absolute path counts (XDG Base Directory spec).
fn xdg(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

impl Dirs {
    /// `$XDG_DATA_HOME` (default `~/.local/share`), `$XDG_CONFIG_HOME` (default `~/.config`)
    /// and the running executable.
    pub fn from_env() -> Result<Dirs> {
        let home = dirs::home_dir().context("no home directory")?;
        let exe = std::env::current_exe().context("locate the lantern executable")?;
        // A binary replaced while running reads back as "<path> (deleted)".
        let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            Some(path) => PathBuf::from(path),
            None => exe,
        };
        Ok(Self::resolve(
            xdg("XDG_DATA_HOME"),
            xdg("XDG_CONFIG_HOME"),
            &home,
            exe,
        ))
    }

    /// The layout under the given base dirs; `None` (or a relative path) means the default.
    pub fn resolve(
        data: Option<PathBuf>,
        config: Option<PathBuf>,
        home: &Path,
        exe: PathBuf,
    ) -> Dirs {
        let data = data
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"));
        let config = config
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        Dirs {
            applications: data.join("applications"),
            icons: data.join("lantern").join("icons"),
            menus: config.join("menus").join("applications-merged"),
            directories: data.join("desktop-directories"),
            exe,
        }
    }

    fn desktop_path(&self, pkg: &str, user: u32) -> PathBuf {
        self.applications
            .join(format!("{}.desktop", app_id(pkg, user)))
    }

    fn icon_path(&self, pkg: &str, user: u32) -> PathBuf {
        self.icons.join(format!("{}.png", app_id(pkg, user)))
    }
}

/// One launcher app of one Android user, as the phone reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub pkg: String,
    pub user: u32,
    pub label: String,
    pub work: bool,
    /// PNG bytes, or empty for no icon.
    pub icon_png: Vec<u8>,
}

impl AppEntry {
    /// From an `app` message: `icon` is base64; bad base64 or non-PNG bytes mean no icon.
    pub fn from_message(pkg: String, user: u32, label: String, work: bool, icon: &str) -> Self {
        use base64::Engine as _;
        let icon_png = base64::engine::general_purpose::STANDARD
            .decode(icon)
            .ok()
            .filter(|b| is_png(b))
            .unwrap_or_default();
        Self {
            pkg,
            user,
            label,
            work,
            icon_png,
        }
    }
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(PNG_SIGNATURE)
}

/// Android package names use only `[A-Za-z0-9_.]`; anything else (or a name too long for a file
/// name) is refused before it reaches a path or an Exec line.
fn check_pkg(pkg: &str) -> Result<()> {
    if pkg.is_empty()
        || pkg.len() > 200
        || !pkg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        bail!("{pkg:?} is not an Android package name");
    }
    Ok(())
}

fn check_user(user: u32) -> Result<()> {
    if user > MAX_USER {
        bail!("{user} is not an Android user id");
    }
    Ok(())
}

/// Desktop Entry string escaping (`\\`, `\n`, `\t`, `\r`, leading `\s`), other control characters
/// dropped, at most `max` output chars, never splitting an escape.
fn escape_value(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let piece: &str = match c {
            '\\' => "\\\\",
            '\n' => "\\n",
            '\t' => "\\t",
            '\r' => "\\r",
            ' ' if out.is_empty() => "\\s",
            c if c.is_control() => continue,
            _ => {
                if n + 1 > max {
                    break;
                }
                out.push(c);
                n += 1;
                continue;
            }
        };
        if n + piece.len() > max {
            break;
        }
        out.push_str(piece);
        n += piece.len();
    }
    out
}

fn unescape_value(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('s') => out.push(' '),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The exe as one Exec argument: double-quoted with `"`, `` ` ``, `$`, `\` backslash-escaped,
/// `%` doubled, then string-escaped (which doubles those backslashes), per the Desktop Entry spec.
fn exec_quote(exe: &Path) -> String {
    let mut quoted = String::from("\"");
    for c in exe.to_string_lossy().chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    escape_value(&quoted, usize::MAX)
}

/// `Name=` value: the label escaped, or the package when the label is empty (or only control
/// characters), plus the profile suffix, at most [`MAX_LABEL`] chars in all.
fn entry_name(e: &AppEntry) -> String {
    let suffix = profile_suffix(e.user, e.work);
    let room = MAX_LABEL.saturating_sub(suffix.chars().count());
    let mut name = escape_value(&e.label, room);
    if name.is_empty() {
        name = escape_value(&e.pkg, room);
    }
    name + &suffix
}

/// The `.desktop` file for `e` on phone `serial`. `icon` is the absolute PNG path, if any.
pub fn desktop_file(e: &AppEntry, serial: &str, exe: &Path, icon: Option<&Path>) -> String {
    let user_arg = match e.user {
        0 => String::new(),
        n => format!(" --user {n}"),
    };
    let mut f = String::from("[Desktop Entry]\nType=Application\nVersion=1.5\n");
    f += &format!("Name={}\n", entry_name(e));
    f += &format!("Exec={} open{user_arg} {}\n", exec_quote(exe), e.pkg);
    if let Some(icon) = icon {
        f += &format!(
            "Icon={}\n",
            escape_value(&icon.to_string_lossy(), usize::MAX)
        );
    }
    f += "Categories=X-Lantern;\n";
    f += &format!("StartupWMClass={}\n", app_id(&e.pkg, e.user));
    f += "StartupNotify=true\n";
    f += &format!("X-Lantern-Serial={}\n", escape_value(serial, MAX_LABEL));
    f += &format!("X-Lantern-Package={}\n", e.pkg);
    f += &format!("X-Lantern-User={}\n", e.user);
    f
}

/// What a Lantern-written `.desktop` file says about itself.
#[derive(Debug)]
struct Owned {
    serial: String,
    pkg: Option<String>,
    user: Option<u32>,
    name: Option<String>,
}

fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// `Some` if `path` is a Lantern entry (has `X-Lantern-Serial=`), `None` if it is someone
/// else's or absent.
fn read_owned(path: &Path) -> Result<Option<Owned>> {
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let Some(serial) = value(&text, "X-Lantern-Serial") else {
        return Ok(None);
    };
    Ok(Some(Owned {
        serial: unescape_value(serial),
        pkg: value(&text, "X-Lantern-Package").map(str::to_owned),
        user: value(&text, "X-Lantern-User").and_then(|u| u.parse().ok()),
        name: value(&text, "Name").map(unescape_value),
    }))
}

/// Writes `bytes` to `path` atomically (temp file + rename) unless it already holds them.
/// Returns whether it wrote.
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool> {
    match std::fs::read(path) {
        Ok(old) if old == bytes => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename to {}", path.display()));
    }
    Ok(true)
}

/// Removes `path`; returns whether it existed.
fn remove_file(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

const MENU_FILE: &str = "lantern.menu";
const DIRECTORY_FILE: &str = "lantern-phone.directory";

const MENU: &str = "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\" \
\"http://www.freedesktop.org/standards/menu-spec/menu-1.0.dtd\">\n\
<Menu><Name>Applications</Name><Menu><Name>Phone</Name>\
<Directory>lantern-phone.directory</Directory>\
<Include><Category>X-Lantern</Category></Include></Menu></Menu>\n";

const DIRECTORY: &str = "[Desktop Entry]\nType=Directory\nName=Phone\nIcon=smartphone\n";

/// Writes the "Phone" submenu files unless they already hold the same bytes. Returns whether
/// anything changed.
pub fn ensure_menu(dirs: &Dirs) -> Result<bool> {
    let menu = write_if_changed(&dirs.menus.join(MENU_FILE), MENU.as_bytes())?;
    let dir = write_if_changed(&dirs.directories.join(DIRECTORY_FILE), DIRECTORY.as_bytes())?;
    Ok(menu || dir)
}

/// Every `lantern.*.desktop` in `applications` that Lantern owns.
fn owned_entries(dirs: &Dirs) -> Result<Vec<(PathBuf, Owned)>> {
    let rd = match std::fs::read_dir(&dirs.applications) {
        Ok(rd) => rd,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("list {}", dirs.applications.display())),
    };
    let mut out = Vec::new();
    for item in rd {
        let path = item?.path();
        let ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("lantern.") && n.ends_with(".desktop"));
        if !ours {
            continue;
        }
        match read_owned(&path) {
            Ok(Some(owned)) => out.push((path, owned)),
            Ok(None) => {}
            Err(error) => tracing::warn!(error = format!("{error:#}"), "skipping catalogue entry"),
        }
    }
    Ok(out)
}

/// Runs a rebuild once, `delay` after the last [`Rebuilder::changed`]. A pending rebuild runs
/// immediately when the rebuilder is dropped.
struct Rebuilder {
    tx: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Rebuilder {
    fn spawn(rebuild: Rebuild, delay: Duration) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("sycoca".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    loop {
                        match rx.recv_timeout(delay) {
                            Ok(()) => continue,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                rebuild();
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                rebuild();
                                return;
                            }
                        }
                    }
                }
            })
            .ok();
        Self {
            tx: Some(tx),
            thread,
        }
    }

    fn changed(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(());
        }
    }
}

impl Drop for Rebuilder {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What to run after the catalogue changed (`kbuildsycoca6` by default).
pub type Rebuild = Arc<dyn Fn() + Send + Sync>;

/// Spawns `kbuildsycoca6` and reaps it on a side thread; never waits for it.
pub fn run_kbuildsycoca() {
    use std::process::{Command, Stdio};
    match Command::new("kbuildsycoca6")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            tracing::debug!("kbuildsycoca6 started");
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => tracing::warn!(%status, "kbuildsycoca6 failed"),
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "kbuildsycoca6 wait failed"),
            });
        }
        Err(error) => tracing::warn!(%error, "could not run kbuildsycoca6"),
    }
}

const REBUILD_DELAY: Duration = Duration::from_secs(1);

/// The menu entries of the phone's launcher apps. See the module docs.
pub struct Catalog {
    dirs: Dirs,
    /// The phone the current sync window belongs to.
    serial: Option<String>,
    /// `upsert`/`remove` calls since `begin_sync`.
    seen: u32,
    /// Android users with at least one upsert in this window.
    users: HashSet<u32>,
    /// (pkg, user) upserted in this window.
    keep: HashSet<(String, u32)>,
    rebuild: Rebuilder,
}

impl Catalog {
    /// A catalogue that runs `kbuildsycoca6` after changes.
    pub fn new(dirs: Dirs) -> Self {
        Self::with_rebuild(dirs, Arc::new(run_kbuildsycoca))
    }

    /// A catalogue that runs `rebuild` (debounced) after changes.
    pub fn with_rebuild(dirs: Dirs, rebuild: Rebuild) -> Self {
        let cat = Self {
            dirs,
            serial: None,
            seen: 0,
            users: HashSet::new(),
            keep: HashSet::new(),
            rebuild: Rebuilder::spawn(rebuild, REBUILD_DELAY),
        };
        match ensure_menu(&cat.dirs) {
            Ok(true) => cat.rebuild.changed(),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                error = format!("{error:#}"),
                "could not write the Phone menu"
            ),
        }
        cat
    }

    pub fn dirs(&self) -> &Dirs {
        &self.dirs
    }

    /// Starts counting a full catalogue from `serial`.
    pub fn begin_sync(&mut self, serial: &str) {
        self.serial = Some(serial.to_owned());
        self.seen = 0;
        self.users.clear();
        self.keep.clear();
    }

    /// Writes (or refreshes) `e`'s icon and menu entry. Counts toward the sync window even when
    /// it fails, since the phone counted it.
    pub fn upsert(&mut self, e: &AppEntry) -> Result<()> {
        self.seen = self.seen.saturating_add(1);
        check_pkg(&e.pkg)?;
        check_user(e.user)?;
        self.users.insert(e.user);
        self.keep.insert((e.pkg.clone(), e.user));
        let serial = self
            .serial
            .clone()
            .context("no phone session (begin_sync first)")?;
        let path = self.dirs.desktop_path(&e.pkg, e.user);
        if path.exists() && read_owned(&path)?.is_none() {
            bail!(
                "{} is not a Lantern entry; leaving it alone",
                path.display()
            );
        }
        let icon_path = self.dirs.icon_path(&e.pkg, e.user);
        let mut changed = false;
        let icon = if is_png(&e.icon_png) {
            changed |= write_if_changed(&icon_path, &e.icon_png)?;
            Some(icon_path.as_path())
        } else {
            changed |= remove_file(&icon_path)?;
            None
        };
        let text = desktop_file(e, &serial, &self.dirs.exe, icon);
        changed |= write_if_changed(&path, text.as_bytes())?;
        if changed {
            self.rebuild.changed();
        }
        Ok(())
    }

    /// Removes `pkg`'s entry for `user` (a no-op if there is none or it is not Lantern's).
    pub fn remove(&mut self, pkg: &str, user: u32) -> Result<()> {
        self.seen = self.seen.saturating_add(1);
        check_pkg(pkg)?;
        check_user(user)?;
        self.keep.remove(&(pkg.to_owned(), user));
        let path = self.dirs.desktop_path(pkg, user);
        let mut changed = false;
        if read_owned(&path)?.is_some() {
            changed |= remove_file(&path)?;
            changed |= remove_file(&self.dirs.icon_path(pkg, user))?;
        }
        if changed {
            self.rebuild.changed();
        }
        Ok(())
    }

    fn remove_entry(&self, path: &Path) -> Result<()> {
        remove_file(path)?;
        if let Some(stem) = path.file_stem() {
            let icon = self
                .dirs
                .icons
                .join(format!("{}.png", stem.to_string_lossy()));
            remove_file(&icon)?;
        }
        Ok(())
    }

    /// Ends the sync window. If `count` matches what arrived, prunes entries of other serials
    /// and, for each user that appeared, the apps it no longer lists. Returns how many entries
    /// were removed.
    pub fn end_sync(&mut self, count: u32) -> Result<usize> {
        let seen = self.seen;
        let users = std::mem::take(&mut self.users);
        let keep = std::mem::take(&mut self.keep);
        self.seen = 0;
        let Some(serial) = self.serial.clone() else {
            return Ok(0);
        };
        if count != seen {
            tracing::info!("partial catalogue (got {seen} of {count}); not pruning");
            return Ok(0);
        }
        let mut removed = 0;
        for (path, owned) in owned_entries(&self.dirs)? {
            let stale = if owned.serial != serial {
                true
            } else {
                match (owned.pkg, owned.user) {
                    (Some(pkg), Some(user)) => {
                        users.contains(&user) && !keep.contains(&(pkg, user))
                    }
                    _ => false,
                }
            };
            if stale {
                self.remove_entry(&path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::info!(removed, "pruned catalogue entries");
            self.rebuild.changed();
        }
        Ok(removed)
    }

    /// Removes every entry of `serial`; with no Lantern entries left, the Phone menu goes too.
    /// Returns how many entries were removed.
    pub fn remove_serial(&mut self, serial: &str) -> Result<usize> {
        let mut removed = 0;
        let mut left = 0;
        for (path, owned) in owned_entries(&self.dirs)? {
            if owned.serial == serial {
                self.remove_entry(&path)?;
                removed += 1;
            } else {
                left += 1;
            }
        }
        let mut changed = removed > 0;
        if left == 0 {
            changed |= remove_file(&self.dirs.menus.join(MENU_FILE))?;
            changed |= remove_file(&self.dirs.directories.join(DIRECTORY_FILE))?;
        }
        if changed {
            self.rebuild.changed();
        }
        Ok(removed)
    }
}

/// The "Connect your phone" notification for `lantern open [--user N] <pkg>` without a phone:
/// (summary, body, icon). The body names the app by its menu entry's Name when there is one.
pub fn connect_notice(dirs: &Dirs, pkg: &str, user: u32) -> (String, String, Option<PathBuf>) {
    let (name, icon) = if check_pkg(pkg).is_ok() {
        let name = read_owned(&dirs.desktop_path(pkg, user))
            .ok()
            .flatten()
            .and_then(|o| o.name)
            .filter(|n| !n.is_empty());
        let icon = Some(dirs.icon_path(pkg, user)).filter(|p| p.is_file());
        (name, icon)
    } else {
        (None, None)
    };
    let name = name.unwrap_or_else(|| pkg.to_owned());
    (
        "Connect your phone".to_owned(),
        format!("{name} opens when your phone is connected by USB."),
        icon,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A private temp tree standing in for `$XDG_DATA_HOME`/`$XDG_CONFIG_HOME`.
    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp() -> (Tmp, Dirs) {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("lantern-cat-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dirs = Dirs {
            applications: root.join("data/applications"),
            icons: root.join("data/lantern/icons"),
            menus: root.join("config/menus/applications-merged"),
            directories: root.join("data/desktop-directories"),
            exe: PathBuf::from("/usr/bin/lantern"),
        };
        (Tmp(root), dirs)
    }

    fn quiet(dirs: Dirs) -> (Catalog, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let cat = Catalog::with_rebuild(
            dirs,
            Arc::new(move || {
                r.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (cat, runs)
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRfake";

    fn entry(pkg: &str, user: u32) -> AppEntry {
        AppEntry {
            pkg: pkg.into(),
            user,
            label: format!("Label {pkg}"),
            work: user == 10,
            icon_png: PNG.to_vec(),
        }
    }

    fn desktop(dirs: &Dirs, pkg: &str, user: u32) -> PathBuf {
        dirs.applications
            .join(format!("{}.desktop", app_id(pkg, user)))
    }

    fn line<'a>(file: &'a str, key: &str) -> Vec<&'a str> {
        file.lines()
            .filter_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
            .collect()
    }

    #[test]
    fn desktop_file_escapes_label() {
        let mut label = "A\nB=C;%x \"q\" 🙂".to_owned();
        label.push_str(&"x".repeat(10_000));
        let e = AppEntry {
            label,
            ..entry("com.a", 0)
        };
        let f = desktop_file(&e, "ser", Path::new("/usr/bin/lantern"), None);
        let name = line(&f, "Name");
        assert_eq!(name.len(), 1, "{f}");
        assert!(name[0].starts_with("A\\nB=C;%x \"q\" 🙂x"), "{}", name[0]);
        assert!(
            name[0].chars().count() <= 200,
            "{}",
            name[0].chars().count()
        );
        assert!(!f.contains("A\nB"), "raw newline in {f}");
        let exec = line(&f, "Exec");
        assert_eq!(exec, ["\"/usr/bin/lantern\" open com.a"]);
        assert!(!f.contains("Icon="));
        for want in [
            "[Desktop Entry]",
            "Type=Application",
            "Version=1.5",
            "Categories=X-Lantern;",
            "StartupWMClass=lantern.com.a",
            "StartupNotify=true",
            "X-Lantern-Serial=ser",
            "X-Lantern-Package=com.a",
            "X-Lantern-User=0",
        ] {
            assert!(f.lines().any(|l| l == want), "missing {want} in {f}");
        }
        assert!(f.starts_with("[Desktop Entry]\n"));
    }

    #[test]
    fn label_escapes_backslash_tab_cr_and_drops_controls() {
        let e = AppEntry {
            label: "a\\b\tc\rd\u{7}e\u{0}f".into(),
            ..entry("com.a", 0)
        };
        let f = desktop_file(&e, "s", Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["a\\\\b\\tc\\rdef"]);
        // Truncation never splits an escape: 199 x then a backslash gives no dangling `\`.
        let e = AppEntry {
            label: format!("{}\\", "x".repeat(199)),
            ..entry("com.a", 0)
        };
        let f = desktop_file(&e, "s", Path::new("/l"), None);
        let name = line(&f, "Name")[0];
        assert!(name.chars().count() <= 200);
        assert!(!name.ends_with('\\') || name.ends_with("\\\\"), "{name}");
    }

    #[test]
    fn desktop_file_work_and_user_suffix() {
        let work = AppEntry {
            label: "Files".into(),
            work: true,
            ..entry("com.f", 10)
        };
        let f = desktop_file(&work, "s", Path::new("/l"), Some(Path::new("/i/x.png")));
        assert_eq!(line(&f, "Name"), ["Files (work)"]);
        assert_eq!(line(&f, "Exec"), ["\"/l\" open --user 10 com.f"]);
        assert_eq!(line(&f, "Icon"), ["/i/x.png"]);
        assert_eq!(line(&f, "StartupWMClass"), ["lantern.com.f-u10"]);
        assert_eq!(line(&f, "X-Lantern-User"), ["10"]);
        let other = AppEntry {
            label: "Files".into(),
            work: false,
            ..entry("com.f", 11)
        };
        let f = desktop_file(&other, "s", Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["Files (user 11)"]);
        let owner = AppEntry {
            label: "Files".into(),
            work: false,
            ..entry("com.f", 0)
        };
        assert_eq!(
            line(&desktop_file(&owner, "s", Path::new("/l"), None), "Name"),
            ["Files"]
        );
        // A long label still leaves room for the suffix.
        let long = AppEntry {
            label: "y".repeat(500),
            work: true,
            ..entry("com.f", 10)
        };
        let f = desktop_file(&long, "s", Path::new("/l"), None);
        let name = line(&f, "Name")[0];
        assert!(
            name.ends_with(" (work)") && name.chars().count() <= 200,
            "{name}"
        );
    }

    #[test]
    fn empty_label_falls_back_to_package() {
        let e = AppEntry {
            label: String::new(),
            work: true,
            ..entry("com.f", 10)
        };
        let f = desktop_file(&e, "s", Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["com.f (work)"]);
        let e = AppEntry {
            label: "\u{1}\u{2}".into(),
            work: false,
            ..entry("com.f", 0)
        };
        let f = desktop_file(&e, "s", Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["com.f"]);
    }

    #[test]
    fn exec_quotes_exe_with_space() {
        let f = desktop_file(
            &entry("com.a", 0),
            "s",
            Path::new("/home/a b/lantern"),
            None,
        );
        assert_eq!(line(&f, "Exec"), ["\"/home/a b/lantern\" open com.a"]);
        // `"`, `` ` ``, `$`, `\` are backslash-escaped inside the quotes, then the string
        // escape doubles every backslash; `%` is doubled.
        let f = desktop_file(
            &entry("com.a", 0),
            "s",
            Path::new("/o\"d`d$d\\d%d/lantern"),
            None,
        );
        assert_eq!(
            line(&f, "Exec"),
            ["\"/o\\\\\"d\\\\`d\\\\$d\\\\\\\\d%%d/lantern\" open com.a"]
        );
    }

    #[test]
    fn rejects_bad_package() {
        let (_t, dirs) = tmp();
        let apps = dirs.applications.clone();
        let icons = dirs.icons.clone();
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("s");
        for bad in ["com.x;rm -rf", "", "../x", "com/x", "com.x-u1"] {
            assert!(cat.upsert(&entry(bad, 0)).is_err(), "{bad:?} accepted");
            assert!(cat.remove(bad, 0).is_err(), "{bad:?} remove accepted");
        }
        let count = |d: &Path| std::fs::read_dir(d).map(|r| r.count()).unwrap_or(0);
        assert_eq!(count(&apps), 0);
        assert_eq!(count(&icons), 0);
    }

    #[test]
    fn upsert_writes_icon_and_entry() {
        let (_t, dirs) = tmp();
        let path = desktop(&dirs, "com.a", 0);
        let icon = dirs.icons.join("lantern.com.a.png");
        let apps = dirs.applications.clone();
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("ser");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(std::fs::read(&icon).unwrap(), PNG);
        let f = std::fs::read_to_string(&path).unwrap();
        assert_eq!(line(&f, "Icon"), [icon.to_str().unwrap()]);
        assert_eq!(line(&f, "Name"), ["Label com.a"]);
        // No temp files are left behind.
        assert_eq!(std::fs::read_dir(&apps).unwrap().count(), 1);
        // A non-PNG icon means no icon: the old one is removed and Icon= dropped.
        cat.upsert(&AppEntry {
            icon_png: b"GIF89a".to_vec(),
            ..entry("com.a", 0)
        })
        .unwrap();
        assert!(!icon.exists());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("Icon="));
    }

    #[test]
    fn upsert_skips_unchanged_bytes() {
        let (_t, dirs) = tmp();
        let path = desktop(&dirs, "com.a", 0);
        let icon = dirs.icons.join("lantern.com.a.png");
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        for p in [&path, &icon] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        cat.upsert(&entry("com.a", 0)).unwrap();
        for p in [&path, &icon] {
            assert_eq!(
                std::fs::metadata(p).unwrap().modified().unwrap(),
                old,
                "{p:?}"
            );
        }
        cat.upsert(&AppEntry {
            label: "New".into(),
            ..entry("com.a", 0)
        })
        .unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
        assert_eq!(std::fs::metadata(&icon).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn full_sync_prunes_uninstalled() {
        let (_t, dirs) = tmp();
        let (a, b) = (desktop(&dirs, "com.a", 0), desktop(&dirs, "com.b", 0));
        let b_icon = dirs.icons.join("lantern.com.b.png");
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.upsert(&entry("com.b", 0)).unwrap();
        assert_eq!(cat.end_sync(2).unwrap(), 0);
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(cat.end_sync(1).unwrap(), 1);
        assert!(a.exists());
        assert!(!b.exists());
        assert!(!b_icon.exists());
    }

    #[test]
    fn full_sync_prunes_other_serial() {
        let (_t, dirs) = tmp();
        let (a, b) = (desktop(&dirs, "com.a", 0), desktop(&dirs, "com.b", 10));
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("old");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.upsert(&entry("com.b", 10)).unwrap();
        assert_eq!(cat.end_sync(2).unwrap(), 0);
        cat.begin_sync("new");
        cat.upsert(&entry("com.c", 0)).unwrap();
        assert_eq!(cat.end_sync(1).unwrap(), 2);
        assert!(!a.exists() && !b.exists());
    }

    #[test]
    fn full_sync_keeps_absent_profile() {
        let (_t, dirs) = tmp();
        let (a, w) = (desktop(&dirs, "com.a", 0), desktop(&dirs, "com.w", 10));
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.upsert(&entry("com.w", 10)).unwrap();
        cat.end_sync(2).unwrap();
        // The work profile is paused: the next full sync has only user 0's apps.
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(cat.end_sync(1).unwrap(), 0);
        assert!(a.exists() && w.exists());
    }

    #[test]
    fn partial_sync_prunes_nothing() {
        let (_t, dirs) = tmp();
        let b = desktop(&dirs, "com.b", 0);
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.upsert(&entry("com.b", 0)).unwrap();
        cat.end_sync(2).unwrap();
        cat.begin_sync("other");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(cat.end_sync(5).unwrap(), 0);
        assert!(b.exists());
        // A live removal counts toward the window, so the sync no longer matches.
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.remove("com.zz", 0).unwrap();
        assert_eq!(cat.end_sync(1).unwrap(), 0);
        assert!(b.exists());
    }

    #[test]
    fn remove_deletes_entry_and_icon() {
        let (_t, dirs) = tmp();
        let a = desktop(&dirs, "com.a", 10);
        let icon = dirs.icons.join("lantern.com.a-u10.png");
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 10)).unwrap();
        cat.remove("com.a", 10).unwrap();
        assert!(!a.exists() && !icon.exists());
        cat.remove("com.a", 10).unwrap();
    }

    #[test]
    fn never_touches_foreign_desktop_files() {
        let (_t, dirs) = tmp();
        std::fs::create_dir_all(&dirs.applications).unwrap();
        let foo = dirs.applications.join("lantern.foo.desktop");
        let mine = "[Desktop Entry]\nType=Application\nName=Mine\nExec=foo\n";
        std::fs::write(&foo, mine).unwrap();
        let other = dirs.applications.join("org.kde.dolphin.desktop");
        std::fs::write(&other, "[Desktop Entry]\nX-Lantern-Serial=s\n").unwrap();
        let (mut cat, _) = quiet(dirs);
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        // Same app_id as the hand-made file: refused, file untouched.
        assert!(cat.upsert(&entry("foo", 0)).is_err());
        // A full sync (the refused app counts: the phone sent it) prunes nothing foreign.
        assert_eq!(cat.end_sync(2).unwrap(), 0);
        assert!(cat.remove("foo", 0).is_ok());
        assert_eq!(cat.remove_serial("s").unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&foo).unwrap(), mine);
        assert!(other.exists());
    }

    #[test]
    fn remove_serial_removes_only_that_serial() {
        let (_t, dirs) = tmp();
        let (a, b) = (desktop(&dirs, "com.a", 0), desktop(&dirs, "com.b", 0));
        let menu = dirs.menus.join("lantern.menu");
        let dir = dirs.directories.join("lantern-phone.directory");
        let (mut cat, _) = quiet(dirs);
        assert!(menu.exists() && dir.exists());
        cat.begin_sync("one");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.begin_sync("two");
        cat.upsert(&entry("com.b", 0)).unwrap();
        assert_eq!(cat.remove_serial("one").unwrap(), 1);
        assert!(!a.exists() && b.exists());
        assert!(menu.exists(), "other entries remain: the menu stays");
        assert_eq!(cat.remove_serial("two").unwrap(), 1);
        assert!(
            !menu.exists() && !dir.exists(),
            "no entries left: menu removed"
        );
    }

    #[test]
    fn menu_and_directory_files() {
        let (_t, dirs) = tmp();
        let menu = dirs.menus.join("lantern.menu");
        let dir = dirs.directories.join("lantern-phone.directory");
        let _cat = quiet(dirs);
        let m = std::fs::read_to_string(&menu).unwrap();
        for want in [
            "<Menu>",
            "<Name>Applications</Name>",
            "<Name>Phone</Name>",
            "<Directory>lantern-phone.directory</Directory>",
            "<Include><Category>X-Lantern</Category></Include>",
        ] {
            assert!(m.contains(want), "{want} missing in {m}");
        }
        let d = std::fs::read_to_string(&dir).unwrap();
        assert!(
            d.contains("Name=Phone\n") && d.contains("Icon=smartphone\n"),
            "{d}"
        );
        assert!(d.contains("Type=Directory\n"), "{d}");
    }

    #[test]
    fn rebuild_runs_once_after_changes_settle() {
        let (_t, dirs) = tmp();
        let (mut cat, runs) = quiet(dirs);
        // Writing the menu files is a change too.
        cat.begin_sync("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.upsert(&entry("com.b", 0)).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "ran before the debounce");
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // Unchanged bytes are not a change.
        cat.upsert(&entry("com.a", 0)).unwrap();
        std::thread::sleep(Duration::from_millis(1300));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // A pending rebuild runs on drop (e.g. undo-setup exiting).
        cat.remove("com.a", 0).unwrap();
        drop(cat);
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn icon_decodes_base64_png_only() {
        let e = |icon| AppEntry::from_message("com.a".into(), 0, "A".into(), false, icon);
        assert_eq!(e("iVBORw0KGgo=").icon_png, PNG_SIGNATURE);
        assert!(e("").icon_png.is_empty());
        assert!(e("not base64!").icon_png.is_empty());
        assert!(e("R0lGODlh").icon_png.is_empty(), "GIF is not a PNG");
    }

    #[test]
    fn dirs_follow_xdg_with_defaults() {
        let d = Dirs::resolve(
            Some(PathBuf::from("/d")),
            Some(PathBuf::from("/c")),
            Path::new("/h"),
            PathBuf::from("/x"),
        );
        assert_eq!(d.applications, Path::new("/d/applications"));
        assert_eq!(d.icons, Path::new("/d/lantern/icons"));
        assert_eq!(d.menus, Path::new("/c/menus/applications-merged"));
        assert_eq!(d.directories, Path::new("/d/desktop-directories"));
        assert_eq!(d.exe, Path::new("/x"));
        // Unset or relative values fall back to the defaults (XDG Base Directory spec).
        let d = Dirs::resolve(
            None,
            Some(PathBuf::from("rel")),
            Path::new("/h"),
            "/x".into(),
        );
        assert_eq!(d.applications, Path::new("/h/.local/share/applications"));
        assert_eq!(d.menus, Path::new("/h/.config/menus/applications-merged"));
    }

    #[test]
    fn connect_notice_uses_entry_name_and_icon() {
        let (_t, dirs) = tmp();
        let (s, body, icon) = connect_notice(&dirs, "com.a", 0);
        assert_eq!(s, "Connect your phone");
        assert_eq!(body, "com.a opens when your phone is connected by USB.");
        assert_eq!(icon, None);
        let icons = dirs.icons.clone();
        let (mut cat, _) = quiet(Dirs { ..dirs });
        cat.begin_sync("s");
        cat.upsert(&AppEntry {
            label: "Sett\\ings".into(),
            ..entry("com.a", 0)
        })
        .unwrap();
        let dirs = cat.dirs();
        let (_, body, icon) = connect_notice(dirs, "com.a", 0);
        assert_eq!(
            body,
            "Sett\\ings opens when your phone is connected by USB."
        );
        assert_eq!(icon, Some(icons.join("lantern.com.a.png")));
    }
}
