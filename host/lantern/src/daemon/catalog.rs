//! The phone's launcher apps as KDE menu entries.
//!
//! lanternd writes one `.desktop` file per launcher app and Android user into
//! `$XDG_DATA_HOME/applications` (named by [`app_id`], so the window's app_id matches
//! `StartupWMClass`), its icon into `$XDG_DATA_HOME/lantern/icons`, and a merged menu
//! (`lantern.menu` + `lantern-phone.directory`) that gathers `Categories=X-Lantern` into a
//! "Phone" submenu. Every file Lantern writes carries `X-Lantern-Serial=`; a file without it is
//! never modified or deleted. After changes, `kbuildsycoca6` runs once, 1 s after the last one.
//!
//! One entry serves every phone that has the app: `X-Lantern-Serials=` lists them (a Plan 2 file
//! without it lists its `X-Lantern-Serial`). A phone's session only ever adds or removes its own
//! serial; the file (and icon) goes when the list is empty. Full syncs (`start_session` …
//! `end_sync(count)`) take the phone off entries it no longer lists, but only when every
//! announced app arrived, and only for the Android users that appeared in the sync (a paused work
//! profile sends nothing and keeps its entries). Other phones' entries are never touched.

use std::collections::{BTreeSet, HashSet};
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

/// `X-Lantern-Serials=` value: each serial string-escaped with `;` as `\;`, each followed by `;`,
/// sorted and deduplicated.
fn serials_value(serials: &[String]) -> String {
    let set: BTreeSet<&String> = serials.iter().collect();
    set.into_iter()
        .map(|s| escape_value(s, MAX_LABEL).replace(';', "\\;") + ";")
        .collect()
}

/// Splits an `X-Lantern-Serials=` value at its unescaped `;` and unescapes each serial.
fn parse_serials(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut piece = String::new();
    let mut it = value.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                piece.push(c);
                if let Some(next) = it.next() {
                    piece.push(next);
                }
            }
            ';' => out.push(unescape_value(&std::mem::take(&mut piece))),
            c => piece.push(c),
        }
    }
    if !piece.is_empty() {
        out.push(unescape_value(&piece));
    }
    out.retain(|s| !s.is_empty());
    out.sort();
    out.dedup();
    out
}

/// The `.desktop` file for `e`, written by phone `serial`'s sync and listing the phones
/// `serials`. `icon` is the absolute PNG path, if any.
pub fn desktop_file(
    e: &AppEntry,
    serial: &str,
    serials: &[String],
    exe: &Path,
    icon: Option<&Path>,
) -> String {
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
    f += &format!("X-Lantern-Serials={}\n", serials_value(serials));
    f += &format!("X-Lantern-Package={}\n", e.pkg);
    f += &format!("X-Lantern-User={}\n", e.user);
    f
}

/// What a Lantern-written `.desktop` file says about itself.
#[derive(Debug)]
struct Owned {
    /// The phones that list this app (`X-Lantern-Serials`, else `[X-Lantern-Serial]`).
    serials: Vec<String>,
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
    let serials = match value(&text, "X-Lantern-Serials") {
        Some(list) => parse_serials(list),
        None => vec![unescape_value(serial)],
    };
    Ok(Some(Owned {
        serials,
        pkg: value(&text, "X-Lantern-Package").map(str::to_owned),
        user: value(&text, "X-Lantern-User").and_then(|u| u.parse().ok()),
        name: value(&text, "Name").map(unescape_value),
    }))
}

/// `text` with its `X-Lantern-Serials=` line set to `serials` (added after `X-Lantern-Serial=`
/// in a file that has none).
fn with_serials(text: &str, serials: &[String]) -> String {
    let new = format!("X-Lantern-Serials={}", serials_value(serials));
    let mut out = String::new();
    let mut done = false;
    for l in text.lines() {
        if l.starts_with("X-Lantern-Serials=") {
            if !done {
                out += &new;
                out.push('\n');
                done = true;
            }
            continue;
        }
        out += l;
        out.push('\n');
        if !done && l.starts_with("X-Lantern-Serial=") {
            out += &new;
            out.push('\n');
            done = true;
        }
    }
    out
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
    /// The phone of the live session; the sync window belongs to it.
    serial: Option<String>,
    /// A full sync whose count matched has ended since `start_session`.
    synced: bool,
    /// `upsert`/`remove` calls since the window opened (`start_session` or the last `end_sync`).
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

    /// A catalogue that runs `rebuild` (debounced) after changes. Writes nothing yet: the
    /// Phone menu files come with the first entry.
    pub fn with_rebuild(dirs: Dirs, rebuild: Rebuild) -> Self {
        Self {
            dirs,
            serial: None,
            synced: false,
            seen: 0,
            users: HashSet::new(),
            keep: HashSet::new(),
            rebuild: Rebuilder::spawn(rebuild, REBUILD_DELAY),
        }
    }

    pub fn dirs(&self) -> &Dirs {
        &self.dirs
    }

    /// A session with `serial` is up: its first sync window opens and [`Self::synced`] is false
    /// until a full catalogue from it arrives.
    pub fn start_session(&mut self, serial: &str) {
        self.serial = Some(serial.to_owned());
        self.synced = false;
        self.seen = 0;
        self.users.clear();
        self.keep.clear();
    }

    /// True once an `end_sync` whose count matched has run since [`Self::start_session`].
    pub fn synced(&self) -> bool {
        self.synced
    }

    /// The phones whose catalogue lists `pkg` for `user`; empty when there is no Lantern entry.
    pub fn phones(&self, pkg: &str, user: u32) -> Vec<String> {
        if check_pkg(pkg).is_err() || check_user(user).is_err() {
            return Vec::new();
        }
        match read_owned(&self.dirs.desktop_path(pkg, user)) {
            Ok(Some(owned)) => owned.serials,
            Ok(None) => Vec::new(),
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not read catalogue entry"
                );
                Vec::new()
            }
        }
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
            .context("no phone session (start_session first)")?;
        let path = self.dirs.desktop_path(&e.pkg, e.user);
        let mut serials = match read_owned(&path)? {
            Some(owned) => owned.serials,
            None if path.exists() => bail!(
                "{} is not a Lantern entry; leaving it alone",
                path.display()
            ),
            None => Vec::new(),
        };
        serials.push(serial.clone());
        let mut changed = match ensure_menu(&self.dirs) {
            Ok(changed) => changed,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not write the Phone menu"
                );
                false
            }
        };
        let icon_path = self.dirs.icon_path(&e.pkg, e.user);
        let icon = if is_png(&e.icon_png) {
            changed |= write_if_changed(&icon_path, &e.icon_png)?;
            Some(icon_path.as_path())
        } else {
            changed |= remove_file(&icon_path)?;
            None
        };
        let text = desktop_file(e, &serial, &serials, &self.dirs.exe, icon);
        changed |= write_if_changed(&path, text.as_bytes())?;
        if changed {
            self.rebuild.changed();
        }
        Ok(())
    }

    /// The session's phone no longer has `pkg` for `user`: its serial comes off that entry,
    /// which goes when no phone is left (a no-op if there is none or it is not Lantern's).
    pub fn remove(&mut self, pkg: &str, user: u32) -> Result<()> {
        self.seen = self.seen.saturating_add(1);
        check_pkg(pkg)?;
        check_user(user)?;
        self.keep.remove(&(pkg.to_owned(), user));
        let serial = self
            .serial
            .clone()
            .context("no phone session (start_session first)")?;
        let path = self.dirs.desktop_path(pkg, user);
        if let Some(owned) = read_owned(&path)? {
            if self.drop_serial(&path, &owned, &serial)? {
                self.rebuild.changed();
            }
        }
        Ok(())
    }

    /// Takes `serial` off the entry at `path`: rewrites its `X-Lantern-Serials`, or removes it
    /// and its icon when no phone is left. Returns whether `serial` was listed.
    fn drop_serial(&self, path: &Path, owned: &Owned, serial: &str) -> Result<bool> {
        if !owned.serials.iter().any(|s| s == serial) {
            return Ok(false);
        }
        let left: Vec<String> = owned
            .serials
            .iter()
            .filter(|s| *s != serial)
            .cloned()
            .collect();
        if left.is_empty() {
            remove_file(path)?;
            if let Some(stem) = path.file_stem() {
                let icon = self
                    .dirs
                    .icons
                    .join(format!("{}.png", stem.to_string_lossy()));
                remove_file(&icon)?;
            }
        } else {
            let text = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let text = with_serials(&String::from_utf8_lossy(&text), &left);
            write_if_changed(path, text.as_bytes())?;
        }
        Ok(true)
    }

    /// Ends the sync window and opens the next one (a companion relink or a profile change
    /// sends another full catalogue). If `count` matches what arrived, the session's phone comes
    /// off every entry it lists for a user that appeared but did not send that app. Returns how
    /// many entries it came off.
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
        self.synced = true;
        let mut removed = 0;
        for (path, owned) in owned_entries(&self.dirs)? {
            let stale = match (&owned.pkg, owned.user) {
                (Some(pkg), Some(user)) => {
                    users.contains(&user) && !keep.contains(&(pkg.clone(), user))
                }
                _ => false,
            };
            if stale && self.drop_serial(&path, &owned, &serial)? {
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::info!(serial, removed, "pruned catalogue entries");
            self.rebuild.changed();
        }
        Ok(removed)
    }

    /// Takes `serial` off every entry (undo-setup); entries left with no phone go, and with no
    /// Lantern entry left the Phone menu goes too. Returns how many entries it came off.
    pub fn remove_serial(&mut self, serial: &str) -> Result<usize> {
        let mut removed = 0;
        let mut left = 0;
        for (path, owned) in owned_entries(&self.dirs)? {
            if self.drop_serial(&path, &owned, serial)? {
                removed += 1;
            }
            if path.exists() {
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

/// The "Connect your …" notification for `lantern open [--user N] <pkg>` refused for want of
/// the right phone: (summary, body, icon). The app is named by its menu entry's Name (else the
/// package) and the phones its entry lists by `names` (else their serials). `here` is the
/// connected phone that lacks the app, if any.
pub fn connect_notice(
    dirs: &Dirs,
    names: &dyn Fn(&str) -> Option<String>,
    pkg: &str,
    user: u32,
    here: Option<&str>,
) -> (String, String, Option<PathBuf>) {
    let (owned, icon) = if check_pkg(pkg).is_ok() {
        let owned = read_owned(&dirs.desktop_path(pkg, user)).ok().flatten();
        let icon = Some(dirs.icon_path(pkg, user)).filter(|p| p.is_file());
        (owned, icon)
    } else {
        (None, None)
    };
    let app = owned
        .as_ref()
        .and_then(|o| o.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| pkg.to_owned());
    let n = |serial: &str| names(serial).unwrap_or_else(|| serial.to_owned());
    let phones: &[String] = owned.as_ref().map_or(&[], |o| &o.serials);
    let (summary, body) = match phones {
        [] => (
            "Connect your phone".to_owned(),
            format!("{app} opens when your phone is connected by USB."),
        ),
        [a] => {
            let a = n(a);
            let body = match here {
                Some(h) => format!("{app} is on your {a}, not on {}.", n(h)),
                None => format!("{app} opens when your {a} is connected by USB."),
            };
            (format!("Connect your {a}"), body)
        }
        [a, b] => (
            format!("Connect {} or {}", n(a), n(b)),
            format!("{app} opens when one of them is connected by USB."),
        ),
        [a, b, rest @ ..] => (
            "Connect one of your phones".to_owned(),
            format!("{app} is on {}, {} and {} more.", n(a), n(b), rest.len()),
        ),
    };
    (summary, body, icon)
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

    /// A full sync from `serial`: a new session, every app upserted, a matching `end_sync`.
    fn full_sync(cat: &mut Catalog, serial: &str, apps: &[(&str, u32)]) -> usize {
        cat.start_session(serial);
        for (pkg, user) in apps {
            cat.upsert(&entry(pkg, *user)).unwrap();
        }
        cat.end_sync(apps.len() as u32).unwrap()
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn serials_line(path: &Path) -> Vec<String> {
        let f = std::fs::read_to_string(path).unwrap();
        line(&f, "X-Lantern-Serials")
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn full_sync_keeps_other_phones_entries() {
        let (_t, dirs) = tmp();
        let b = desktop(&dirs, "com.b", 0);
        let files = [
            desktop(&dirs, "com.a", 0),
            desktop(&dirs, "com.x", 0),
            dirs.icons.join("lantern.com.a.png"),
            dirs.icons.join("lantern.com.x.png"),
        ];
        let (mut cat, _) = quiet(dirs);
        assert_eq!(full_sync(&mut cat, "A", &[("com.a", 0), ("com.x", 0)]), 0);
        let old = SystemTime::now() - Duration::from_secs(3600);
        for f in &files {
            set_mtime(f, old);
        }
        let before: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
        assert_eq!(full_sync(&mut cat, "B", &[("com.b", 0)]), 0);
        for (f, bytes) in files.iter().zip(&before) {
            assert_eq!(&std::fs::read(f).unwrap(), bytes, "{f:?}");
            assert_eq!(
                std::fs::metadata(f).unwrap().modified().unwrap(),
                old,
                "{f:?}"
            );
        }
        assert_eq!(serials_line(&b), ["B;"]);
        assert_eq!(cat.phones("com.b", 0), ["B"]);
        assert_eq!(cat.phones("com.a", 0), ["A"]);
        assert!(cat.phones("com.none", 0).is_empty());
    }

    #[test]
    fn shared_app_lists_both_phones() {
        let (_t, dirs) = tmp();
        let (s, t) = (desktop(&dirs, "com.s", 0), desktop(&dirs, "com.t", 0));
        let (mut cat, _) = quiet(dirs);
        full_sync(&mut cat, "A", &[("com.s", 0)]);
        full_sync(&mut cat, "B", &[("com.s", 0)]);
        assert_eq!(serials_line(&s), ["A;B;"]);
        // The ownership marker names the phone whose sync wrote the file.
        let f = std::fs::read_to_string(&s).unwrap();
        assert_eq!(line(&f, "X-Lantern-Serial"), ["B"]);
        assert_eq!(cat.phones("com.s", 0), ["A", "B"]);
        // Sorted and deduplicated, whatever the order the phones synced in.
        full_sync(&mut cat, "B", &[("com.t", 0)]);
        full_sync(&mut cat, "A", &[("com.t", 0)]);
        full_sync(&mut cat, "A", &[("com.t", 0)]);
        assert_eq!(serials_line(&t), ["A;B;"]);
    }

    #[test]
    fn full_sync_drops_only_that_phone() {
        let (_t, dirs) = tmp();
        let s = desktop(&dirs, "com.s", 0);
        let s_icon = dirs.icons.join("lantern.com.s.png");
        let (mut cat, _) = quiet(dirs);
        full_sync(&mut cat, "A", &[("com.s", 0), ("com.a", 0)]);
        full_sync(&mut cat, "B", &[("com.s", 0), ("com.b", 0)]);
        assert_eq!(full_sync(&mut cat, "B", &[("com.b", 0)]), 1);
        assert!(s.exists() && s_icon.exists());
        assert_eq!(serials_line(&s), ["A;"]);
        assert_eq!(cat.phones("com.s", 0), ["A"]);
        assert_eq!(full_sync(&mut cat, "A", &[("com.a", 0)]), 1);
        assert!(!s.exists() && !s_icon.exists());
        assert!(cat.phones("com.s", 0).is_empty());
    }

    #[test]
    fn live_remove_drops_only_that_phone() {
        let (_t, dirs) = tmp();
        let s = desktop(&dirs, "com.s", 0);
        let s_icon = dirs.icons.join("lantern.com.s.png");
        let (mut cat, _) = quiet(dirs);
        cat.start_session("A");
        cat.upsert(&entry("com.s", 0)).unwrap();
        cat.start_session("B");
        cat.upsert(&entry("com.s", 0)).unwrap();
        cat.remove("com.s", 0).unwrap();
        assert!(s.exists() && s_icon.exists());
        assert_eq!(serials_line(&s), ["A;"]);
        // B no longer lists it: removing it again changes nothing.
        cat.remove("com.s", 0).unwrap();
        assert_eq!(cat.phones("com.s", 0), ["A"]);
        cat.start_session("A");
        cat.remove("com.s", 0).unwrap();
        assert!(!s.exists() && !s_icon.exists());
    }

    #[test]
    fn legacy_entry_reads_as_one_phone() {
        let (_t, dirs) = tmp();
        let l = desktop(&dirs, "com.l", 0);
        std::fs::create_dir_all(&dirs.applications).unwrap();
        std::fs::write(
            &l,
            "[Desktop Entry]\nType=Application\nName=Old\nExec=x open com.l\n\
             Categories=X-Lantern;\nX-Lantern-Serial=A\nX-Lantern-Package=com.l\n\
             X-Lantern-User=0\n",
        )
        .unwrap();
        let (mut cat, _) = quiet(dirs);
        assert_eq!(cat.phones("com.l", 0), ["A"]);
        cat.start_session("B");
        cat.upsert(&entry("com.l", 0)).unwrap();
        assert_eq!(serials_line(&l), ["A;B;"]);
        assert_eq!(cat.phones("com.l", 0), ["A", "B"]);
    }

    #[test]
    fn serial_with_semicolon_round_trips() {
        let (_t, dirs) = tmp();
        let a = desktop(&dirs, "com.a", 0);
        let (mut cat, _) = quiet(dirs);
        cat.start_session("a;b");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(cat.phones("com.a", 0), ["a;b"]);
        cat.start_session("c\\d");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(serials_line(&a), ["a\\;b;c\\\\d;"]);
        assert_eq!(cat.phones("com.a", 0), ["a;b", "c\\d"]);
        cat.remove("com.a", 0).unwrap();
        assert_eq!(cat.phones("com.a", 0), ["a;b"]);
    }

    #[test]
    fn synced_after_matching_end_sync_only() {
        let (_t, dirs) = tmp();
        let (mut cat, _) = quiet(dirs);
        assert!(!cat.synced());
        cat.start_session("A");
        assert!(!cat.synced());
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.end_sync(2).unwrap();
        assert!(!cat.synced(), "mismatched end_sync");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.end_sync(1).unwrap();
        assert!(cat.synced());
        cat.start_session("B");
        assert!(!cat.synced());
    }

    #[test]
    fn desktop_file_escapes_label() {
        let mut label = "A\nB=C;%x \"q\" 🙂".to_owned();
        label.push_str(&"x".repeat(10_000));
        let e = AppEntry {
            label,
            ..entry("com.a", 0)
        };
        let f = desktop_file(
            &e,
            "ser",
            &["ser".to_owned()],
            Path::new("/usr/bin/lantern"),
            None,
        );
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
            "X-Lantern-Serials=ser;",
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
        let f = desktop_file(&e, "s", &["s".to_owned()], Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["a\\\\b\\tc\\rdef"]);
        // Truncation never splits an escape: 199 x then a backslash gives no dangling `\`.
        let e = AppEntry {
            label: format!("{}\\", "x".repeat(199)),
            ..entry("com.a", 0)
        };
        let f = desktop_file(&e, "s", &["s".to_owned()], Path::new("/l"), None);
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
        let f = desktop_file(
            &work,
            "s",
            &["s".to_owned()],
            Path::new("/l"),
            Some(Path::new("/i/x.png")),
        );
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
        let f = desktop_file(&other, "s", &["s".to_owned()], Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["Files (user 11)"]);
        let owner = AppEntry {
            label: "Files".into(),
            work: false,
            ..entry("com.f", 0)
        };
        assert_eq!(
            line(
                &desktop_file(&owner, "s", &["s".to_owned()], Path::new("/l"), None),
                "Name"
            ),
            ["Files"]
        );
        // A long label still leaves room for the suffix.
        let long = AppEntry {
            label: "y".repeat(500),
            work: true,
            ..entry("com.f", 10)
        };
        let f = desktop_file(&long, "s", &["s".to_owned()], Path::new("/l"), None);
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
        let f = desktop_file(&e, "s", &["s".to_owned()], Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["com.f (work)"]);
        let e = AppEntry {
            label: "\u{1}\u{2}".into(),
            work: false,
            ..entry("com.f", 0)
        };
        let f = desktop_file(&e, "s", &["s".to_owned()], Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["com.f"]);
    }

    #[test]
    fn exec_quotes_exe_with_space() {
        let f = desktop_file(
            &entry("com.a", 0),
            "s",
            &["s".to_owned()],
            Path::new("/home/a b/lantern"),
            None,
        );
        assert_eq!(line(&f, "Exec"), ["\"/home/a b/lantern\" open com.a"]);
        // `"`, `` ` ``, `$`, `\` are backslash-escaped inside the quotes, then the string
        // escape doubles every backslash; `%` is doubled.
        let f = desktop_file(
            &entry("com.a", 0),
            "s",
            &["s".to_owned()],
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
        cat.start_session("s");
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
        cat.start_session("ser");
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
        cat.start_session("s");
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
        cat.start_session("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.upsert(&entry("com.b", 0)).unwrap();
        assert_eq!(cat.end_sync(2).unwrap(), 0);
        cat.start_session("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(cat.end_sync(1).unwrap(), 1);
        assert!(a.exists());
        assert!(!b.exists());
        assert!(!b_icon.exists());
    }

    #[test]
    fn full_sync_keeps_absent_profile() {
        let (_t, dirs) = tmp();
        let (a, w) = (desktop(&dirs, "com.a", 0), desktop(&dirs, "com.w", 10));
        let (mut cat, _) = quiet(dirs);
        full_sync(&mut cat, "A", &[("com.a", 0), ("com.w", 10)]);
        full_sync(&mut cat, "B", &[("com.b", 0), ("com.w", 10)]);
        // B's work profile is paused: its next full sync has only user 0's apps.
        assert_eq!(full_sync(&mut cat, "B", &[("com.b", 0)]), 0);
        assert_eq!(cat.phones("com.w", 10), ["A", "B"]);
        // So is A's.
        assert_eq!(full_sync(&mut cat, "A", &[("com.a", 0)]), 0);
        assert!(a.exists() && w.exists());
        assert_eq!(cat.phones("com.w", 10), ["A", "B"]);
    }

    #[test]
    fn partial_sync_prunes_nothing() {
        let (_t, dirs) = tmp();
        let b = desktop(&dirs, "com.b", 0);
        let (mut cat, _) = quiet(dirs);
        full_sync(&mut cat, "A", &[("com.a", 0), ("com.b", 0)]);
        full_sync(&mut cat, "B", &[("com.a", 0), ("com.b", 0)]);
        cat.start_session("B");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(cat.end_sync(5).unwrap(), 0);
        assert!(b.exists());
        assert_eq!(cat.phones("com.b", 0), ["A", "B"]);
        // A live removal counts toward the window, so the sync no longer matches.
        cat.start_session("A");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.remove("com.zz", 0).unwrap();
        assert_eq!(cat.end_sync(1).unwrap(), 0);
        assert_eq!(cat.phones("com.b", 0), ["A", "B"]);
    }

    #[test]
    fn remove_deletes_entry_and_icon() {
        let (_t, dirs) = tmp();
        let a = desktop(&dirs, "com.a", 10);
        let icon = dirs.icons.join("lantern.com.a-u10.png");
        let (mut cat, _) = quiet(dirs);
        cat.start_session("s");
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
        cat.start_session("s");
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
    fn remove_serial_drops_one_phone() {
        let (_t, dirs) = tmp();
        let (a, b, s) = (
            desktop(&dirs, "com.a", 0),
            desktop(&dirs, "com.b", 0),
            desktop(&dirs, "com.s", 0),
        );
        let menu = dirs.menus.join("lantern.menu");
        let dir = dirs.directories.join("lantern-phone.directory");
        let (mut cat, _) = quiet(dirs);
        cat.start_session("one");
        cat.upsert(&entry("com.a", 0)).unwrap();
        cat.upsert(&entry("com.s", 0)).unwrap();
        cat.start_session("two");
        cat.upsert(&entry("com.b", 0)).unwrap();
        cat.upsert(&entry("com.s", 0)).unwrap();
        assert!(menu.exists() && dir.exists());
        assert_eq!(cat.remove_serial("one").unwrap(), 2);
        assert!(!a.exists() && b.exists() && s.exists());
        assert_eq!(cat.phones("com.s", 0), ["two"]);
        assert!(
            menu.exists() && dir.exists(),
            "other entries remain: the menu stays"
        );
        assert_eq!(cat.remove_serial("one").unwrap(), 0);
        assert_eq!(cat.remove_serial("two").unwrap(), 2);
        assert!(!b.exists() && !s.exists());
        assert!(
            !menu.exists() && !dir.exists(),
            "no entries left: menu removed"
        );
    }

    #[test]
    fn construction_writes_no_menu_files() {
        let (_t, dirs) = tmp();
        let menu = dirs.menus.join("lantern.menu");
        let dir = dirs.directories.join("lantern-phone.directory");
        let (cat, runs) = quiet(dirs);
        drop(cat);
        assert!(!menu.exists() && !dir.exists());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn menu_and_directory_files() {
        let (_t, dirs) = tmp();
        let menu = dirs.menus.join("lantern.menu");
        let dir = dirs.directories.join("lantern-phone.directory");
        let (mut cat, _) = quiet(dirs);
        cat.start_session("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
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
        cat.start_session("s");
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

    /// Names for "A" and "B" only; any other serial has none.
    fn names(serial: &str) -> Option<String> {
        match serial {
            "A" => Some("Phone A".into()),
            "B" => Some("Phone B".into()),
            "H" => Some("Phone H".into()),
            _ => None,
        }
    }

    /// Plants `com.a`'s entry, labelled "Alpha", listing `serials`.
    fn plant(dirs: &Dirs, serials: &[&str]) {
        let serials: Vec<String> = serials.iter().map(|s| s.to_string()).collect();
        let e = AppEntry {
            label: "Alpha".into(),
            ..entry("com.a", 0)
        };
        let path = desktop(dirs, "com.a", 0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = desktop_file(&e, &serials[0], &serials, &dirs.exe, None);
        std::fs::write(path, text).unwrap();
    }

    fn notice(dirs: &Dirs, here: Option<&str>) -> (String, String) {
        let (s, b, _) = connect_notice(dirs, &names, "com.a", 0, here);
        (s, b)
    }

    #[test]
    fn connect_notice_without_entry_is_unchanged() {
        let (_t, dirs) = tmp();
        assert_eq!(
            notice(&dirs, None),
            (
                "Connect your phone".into(),
                "com.a opens when your phone is connected by USB.".into()
            )
        );
        assert_eq!(notice(&dirs, Some("H")).0, "Connect your phone");
    }

    #[test]
    fn connect_notice_one_phone() {
        let (_t, dirs) = tmp();
        plant(&dirs, &["A"]);
        assert_eq!(
            notice(&dirs, None),
            (
                "Connect your Phone A".into(),
                "Alpha opens when your Phone A is connected by USB.".into()
            )
        );
        // A serial without a recorded name stands for itself.
        plant(&dirs, &["s1"]);
        assert_eq!(
            notice(&dirs, None),
            (
                "Connect your s1".into(),
                "Alpha opens when your s1 is connected by USB.".into()
            )
        );
    }

    #[test]
    fn connect_notice_one_phone_while_another_is_here() {
        let (_t, dirs) = tmp();
        plant(&dirs, &["A"]);
        assert_eq!(
            notice(&dirs, Some("H")),
            (
                "Connect your Phone A".into(),
                "Alpha is on your Phone A, not on Phone H.".into()
            )
        );
        assert_eq!(
            notice(&dirs, Some("h2")).1,
            "Alpha is on your Phone A, not on h2."
        );
    }

    #[test]
    fn connect_notice_two_phones() {
        let (_t, dirs) = tmp();
        plant(&dirs, &["A", "B"]);
        let want = (
            "Connect Phone A or Phone B".to_owned(),
            "Alpha opens when one of them is connected by USB.".to_owned(),
        );
        assert_eq!(notice(&dirs, None), want);
        assert_eq!(notice(&dirs, Some("H")), want);
    }

    #[test]
    fn connect_notice_three_or_more_phones() {
        let (_t, dirs) = tmp();
        plant(&dirs, &["A", "B", "c"]);
        assert_eq!(
            notice(&dirs, Some("H")),
            (
                "Connect one of your phones".into(),
                "Alpha is on Phone A, Phone B and 1 more.".into()
            )
        );
        plant(&dirs, &["A", "B", "c", "d", "e"]);
        assert_eq!(
            notice(&dirs, None).1,
            "Alpha is on Phone A, Phone B and 3 more."
        );
    }

    #[test]
    fn connect_notice_uses_entry_name_and_icon() {
        let (_t, dirs) = tmp();
        let (s, body, icon) = connect_notice(&dirs, &|_| None, "com.a", 0, None);
        assert_eq!(s, "Connect your phone");
        assert_eq!(body, "com.a opens when your phone is connected by USB.");
        assert_eq!(icon, None);
        let icons = dirs.icons.clone();
        let (mut cat, _) = quiet(Dirs { ..dirs });
        cat.start_session("s");
        cat.upsert(&AppEntry {
            label: "Sett\\ings".into(),
            ..entry("com.a", 0)
        })
        .unwrap();
        let dirs = cat.dirs();
        let (_, body, icon) = connect_notice(dirs, &|_| None, "com.a", 0, None);
        assert_eq!(body, "Sett\\ings opens when your s is connected by USB.");
        assert_eq!(icon, Some(icons.join("lantern.com.a.png")));
    }
}
