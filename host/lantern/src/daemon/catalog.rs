//! The phones' launcher apps as KDE menu entries, one folder per phone.
//!
//! lanternd writes one `.desktop` file per (phone, launcher app, Android user) into
//! `$XDG_DATA_HOME/applications`, named by the phone's [`Naming::app_id`] (so the window's app_id
//! matches `StartupWMClass`): the primary phone keeps the names from before Lantern served
//! several phones, every other phone gets a `-p<tag>` suffix. The icon goes to
//! `$XDG_DATA_HOME/lantern/icons/<app_id>.png`. A merged menu (`lantern.menu`,
//! `lantern-phone.directory` and one `lantern-phone-<tag>.directory` per phone) nests one submenu
//! per phone under "Phone", gathering `Categories=X-Lantern-<tag>`.
//!
//! Every `.desktop` file Lantern writes carries `X-Lantern-Serial=`, and so do the per-phone
//! folder files; a file without it is never modified or deleted. A phone's session only writes,
//! prunes and removes its own files. Full syncs (`start_session` … `end_sync(count)`) prune the
//! phone's entries it no longer lists, but only when every announced app arrived, and only for
//! the Android users that appeared in the sync (a paused work profile sends nothing and keeps its
//! entries). Two phones whose serials give the same tag would share a folder: the second one's
//! entries are refused. After changes, `kbuildsycoca6` runs once, 1 s after the last one.
//!
//! [`migrate`] turns the shared entries of Plan 3 (`X-Lantern-Serials=`, one file per app for
//! every phone) and Plan 2 (one phone) into per-phone files once, before the catalogue is built.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::daemon::companion::{self, Names};
use crate::naming::{self, Naming};
pub use crate::window::identity::app_id;
use crate::window::identity::{profile_suffix, MAX_LABEL, MAX_USER};

/// Where the catalogue lives. See [`Dirs::from_env`].
#[derive(Debug, Clone)]
pub struct Dirs {
    pub applications: PathBuf,
    pub icons: PathBuf,
    pub menus: PathBuf,
    pub directories: PathBuf,
    /// Lantern's state dir (`$XDG_STATE_HOME/lantern`): `menu.json` and the device records.
    pub state: PathBuf,
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
    /// `$XDG_DATA_HOME` (default `~/.local/share`), `$XDG_CONFIG_HOME` (default `~/.config`),
    /// `$XDG_STATE_HOME` (default `~/.local/state`) and the running executable.
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
            xdg("XDG_STATE_HOME"),
            &home,
            exe,
        ))
    }

    /// The layout under the given base dirs; `None` (or a relative path) means the default.
    pub fn resolve(
        data: Option<PathBuf>,
        config: Option<PathBuf>,
        state: Option<PathBuf>,
        home: &Path,
        exe: PathBuf,
    ) -> Dirs {
        let base = |dir: Option<PathBuf>, default: &str| {
            dir.filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(default))
        };
        let data = base(data, ".local/share");
        let config = base(config, ".config");
        Dirs {
            applications: data.join("applications"),
            icons: data.join("lantern").join("icons"),
            menus: config.join("menus").join("applications-merged"),
            directories: data.join("desktop-directories"),
            state: base(state, ".local/state").join("lantern"),
            exe,
        }
    }

    fn entry_path(&self, stem: &str) -> PathBuf {
        self.applications.join(format!("{stem}.desktop"))
    }

    fn icon_path(&self, stem: &str) -> PathBuf {
        self.icons.join(format!("{stem}.png"))
    }

    fn folder_path(&self, serial: &str) -> PathBuf {
        self.directories.join(naming::directory_file(serial))
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

/// `arg` as one Exec argument: double-quoted with `"`, `` ` ``, `$`, `\` backslash-escaped, `%`
/// doubled, then string-escaped (which doubles those backslashes), per the Desktop Entry spec.
fn exec_quote(arg: &str) -> String {
    let mut quoted = String::from("\"");
    for c in arg.chars() {
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

/// The serial as an Exec argument: bare when it is all `[A-Za-z0-9._:-]`, else quoted.
fn exec_serial(serial: &str) -> String {
    let bare = !serial.is_empty()
        && serial
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
    if bare {
        serial.to_owned()
    } else {
        exec_quote(serial)
    }
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

/// A per-phone entry; `name` is the already escaped `Name=` value.
fn entry_text(
    name: &str,
    pkg: &str,
    user: u32,
    serial: &str,
    naming: &Naming,
    exe: &Path,
    icon: Option<&Path>,
) -> String {
    let user_arg = match user {
        0 => String::new(),
        n => format!(" --user {n}"),
    };
    let mut f = String::from("[Desktop Entry]\nType=Application\nVersion=1.5\n");
    f += &format!("Name={name}\n");
    f += &format!(
        "Exec={} open --phone {}{user_arg} {pkg}\n",
        exec_quote(&exe.to_string_lossy()),
        exec_serial(serial)
    );
    if let Some(icon) = icon {
        f += &format!(
            "Icon={}\n",
            escape_value(&icon.to_string_lossy(), usize::MAX)
        );
    }
    f += &format!("Categories={};\n", naming::category(serial));
    f += &format!("StartupWMClass={}\n", naming.app_id(pkg, user, serial));
    f += "StartupNotify=true\n";
    f += &format!("X-Lantern-Serial={}\n", escape_value(serial, MAX_LABEL));
    f += &format!("X-Lantern-Package={pkg}\n");
    f += &format!("X-Lantern-User={user}\n");
    f
}

/// The `.desktop` file of `e` on phone `serial`, named by `naming`. `icon` is the absolute PNG
/// path (`icons/<app_id>.png`), if any.
pub fn desktop_file(
    e: &AppEntry,
    serial: &str,
    naming: &Naming,
    exe: &Path,
    icon: Option<&Path>,
) -> String {
    entry_text(&entry_name(e), &e.pkg, e.user, serial, naming, exe, icon)
}

/// `X-Lantern-Serials=` value (Plan 3 shared entries): each serial string-escaped with `;` as
/// `\;`, each followed by `;`, sorted and deduplicated.
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

/// What a Lantern-written `.desktop` file says about itself.
#[derive(Debug)]
struct Owned {
    /// `X-Lantern-Serial`: the phone whose sync wrote it.
    serial: String,
    /// The phones that have this entry (`X-Lantern-Serials` of a Plan 3 shared file, else
    /// `[X-Lantern-Serial]`).
    serials: Vec<String>,
    /// It has `X-Lantern-Serials` (a Plan 3 shared file).
    shared: bool,
    pkg: Option<String>,
    user: Option<u32>,
    /// `Name=` unescaped.
    name: Option<String>,
    /// `Name=` as written.
    raw_name: Option<String>,
}

impl Owned {
    /// The entry is `serial`'s alone.
    fn is_only(&self, serial: &str) -> bool {
        self.serials.len() == 1 && self.serials[0] == serial
    }

    /// Its package and user, if both are valid.
    fn app(&self) -> Option<(&str, u32)> {
        let pkg = self.pkg.as_deref()?;
        let user = self.user?;
        (check_pkg(pkg).is_ok() && check_user(user).is_ok()).then_some((pkg, user))
    }
}

fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// `X-Lantern-Serial` of a file Lantern wrote (unescaped), `None` for anyone else's.
fn marker(text: &str) -> Option<String> {
    value(text, "X-Lantern-Serial").map(unescape_value)
}

/// The file's text, `None` if it is absent.
fn read_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// `Some` if `path` is a Lantern entry (has `X-Lantern-Serial=`), `None` if it is someone
/// else's or absent.
fn read_owned(path: &Path) -> Result<Option<Owned>> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    let Some(serial) = marker(&text) else {
        return Ok(None);
    };
    let shared = value(&text, "X-Lantern-Serials");
    let serials = match shared {
        Some(list) => parse_serials(list),
        None => vec![serial.clone()],
    };
    let raw_name = value(&text, "Name").map(str::to_owned);
    Ok(Some(Owned {
        serial,
        serials,
        shared: shared.is_some(),
        pkg: value(&text, "X-Lantern-Package").map(str::to_owned),
        user: value(&text, "X-Lantern-User").and_then(|u| u.parse().ok()),
        name: raw_name.as_deref().map(unescape_value),
        raw_name,
    }))
}

/// `text` with its `X-Lantern-Serials=` line set to `serials` (Plan 3 shared files only).
fn with_serials(text: &str, serials: &[String]) -> String {
    let new = format!("X-Lantern-Serials={}", serials_value(serials));
    let mut out = String::new();
    for l in text.lines() {
        if l.starts_with("X-Lantern-Serials=") {
            out += &new;
        } else {
            out += l;
        }
        out.push('\n');
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

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Removes the entry at `path` and its icon.
fn remove_entry(dirs: &Dirs, path: &Path) -> Result<()> {
    remove_file(path)?;
    remove_file(&dirs.icon_path(&stem_of(path)))?;
    Ok(())
}

const MENU_FILE: &str = "lantern.menu";
const DIRECTORY_FILE: &str = "lantern-phone.directory";

const MENU_HEAD: &str = "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\" \
\"http://www.freedesktop.org/standards/menu-spec/menu-1.0.dtd\">\n\
<Menu><Name>Applications</Name><Menu><Name>Phone</Name>\
<Directory>lantern-phone.directory</Directory>";

const MENU_TAIL: &str = "\n</Menu></Menu>\n";

const DIRECTORY: &str = "[Desktop Entry]\nType=Directory\nName=Phone\nIcon=smartphone\n";

fn folder_text(name: &str, serial: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Directory\nName={}\nIcon=smartphone\nX-Lantern-Serial={}\n",
        escape_value(name, MAX_LABEL),
        escape_value(serial, MAX_LABEL)
    )
}

/// Writes the menu files for `phones` (serial, name) and removes the ones no longer needed. See
/// [`write_menu`].
pub fn ensure_menu(dirs: &Dirs, phones: &[(String, Option<String>)]) -> Result<bool> {
    let names = |serial: &str| {
        phones
            .iter()
            .find(|(s, _)| s == serial)
            .and_then(|(_, name)| name.clone())
    };
    write_menu(dirs, &owned_entries(dirs)?, &names)
}

/// The menu files for the phones that have at least one per-phone entry in `entries`:
/// `lantern.menu` nests one submenu per phone (sorted by display name) under "Phone", plus
/// `lantern-phone.directory` and each phone's folder file. A phone is named by `names`, else
/// by its folder file's own `Name`, else by its serial. A folder file of a phone with no entry
/// left goes, and with no Lantern entry left at all both menu files go (with only Plan 3
/// shared entries left, the menu files are left alone). Only files with
/// `X-Lantern-Serial=` (and the two fixed menu files) are written or removed; the smallest
/// serial keeps a tag shared by two phones. Returns whether anything changed.
fn write_menu(
    dirs: &Dirs,
    entries: &[(PathBuf, Owned)],
    names: &dyn Fn(&str) -> Option<String>,
) -> Result<bool> {
    let serials: BTreeSet<&str> = entries
        .iter()
        .filter(|(_, o)| !o.shared)
        .map(|(_, o)| o.serial.as_str())
        .collect();
    let mut by_tag: BTreeMap<String, &str> = BTreeMap::new();
    for serial in serials {
        by_tag.entry(naming::tag(serial)).or_insert(serial);
    }
    let mut changed = false;
    let mut phones: Vec<(String, &str)> = Vec::new();
    for serial in by_tag.values().copied() {
        let path = dirs.folder_path(serial);
        let existing = read_text(&path)?;
        let own_name = match existing.as_deref() {
            None => None,
            Some(text) if marker(text).as_deref() == Some(serial) => value(text, "Name"),
            Some(_) => {
                tracing::warn!(path = %path.display(), "folder file is not this phone's; leaving it");
                phones.push((names(serial).unwrap_or_else(|| serial.to_owned()), serial));
                continue;
            }
        };
        let name = names(serial)
            .or_else(|| own_name.map(unescape_value))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| serial.to_owned());
        changed |= write_if_changed(&path, folder_text(&name, serial).as_bytes())?;
        phones.push((name, serial));
    }
    // Folder files of phones without an entry.
    if let Ok(rd) = std::fs::read_dir(&dirs.directories) {
        for item in rd {
            let path = item?.path();
            let ours = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("lantern-phone-") && n.ends_with(".directory"));
            if !ours {
                continue;
            }
            let Some(serial) = read_text(&path)?.as_deref().and_then(marker) else {
                continue;
            };
            if by_tag.get(&naming::tag(&serial)) != Some(&serial.as_str()) {
                changed |= remove_file(&path)?;
            }
        }
    }
    let menu = dirs.menus.join(MENU_FILE);
    let directory = dirs.directories.join(DIRECTORY_FILE);
    if phones.is_empty() {
        // Only Plan 3 shared entries left (not migrated yet): their menu stays as it is.
        if entries.is_empty() {
            changed |= remove_file(&menu)?;
            changed |= remove_file(&directory)?;
        }
        return Ok(changed);
    }
    phones.sort();
    let mut text = MENU_HEAD.to_owned();
    for (_, serial) in &phones {
        let t = naming::tag(serial);
        text += &format!(
            "\n<Menu><Name>lantern-{t}</Name><Directory>lantern-phone-{t}.directory</Directory>\
             <Include><Category>X-Lantern-{t}</Category></Include></Menu>"
        );
    }
    text += MENU_TAIL;
    changed |= write_if_changed(&menu, text.as_bytes())?;
    changed |= write_if_changed(&directory, DIRECTORY.as_bytes())?;
    Ok(changed)
}

/// Every `lantern.*.desktop` in `applications` that Lantern owns, sorted by path.
fn owned_entries(dirs: &Dirs) -> Result<Vec<(PathBuf, Owned)>> {
    entries_where(dirs, |name| {
        name.starts_with("lantern.") && name.ends_with(".desktop")
    })
}

/// The Lantern entries whose file name passes `keep`, sorted by path.
fn entries_where(dirs: &Dirs, keep: impl Fn(&str) -> bool) -> Result<Vec<(PathBuf, Owned)>> {
    let rd = match std::fs::read_dir(&dirs.applications) {
        Ok(rd) => rd,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("list {}", dirs.applications.display())),
    };
    let mut out = Vec::new();
    for item in rd {
        let path = item?.path();
        if !path.file_name().and_then(|n| n.to_str()).is_some_and(&keep) {
            continue;
        }
        match read_owned(&path) {
            Ok(Some(owned)) => out.push((path, owned)),
            Ok(None) => {}
            Err(error) => tracing::warn!(error = format!("{error:#}"), "skipping catalogue entry"),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The Lantern entries of `pkg` for `user`, on any phone: files named `lantern.<pkg>[-u<N>]` or
/// `…-p<tag>` whose own package and user match.
fn entries_of(dirs: &Dirs, pkg: &str, user: u32) -> Vec<(PathBuf, Owned)> {
    if check_pkg(pkg).is_err() || check_user(user).is_err() {
        return Vec::new();
    }
    let stem = app_id(pkg, user);
    let exact = format!("{stem}.desktop");
    let suffixed = format!("{stem}-p");
    let found = entries_where(dirs, |name| {
        name == exact || (name.starts_with(&suffixed) && name.ends_with(".desktop"))
    });
    match found {
        Ok(found) => found
            .into_iter()
            .filter(|(_, o)| o.pkg.as_deref() == Some(pkg) && o.user == Some(user))
            .collect(),
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not read catalogue entries"
            );
            Vec::new()
        }
    }
}

/// The phones that have an entry for `pkg` and `user` (a Plan 3 shared file counts each phone
/// it lists), sorted; empty when there is no Lantern entry.
pub fn phones_with(dirs: &Dirs, pkg: &str, user: u32) -> Vec<String> {
    let set: BTreeSet<String> = entries_of(dirs, pkg, user)
        .into_iter()
        .flat_map(|(_, o)| o.serials)
        .collect();
    set.into_iter().collect()
}

/// The `Name` of any entry for `pkg` and `user`, else `pkg`.
pub fn app_name(dirs: &Dirs, pkg: &str, user: u32) -> String {
    app_of_entries(dirs, pkg, user).0
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

/// The menu entries of the phones' launcher apps. See the module docs.
pub struct Catalog {
    dirs: Dirs,
    /// Which phone keeps the legacy names (from `menu.json`).
    naming: Naming,
    /// Folder names (else each folder keeps its own, else the serial).
    names: Option<Names>,
    /// The phone of the live session; the sync window belongs to it.
    serial: Option<String>,
    /// The session's phone shares its tag with another phone that has entries: it gets none.
    blocked: bool,
    /// The menu has been brought up to date since the session's first entry was written.
    menu_ok: bool,
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

    /// A catalogue that runs `rebuild` (debounced) after changes, named by the `menu.json` in
    /// `dirs.state`. Writes nothing yet: the Phone menu files come with the first entry.
    pub fn with_rebuild(dirs: Dirs, rebuild: Rebuild) -> Self {
        Self {
            naming: Naming::load(&dirs.state),
            dirs,
            names: None,
            serial: None,
            blocked: false,
            menu_ok: false,
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

    /// Names the phones' folders.
    pub fn set_names(&mut self, names: Names) {
        self.names = Some(names);
    }

    /// A session with `serial` is up: it becomes the primary phone if there is none yet
    /// (recorded in `menu.json`), its first sync window opens, [`Self::synced`] is false until a
    /// full catalogue from it arrives, and the menu is rewritten (its name may have changed).
    pub fn start_session(&mut self, serial: &str) {
        if self.naming.primary.is_none() {
            self.naming.primary = Some(serial.to_owned());
            match self.naming.save(&self.dirs.state) {
                Ok(()) => tracing::info!(serial, "primary phone recorded"),
                Err(error) => tracing::warn!(
                    serial,
                    error = format!("{error:#}"),
                    "could not record the primary phone"
                ),
            }
        }
        self.serial = Some(serial.to_owned());
        self.synced = false;
        self.seen = 0;
        self.users.clear();
        self.keep.clear();
        self.menu_ok = false;
        self.blocked = self.tag_taken(serial);
        if self.blocked {
            tracing::warn!(
                serial,
                tag = naming::tag(serial),
                "another phone's menu entries use this phone's tag; its apps get no menu entries"
            );
        }
        self.refresh_menu();
    }

    /// Whether another phone that has entries (or a folder) uses `serial`'s tag.
    fn tag_taken(&self, serial: &str) -> bool {
        let tag = naming::tag(serial);
        let other = |s: &str| s != serial && naming::tag(s) == tag;
        let entries = match owned_entries(&self.dirs) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not list catalogue entries"
                );
                return false;
            }
        };
        let in_entries = entries
            .iter()
            .any(|(_, o)| o.serials.iter().any(|s| other(s)) || other(&o.serial));
        let in_folder = read_text(&self.dirs.folder_path(serial))
            .ok()
            .flatten()
            .as_deref()
            .and_then(marker)
            .is_some_and(|s| other(&s));
        in_entries || in_folder
    }

    /// Brings the menu files up to date; returns whether they changed. A failure is logged.
    fn update_menu(&self) -> Option<bool> {
        let names = |serial: &str| self.names.as_ref().and_then(|n| n(serial));
        match owned_entries(&self.dirs).and_then(|e| write_menu(&self.dirs, &e, &names)) {
            Ok(changed) => Some(changed),
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not write the Phone menu"
                );
                None
            }
        }
    }

    /// [`Self::update_menu`], then a rebuild if it changed.
    fn refresh_menu(&self) {
        if self.update_menu() == Some(true) {
            self.rebuild.changed();
        }
    }

    /// True once an `end_sync` whose count matched has run since [`Self::start_session`].
    pub fn synced(&self) -> bool {
        self.synced
    }

    /// The phones that have `pkg` for `user`; see [`phones_with`].
    pub fn phones(&self, pkg: &str, user: u32) -> Vec<String> {
        phones_with(&self.dirs, pkg, user)
    }

    fn session_serial(&self) -> Result<String> {
        self.serial
            .clone()
            .context("no phone session (start_session first)")
    }

    /// Writes (or refreshes) `e`'s icon and the session phone's menu entry, then the menu.
    /// Counts toward the sync window even when it fails, since the phone counted it.
    pub fn upsert(&mut self, e: &AppEntry) -> Result<()> {
        self.seen = self.seen.saturating_add(1);
        check_pkg(&e.pkg)?;
        check_user(e.user)?;
        self.users.insert(e.user);
        self.keep.insert((e.pkg.clone(), e.user));
        let serial = self.session_serial()?;
        if self.blocked {
            bail!(
                "phone {serial:?} shares the menu tag {:?} with another phone; no entry",
                naming::tag(&serial)
            );
        }
        let stem = self.naming.app_id(&e.pkg, e.user, &serial);
        let path = self.dirs.entry_path(&stem);
        match read_owned(&path)? {
            Some(owned) if !owned.is_only(&serial) => bail!(
                "{} is another phone's entry; leaving it alone",
                path.display()
            ),
            None if path.exists() => bail!(
                "{} is not a Lantern entry; leaving it alone",
                path.display()
            ),
            _ => {}
        }
        let icon_path = self.dirs.icon_path(&stem);
        let mut changed = false;
        let icon = if is_png(&e.icon_png) {
            changed |= write_if_changed(&icon_path, &e.icon_png)?;
            Some(icon_path.as_path())
        } else {
            changed |= remove_file(&icon_path)?;
            None
        };
        let text = desktop_file(e, &serial, &self.naming, &self.dirs.exe, icon);
        changed |= write_if_changed(&path, text.as_bytes())?;
        // The entry first, then the menu that shows it.
        if !self.menu_ok {
            if let Some(menu) = self.update_menu() {
                changed |= menu;
                self.menu_ok = true;
            }
        }
        if changed {
            self.rebuild.changed();
        }
        Ok(())
    }

    /// The session's phone no longer has `pkg` for `user`: its entry and icon go (a no-op if
    /// there is none or it is not that phone's own), and with it the phone's folder once empty.
    pub fn remove(&mut self, pkg: &str, user: u32) -> Result<()> {
        self.seen = self.seen.saturating_add(1);
        check_pkg(pkg)?;
        check_user(user)?;
        self.keep.remove(&(pkg.to_owned(), user));
        let serial = self.session_serial()?;
        let path = self
            .dirs
            .entry_path(&self.naming.app_id(pkg, user, &serial));
        if read_owned(&path)?.is_some_and(|o| o.is_only(&serial)) {
            remove_entry(&self.dirs, &path)?;
            self.update_menu();
            self.rebuild.changed();
        }
        Ok(())
    }

    /// Ends the sync window and opens the next one (a companion relink or a profile change
    /// sends another full catalogue). If `count` matches what arrived, every entry of the
    /// session's phone for a user that appeared but did not send that app goes. Returns how
    /// many entries went.
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
            if !owned.is_only(&serial) {
                continue;
            }
            let stale = owned.app().is_some_and(|(pkg, user)| {
                users.contains(&user) && !keep.contains(&(pkg.to_owned(), user))
            });
            if stale {
                remove_entry(&self.dirs, &path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::info!(serial, removed, "pruned catalogue entries");
            self.update_menu();
            self.rebuild.changed();
        }
        Ok(removed)
    }

    /// Removes `serial`'s entries, their icons and its folder (undo-setup); a Plan 3 shared
    /// entry only loses `serial` from its list. With no entry left the Phone menu goes too.
    /// Returns how many entries `serial` came off.
    pub fn remove_serial(&mut self, serial: &str) -> Result<usize> {
        let mut removed = 0;
        for (path, owned) in owned_entries(&self.dirs)? {
            if !owned.serials.iter().any(|s| s == serial) {
                continue;
            }
            let left: Vec<String> = owned
                .serials
                .iter()
                .filter(|s| *s != serial)
                .cloned()
                .collect();
            if left.is_empty() {
                remove_entry(&self.dirs, &path)?;
            } else {
                let text =
                    std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
                let text = with_serials(&String::from_utf8_lossy(&text), &left);
                write_if_changed(&path, text.as_bytes())?;
            }
            removed += 1;
        }
        // A phone that had nothing here leaves every file alone.
        let own_folder = read_text(&self.dirs.folder_path(serial))?
            .as_deref()
            .and_then(marker)
            .is_some_and(|s| s == serial);
        if removed == 0 && !own_folder {
            return Ok(0);
        }
        let names = |serial: &str| self.names.as_ref().and_then(|n| n(serial));
        write_menu(&self.dirs, &owned_entries(&self.dirs)?, &names)?;
        self.rebuild.changed();
        Ok(removed)
    }
}

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The phone that keeps the legacy names.
    pub primary: Option<String>,
    /// Per-phone entries written.
    pub written: usize,
    /// Legacy entries removed.
    pub removed: usize,
}

/// The phone that keeps the legacy names (decision 1): the most legacy entries pinned in
/// Plasma (`pins`: the `launchers=` lines of `plasma-org.kde.plasma.desktop-appletsrc`, each
/// `applications:<stem>.desktop` counting for every phone of that entry), then the most
/// entries, then the smallest serial. `legacy` is (stem, phones) per entry.
pub fn choose_primary(legacy: &[(String, Vec<String>)], pins: Option<&str>) -> Option<String> {
    let mut pinned: HashMap<&str, usize> = HashMap::new();
    for item in pins
        .unwrap_or("")
        .lines()
        .filter_map(|l| l.trim().strip_prefix("launchers="))
        .flat_map(|v| v.split(','))
    {
        if let Some(stem) = item
            .trim()
            .strip_prefix("applications:")
            .and_then(|i| i.strip_suffix(".desktop"))
        {
            *pinned.entry(stem).or_default() += 1;
        }
    }
    let mut stats: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (stem, serials) in legacy {
        let pins = pinned.get(stem.as_str()).copied().unwrap_or(0);
        let serials: BTreeSet<&String> = serials.iter().collect();
        for serial in serials {
            let s = stats.entry(serial).or_default();
            s.0 += pins;
            s.1 += 1;
        }
    }
    stats
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(serial, _)| serial.to_owned())
}

/// The one-time move to per-phone entries; a no-op once `menu.json` records it.
///
/// 1. Lantern's `lantern.*.desktop` files (with `X-Lantern-Serial=`) are listed; their phones
///    are `X-Lantern-Serials`, else `[X-Lantern-Serial]`. One without a valid package/user is
///    left as it is.
/// 2. The primary is [`choose_primary`] over them.
/// 3. For each file and each of its phones the per-phone entry is written (`Name` verbatim, the
///    icon copied to the new name), the old file itself last when it is a target; then the old
///    file and icon go unless one is a target. A target that is someone else's file keeps the
///    old file. A phone whose tag another phone (the primary, else the smallest serial) has is
///    dropped, and the old file stays as it is.
/// 4. The menu files are rewritten, and `menu.json` last: a run cut short starts over and ends
///    with the same files.
pub fn migrate(dirs: &Dirs, pins: Option<&str>) -> Result<Migration> {
    if Naming::migrated(&dirs.state) {
        return Ok(Migration {
            primary: Naming::load(&dirs.state).primary,
            written: 0,
            removed: 0,
        });
    }
    let mut files = Vec::new();
    for (path, owned) in owned_entries(dirs)? {
        match owned.app() {
            Some((pkg, user)) => {
                let (pkg, user) = (pkg.to_owned(), user);
                files.push((path, owned, pkg, user));
            }
            None => tracing::warn!(
                path = %path.display(),
                "Lantern entry without a valid package/user; leaving it as it is"
            ),
        }
    }
    // Per app, every phone that has it (whether its file is migrated yet or not), so a rerun
    // after a partial run picks the same primary.
    let mut apps: BTreeMap<(String, u32), BTreeSet<String>> = BTreeMap::new();
    for (_, owned, pkg, user) in &files {
        apps.entry((pkg.clone(), *user))
            .or_default()
            .extend(owned.serials.iter().cloned());
    }
    let legacy: Vec<(String, Vec<String>)> = apps
        .iter()
        .map(|((pkg, user), serials)| (app_id(pkg, *user), serials.iter().cloned().collect()))
        .collect();
    let primary = choose_primary(&legacy, pins);
    let naming = Naming {
        primary: primary.clone(),
    };
    // The primary keeps its tag, else the smallest serial does.
    let all: BTreeSet<&String> = apps.values().flatten().collect();
    let mut by_tag: BTreeMap<String, String> = BTreeMap::new();
    for serial in primary.iter().chain(all.iter().copied()) {
        by_tag
            .entry(naming::tag(serial))
            .or_insert_with(|| serial.clone());
    }
    let allowed = |serial: &String| by_tag.get(&naming::tag(serial)) == Some(serial);
    for serial in all {
        if !allowed(serial) {
            tracing::warn!(
                serial,
                tag = naming::tag(serial),
                "another phone has this phone's menu tag; its entries are dropped"
            );
        }
    }

    let (mut written, mut removed) = (0, 0);
    for (path, owned, pkg, user) in &files {
        let old_icon = dirs.icon_path(&stem_of(path));
        let has_old_icon = old_icon.is_file();
        let name = owned
            .raw_name
            .clone()
            .unwrap_or_else(|| escape_value(pkg, MAX_LABEL));
        let mut targets = Vec::new();
        // A phone refused for its tag keeps its old file (and icon) as they are.
        let mut keep_old = owned.serials.iter().any(|s| !allowed(s));
        // The old file itself, when it is a target, is rewritten last: a run cut short before
        // then still lists every phone there, so the rerun finds them all.
        let mut plan: Vec<(&String, String, PathBuf)> = owned
            .serials
            .iter()
            .filter(|s| allowed(s))
            .map(|serial| {
                let stem = naming.app_id(pkg, *user, serial);
                let target = dirs.entry_path(&stem);
                (serial, stem, target)
            })
            .collect();
        plan.sort_by_key(|(_, _, target)| target == path);
        for (serial, stem, target) in plan {
            if target != *path {
                match read_owned(&target)? {
                    Some(o) if !o.is_only(serial) => {
                        tracing::warn!(path = %target.display(), "another phone's entry; not migrating onto it");
                        keep_old = true;
                        continue;
                    }
                    None if target.exists() => {
                        tracing::warn!(path = %target.display(), "not a Lantern entry; not migrating onto it");
                        keep_old = true;
                        continue;
                    }
                    _ => {}
                }
            }
            let icon = dirs.icon_path(&stem);
            if has_old_icon && icon != old_icon {
                let bytes = std::fs::read(&old_icon)
                    .with_context(|| format!("read {}", old_icon.display()))?;
                write_if_changed(&icon, &bytes)?;
            }
            let icon = Some(icon).filter(|i| i.is_file());
            let text = entry_text(
                &name,
                pkg,
                *user,
                serial,
                &naming,
                &dirs.exe,
                icon.as_deref(),
            );
            write_if_changed(&target, text.as_bytes())?;
            written += 1;
            targets.push(target);
        }
        if !keep_old && !targets.contains(path) {
            // The icon first: a run cut short between the two still finds the entry.
            remove_file(&old_icon)?;
            remove_file(path)?;
            removed += 1;
        }
    }
    let names = companion::names_in(dirs.state.clone());
    write_menu(dirs, &owned_entries(dirs)?, &*names)?;
    naming.save(&dirs.state)?;
    Ok(Migration {
        primary,
        written,
        removed,
    })
}

/// The app's name from any of its entries (else `pkg`), the phones that have it, and the icon
/// of the first entry that has one.
fn app_of_entries(dirs: &Dirs, pkg: &str, user: u32) -> (String, Vec<String>, Option<PathBuf>) {
    let entries = entries_of(dirs, pkg, user);
    let name = entries
        .iter()
        .find_map(|(_, o)| o.name.clone().filter(|n| !n.is_empty()))
        .unwrap_or_else(|| pkg.to_owned());
    let icon = entries
        .iter()
        .map(|(path, _)| dirs.icon_path(&stem_of(path)))
        .find(|icon| icon.is_file());
    let phones: BTreeSet<String> = entries.into_iter().flat_map(|(_, o)| o.serials).collect();
    (name, phones.into_iter().collect(), icon)
}

/// The "Connect your …" notification for `lantern open [--user N] <pkg>` refused for want of
/// the right phone: (summary, body, icon). The app is named by any of its menu entries (else
/// the package) and the phones that have it by `names` (else their serials). `here` is the
/// connected phone that lacks the app, if any.
pub fn connect_notice(
    dirs: &Dirs,
    names: &dyn Fn(&str) -> Option<String>,
    pkg: &str,
    user: u32,
    here: Option<&str>,
) -> (String, String, Option<PathBuf>) {
    let (app, phones, icon) = app_of_entries(dirs, pkg, user);
    notice_for_phones(names, app, &phones, here, icon)
}

/// [`connect_notice`] for an open that named its phone (`lantern open --phone S`): it names
/// that phone only, whichever other phones have the app.
pub fn connect_notice_for_phone(
    dirs: &Dirs,
    names: &dyn Fn(&str) -> Option<String>,
    pkg: &str,
    user: u32,
    phone: &str,
) -> (String, String, Option<PathBuf>) {
    let (app, _, icon) = app_of_entries(dirs, pkg, user);
    notice_for_phones(names, app, &[phone.to_owned()], None, icon)
}

/// "Lantern is disconnected": the open was refused because the user disconnected the phone in
/// Lantern. The app is named (with its icon) as in its menu entries.
pub fn paused_notice(dirs: &Dirs, pkg: &str, user: u32) -> (String, String, Option<PathBuf>) {
    let (app, _, icon) = app_of_entries(dirs, pkg, user);
    (
        "Lantern is disconnected".to_owned(),
        format!("Click Connect in Lantern's tray menu to open {app}."),
        icon,
    )
}

fn notice_for_phones(
    names: &dyn Fn(&str) -> Option<String>,
    app: String,
    phones: &[String],
    here: Option<&str>,
    icon: Option<PathBuf>,
) -> (String, String, Option<PathBuf>) {
    let n = |serial: &str| names(serial).unwrap_or_else(|| serial.to_owned());
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
    use crate::naming::Naming;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A private temp tree standing in for `$XDG_DATA_HOME`/`$XDG_CONFIG_HOME`/`$XDG_STATE_HOME`.
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
            state: root.join("state/lantern"),
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

    /// [`quiet`] with the folder names of [`names`].
    fn named(dirs: Dirs) -> Catalog {
        let (mut cat, _) = quiet(dirs);
        cat.set_names(Arc::new(names));
        cat
    }

    /// Records `serial` as the primary phone (a finished migration).
    fn set_primary(dirs: &Dirs, serial: &str) {
        Naming {
            primary: Some(serial.into()),
        }
        .save(&dirs.state)
        .unwrap();
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

    /// `applications/<stem>.desktop`.
    fn file(dirs: &Dirs, stem: &str) -> PathBuf {
        dirs.applications.join(format!("{stem}.desktop"))
    }

    /// `icons/<stem>.png`.
    fn icon(dirs: &Dirs, stem: &str) -> PathBuf {
        dirs.icons.join(format!("{stem}.png"))
    }

    fn folder(dirs: &Dirs, serial: &str) -> PathBuf {
        dirs.directories.join(crate::naming::directory_file(serial))
    }

    fn menu(dirs: &Dirs) -> PathBuf {
        dirs.menus.join("lantern.menu")
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
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

    fn mtime(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    /// The submenu of `serial` as `lantern.menu` holds it.
    fn submenu(serial: &str) -> String {
        let t = crate::naming::tag(serial);
        format!(
            "<Menu><Name>lantern-{t}</Name><Directory>lantern-phone-{t}.directory</Directory>\
             <Include><Category>X-Lantern-{t}</Category></Include></Menu>"
        )
    }

    /// Names for "A", "B" and "H" only; any other serial has none.
    fn names(serial: &str) -> Option<String> {
        match serial {
            "A" => Some("Phone A".into()),
            "B" => Some("Phone B".into()),
            "H" => Some("Phone H".into()),
            _ => None,
        }
    }

    // --- per-phone entries and folders ---------------------------------------------------------

    #[test]
    fn per_phone_entries_and_folders() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let mut cat = named(dirs.clone());
        full_sync(&mut cat, "A", &[("com.s", 0)]);
        full_sync(&mut cat, "B", &[("com.s", 0)]);

        let a = read(&file(&dirs, "lantern.com.s"));
        assert_eq!(line(&a, "X-Lantern-Serial"), ["A"], "{a}");
        assert_eq!(
            line(&a, "Exec"),
            ["\"/usr/bin/lantern\" open --phone A com.s"]
        );
        assert_eq!(line(&a, "Categories"), ["X-Lantern-A;"]);
        assert_eq!(line(&a, "StartupWMClass"), ["lantern.com.s"]);
        assert_eq!(
            line(&a, "Icon"),
            [icon(&dirs, "lantern.com.s").to_str().unwrap()]
        );
        assert!(!a.contains("X-Lantern-Serials"), "{a}");

        let b = read(&file(&dirs, "lantern.com.s-pB"));
        assert_eq!(line(&b, "X-Lantern-Serial"), ["B"], "{b}");
        assert_eq!(line(&b, "StartupWMClass"), ["lantern.com.s-pB"]);
        assert_eq!(
            line(&b, "Exec"),
            ["\"/usr/bin/lantern\" open --phone B com.s"]
        );
        assert_eq!(line(&b, "Categories"), ["X-Lantern-B;"]);
        assert_eq!(
            line(&b, "Icon"),
            [icon(&dirs, "lantern.com.s-pB").to_str().unwrap()]
        );
        assert_eq!(std::fs::read(icon(&dirs, "lantern.com.s-pB")).unwrap(), PNG);
        assert_eq!(cat.phones("com.s", 0), ["A", "B"]);

        // One submenu per phone under "Phone", named by `names`, sorted by name.
        let m = read(&menu(&dirs));
        let (sa, sb) = (submenu("A"), submenu("B"));
        assert!(m.contains(&sa) && m.contains(&sb), "{m}");
        assert!(m.find(&sa) < m.find(&sb), "Phone A sorts first: {m}");
        assert!(
            m.contains("<Name>Phone</Name><Directory>lantern-phone.directory</Directory>"),
            "{m}"
        );
        assert_eq!(
            read(&folder(&dirs, "A")),
            "[Desktop Entry]\nType=Directory\nName=Phone A\nIcon=smartphone\nX-Lantern-Serial=A\n"
        );
        assert_eq!(
            read(&folder(&dirs, "B")),
            "[Desktop Entry]\nType=Directory\nName=Phone B\nIcon=smartphone\nX-Lantern-Serial=B\n"
        );
        assert_eq!(
            read(&dirs.directories.join("lantern-phone.directory")),
            "[Desktop Entry]\nType=Directory\nName=Phone\nIcon=smartphone\n"
        );

        // A session start rewrites the menu: B's new name sorts it first.
        cat.set_names(Arc::new(|s: &str| match s {
            "B" => Some("Aardvark <1>".into()),
            other => names(other),
        }));
        cat.start_session("B");
        let m = read(&menu(&dirs));
        assert!(m.find(&sb) < m.find(&sa), "{m}");
        assert_eq!(line(&read(&folder(&dirs, "B")), "Name"), ["Aardvark <1>"]);
    }

    #[test]
    fn exec_quotes_odd_serial() {
        let n = Naming {
            primary: Some("a b".into()),
        };
        let l = Path::new("/l");
        let f = desktop_file(&entry("com.a", 0), "a b", &n, l, None);
        assert_eq!(line(&f, "Exec"), ["\"/l\" open --phone \"a b\" com.a"]);
        assert_eq!(line(&f, "X-Lantern-Serial"), ["a b"]);
        // `"`, `$` are backslash-escaped inside the quotes, then the string escape doubles every
        // backslash (as for the exe).
        let f = desktop_file(&entry("com.a", 0), "a\"$b", &n, l, None);
        assert_eq!(
            line(&f, "Exec"),
            ["\"/l\" open --phone \"a\\\\\"\\\\$b\" com.a"]
        );
        // `[A-Za-z0-9._:-]` stays bare.
        let f = desktop_file(&entry("com.a", 10), "192.168.1.5:5555", &n, l, None);
        assert_eq!(
            line(&f, "Exec"),
            ["\"/l\" open --phone 192.168.1.5:5555 --user 10 com.a"]
        );
        assert_eq!(line(&f, "Categories"), ["X-Lantern-192_168_1_5_5555;"]);
        assert_eq!(
            line(&f, "StartupWMClass"),
            ["lantern.com.a-u10-p192_168_1_5_5555"]
        );
    }

    #[test]
    fn full_sync_keeps_other_phones_entries() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let files = [
            file(&dirs, "lantern.com.a"),
            file(&dirs, "lantern.com.x"),
            icon(&dirs, "lantern.com.a"),
            icon(&dirs, "lantern.com.x"),
            folder(&dirs, "A"),
        ];
        let (mut cat, _) = quiet(dirs.clone());
        assert_eq!(full_sync(&mut cat, "A", &[("com.a", 0), ("com.x", 0)]), 0);
        let old = SystemTime::now() - Duration::from_secs(3600);
        for f in &files {
            set_mtime(f, old);
        }
        let before: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
        assert_eq!(full_sync(&mut cat, "B", &[("com.b", 0)]), 0);
        for (f, bytes) in files.iter().zip(&before) {
            assert_eq!(&std::fs::read(f).unwrap(), bytes, "{f:?}");
            assert_eq!(mtime(f), old, "{f:?}");
        }
        assert!(file(&dirs, "lantern.com.b-pB").exists());
        assert_eq!(cat.phones("com.b", 0), ["B"]);
        assert_eq!(cat.phones("com.a", 0), ["A"]);
        assert!(cat.phones("com.none", 0).is_empty());
    }

    #[test]
    fn full_sync_prunes_only_its_phone() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let (s, s_icon) = (file(&dirs, "lantern.com.s"), icon(&dirs, "lantern.com.s"));
        let (sb, sb_icon) = (
            file(&dirs, "lantern.com.s-pB"),
            icon(&dirs, "lantern.com.s-pB"),
        );
        let (mut cat, _) = quiet(dirs.clone());
        full_sync(&mut cat, "A", &[("com.s", 0), ("com.a", 0)]);
        full_sync(&mut cat, "B", &[("com.s", 0), ("com.b", 0)]);
        assert_eq!(full_sync(&mut cat, "B", &[("com.b", 0)]), 1);
        assert!(!sb.exists() && !sb_icon.exists());
        assert!(s.exists() && s_icon.exists());
        assert_eq!(cat.phones("com.s", 0), ["A"]);
        assert_eq!(full_sync(&mut cat, "A", &[("com.a", 0)]), 1);
        assert!(!s.exists() && !s_icon.exists());
        assert!(cat.phones("com.s", 0).is_empty());
        assert!(file(&dirs, "lantern.com.b-pB").exists());
    }

    #[test]
    fn live_remove_drops_only_that_phone() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let (s, s_icon) = (file(&dirs, "lantern.com.s"), icon(&dirs, "lantern.com.s"));
        let sb = file(&dirs, "lantern.com.s-pB");
        let (mut cat, _) = quiet(dirs.clone());
        cat.start_session("A");
        cat.upsert(&entry("com.s", 0)).unwrap();
        cat.start_session("B");
        cat.upsert(&entry("com.s", 0)).unwrap();
        cat.remove("com.s", 0).unwrap();
        assert!(s.exists() && s_icon.exists() && !sb.exists());
        // B no longer has it: removing it again changes nothing.
        cat.remove("com.s", 0).unwrap();
        assert_eq!(cat.phones("com.s", 0), ["A"]);
        cat.start_session("A");
        cat.remove("com.s", 0).unwrap();
        assert!(!s.exists() && !s_icon.exists());
    }

    #[test]
    fn phones_with_lists_per_phone_and_legacy_entries() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let (mut cat, _) = quiet(dirs.clone());
        full_sync(&mut cat, "A", &[("com.s", 0)]);
        full_sync(&mut cat, "B", &[("com.s", 0), ("com.s", 10)]);
        // A Plan 3 shared file and a Plan 2 single-serial file.
        legacy(&dirs, "com.l", 0, "D", Some("C;D;"), "Old");
        legacy(&dirs, "com.p", 10, "E", None, "Plan two");
        // Someone else's file is no entry.
        std::fs::write(
            file(&dirs, "lantern.com.f"),
            "[Desktop Entry]\nName=F\nX-Lantern-Package=com.f\nX-Lantern-User=0\n",
        )
        .unwrap();
        assert_eq!(phones_with(&dirs, "com.s", 0), ["A", "B"]);
        assert_eq!(phones_with(&dirs, "com.s", 10), ["B"]);
        assert_eq!(phones_with(&dirs, "com.l", 0), ["C", "D"]);
        assert_eq!(phones_with(&dirs, "com.p", 10), ["E"]);
        assert!(phones_with(&dirs, "com.p", 0).is_empty());
        assert!(phones_with(&dirs, "com.f", 0).is_empty());
        assert!(phones_with(&dirs, "com.none", 0).is_empty());
        assert!(phones_with(&dirs, "../x", 0).is_empty());
        assert_eq!(cat.phones("com.s", 0), ["A", "B"]);
        assert_eq!(cat.phones("com.l", 0), ["C", "D"]);
        // The app's name comes from any of its entries.
        assert_eq!(app_name(&dirs, "com.s", 0), "Label com.s");
        assert_eq!(app_name(&dirs, "com.s", 10), "Label com.s (work)");
        assert_eq!(app_name(&dirs, "com.l", 0), "Old");
        assert_eq!(app_name(&dirs, "com.f", 0), "com.f");
    }

    #[test]
    fn first_session_becomes_primary() {
        let (_t, dirs) = tmp();
        assert!(!dirs.state.join("menu.json").exists());
        let (mut cat, _) = quiet(dirs.clone());
        cat.start_session("B");
        assert_eq!(Naming::load(&dirs.state).primary.as_deref(), Some("B"));
        assert!(Naming::migrated(&dirs.state));
        cat.upsert(&entry("com.s", 0)).unwrap();
        assert!(
            file(&dirs, "lantern.com.s").exists(),
            "the primary keeps legacy names"
        );
        // A later phone does not take it over.
        cat.start_session("A");
        cat.upsert(&entry("com.s", 0)).unwrap();
        assert_eq!(Naming::load(&dirs.state).primary.as_deref(), Some("B"));
        assert!(file(&dirs, "lantern.com.s-pA").exists());
        // A new catalogue reads it back.
        drop(cat);
        let (mut cat, _) = quiet(dirs.clone());
        cat.start_session("A");
        cat.upsert(&entry("com.t", 0)).unwrap();
        assert!(file(&dirs, "lantern.com.t-pA").exists());
    }

    #[test]
    fn folder_goes_with_the_phones_last_entry() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let mut cat = named(dirs.clone());
        full_sync(&mut cat, "A", &[("com.a", 0)]);
        full_sync(&mut cat, "B", &[("com.b", 0), ("com.c", 0)]);
        assert!(folder(&dirs, "B").exists());
        cat.start_session("B");
        cat.remove("com.b", 0).unwrap();
        assert!(folder(&dirs, "B").exists());
        assert!(read(&menu(&dirs)).contains(&submenu("B")));
        cat.remove("com.c", 0).unwrap();
        assert!(!folder(&dirs, "B").exists());
        let m = read(&menu(&dirs));
        assert!(
            !m.contains(&submenu("B")) && m.contains(&submenu("A")),
            "{m}"
        );
        assert!(folder(&dirs, "A").exists());
        // A full sync that prunes A's last entry takes the whole Phone menu.
        assert_eq!(
            full_sync(&mut cat, "A", &[]),
            0,
            "an empty sync names no user"
        );
        cat.start_session("A");
        cat.remove("com.a", 0).unwrap();
        assert!(!folder(&dirs, "A").exists());
        assert!(!menu(&dirs).exists());
        assert!(!dirs.directories.join("lantern-phone.directory").exists());
    }

    #[test]
    fn tag_collision_is_refused() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "P");
        let (mut cat, _) = quiet(dirs.clone());
        full_sync(&mut cat, "a:b", &[("com.s", 0)]);
        let first = file(&dirs, "lantern.com.s-pa_b");
        let first_folder = folder(&dirs, "a:b");
        let old = SystemTime::now() - Duration::from_secs(3600);
        set_mtime(&first, old);
        set_mtime(&first_folder, old);
        let (bytes, folder_bytes) = (std::fs::read(&first).unwrap(), read(&first_folder));
        assert_eq!(line(&folder_bytes, "X-Lantern-Serial"), ["a:b"]);

        cat.start_session("a_b");
        assert!(cat.upsert(&entry("com.s", 0)).is_err());
        assert!(
            cat.upsert(&entry("com.t", 0)).is_err(),
            "any app of that phone"
        );
        assert_eq!(cat.end_sync(2).unwrap(), 0);
        cat.remove("com.s", 0).unwrap();
        assert_eq!(cat.remove_serial("a_b").unwrap(), 0);
        assert_eq!(std::fs::read(&first).unwrap(), bytes);
        assert_eq!(mtime(&first), old);
        assert_eq!(read(&first_folder), folder_bytes);
        assert_eq!(mtime(&first_folder), old);
        assert!(!file(&dirs, "lantern.com.t-pa_b").exists());
        assert_eq!(cat.phones("com.s", 0), ["a:b"]);
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

    fn primary_s() -> Naming {
        Naming {
            primary: Some("s".into()),
        }
    }

    #[test]
    fn desktop_file_escapes_label() {
        let mut label = "A\nB=C;%x \"q\" 🙂".to_owned();
        label.push_str(&"x".repeat(10_000));
        let e = AppEntry {
            label,
            ..entry("com.a", 0)
        };
        let f = desktop_file(&e, "s", &primary_s(), Path::new("/usr/bin/lantern"), None);
        let name = line(&f, "Name");
        assert_eq!(name.len(), 1, "{f}");
        assert!(name[0].starts_with("A\\nB=C;%x \"q\" 🙂x"), "{}", name[0]);
        assert!(
            name[0].chars().count() <= 200,
            "{}",
            name[0].chars().count()
        );
        assert!(!f.contains("A\nB"), "raw newline in {f}");
        assert!(!f.contains("Icon="));
        // Every line, in order.
        let keys: Vec<&str> = f.lines().map(|l| l.split('=').next().unwrap()).collect();
        assert_eq!(
            keys,
            [
                "[Desktop Entry]",
                "Type",
                "Version",
                "Name",
                "Exec",
                "Categories",
                "StartupWMClass",
                "StartupNotify",
                "X-Lantern-Serial",
                "X-Lantern-Package",
                "X-Lantern-User",
            ]
        );
        for want in [
            "Type=Application",
            "Version=1.5",
            "Exec=\"/usr/bin/lantern\" open --phone s com.a",
            "Categories=X-Lantern-s;",
            "StartupWMClass=lantern.com.a",
            "StartupNotify=true",
            "X-Lantern-Serial=s",
            "X-Lantern-Package=com.a",
            "X-Lantern-User=0",
        ] {
            assert!(f.lines().any(|l| l == want), "missing {want} in {f}");
        }
        let with_icon = desktop_file(
            &e,
            "s",
            &primary_s(),
            Path::new("/l"),
            Some(Path::new("/i.png")),
        );
        let keys: Vec<&str> = with_icon
            .lines()
            .map(|l| l.split('=').next().unwrap())
            .collect();
        assert_eq!(&keys[3..7], ["Name", "Exec", "Icon", "Categories"]);
    }

    #[test]
    fn label_escapes_backslash_tab_cr_and_drops_controls() {
        let e = AppEntry {
            label: "a\\b\tc\rd\u{7}e\u{0}f".into(),
            ..entry("com.a", 0)
        };
        let f = desktop_file(&e, "s", &primary_s(), Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["a\\\\b\\tc\\rdef"]);
        // Truncation never splits an escape: 199 x then a backslash gives no dangling `\`.
        let e = AppEntry {
            label: format!("{}\\", "x".repeat(199)),
            ..entry("com.a", 0)
        };
        let f = desktop_file(&e, "s", &primary_s(), Path::new("/l"), None);
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
            &primary_s(),
            Path::new("/l"),
            Some(Path::new("/i/x.png")),
        );
        assert_eq!(line(&f, "Name"), ["Files (work)"]);
        assert_eq!(line(&f, "Exec"), ["\"/l\" open --phone s --user 10 com.f"]);
        assert_eq!(line(&f, "Icon"), ["/i/x.png"]);
        assert_eq!(line(&f, "StartupWMClass"), ["lantern.com.f-u10"]);
        assert_eq!(line(&f, "X-Lantern-User"), ["10"]);
        // Another phone's entry gets the suffixed app_id.
        let f = desktop_file(&work, "t", &primary_s(), Path::new("/l"), None);
        assert_eq!(line(&f, "StartupWMClass"), ["lantern.com.f-u10-pt"]);
        let other = AppEntry {
            label: "Files".into(),
            work: false,
            ..entry("com.f", 11)
        };
        let f = desktop_file(&other, "s", &primary_s(), Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["Files (user 11)"]);
        let owner = AppEntry {
            label: "Files".into(),
            work: false,
            ..entry("com.f", 0)
        };
        assert_eq!(
            line(
                &desktop_file(&owner, "s", &primary_s(), Path::new("/l"), None),
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
        let f = desktop_file(&long, "s", &primary_s(), Path::new("/l"), None);
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
        let f = desktop_file(&e, "s", &primary_s(), Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["com.f (work)"]);
        let e = AppEntry {
            label: "\u{1}\u{2}".into(),
            work: false,
            ..entry("com.f", 0)
        };
        let f = desktop_file(&e, "s", &primary_s(), Path::new("/l"), None);
        assert_eq!(line(&f, "Name"), ["com.f"]);
    }

    #[test]
    fn exec_quotes_exe_with_space() {
        let f = desktop_file(
            &entry("com.a", 0),
            "s",
            &primary_s(),
            Path::new("/home/a b/lantern"),
            None,
        );
        assert_eq!(
            line(&f, "Exec"),
            ["\"/home/a b/lantern\" open --phone s com.a"]
        );
        // `"`, `` ` ``, `$`, `\` are backslash-escaped inside the quotes, then the string
        // escape doubles every backslash; `%` is doubled.
        let f = desktop_file(
            &entry("com.a", 0),
            "s",
            &primary_s(),
            Path::new("/o\"d`d$d\\d%d/lantern"),
            None,
        );
        assert_eq!(
            line(&f, "Exec"),
            ["\"/o\\\\\"d\\\\`d\\\\$d\\\\\\\\d%%d/lantern\" open --phone s com.a"]
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
        let path = file(&dirs, "lantern.com.a");
        let ic = icon(&dirs, "lantern.com.a");
        let apps = dirs.applications.clone();
        let (mut cat, _) = quiet(dirs);
        cat.start_session("ser");
        cat.upsert(&entry("com.a", 0)).unwrap();
        assert_eq!(std::fs::read(&ic).unwrap(), PNG);
        let f = read(&path);
        assert_eq!(line(&f, "Icon"), [ic.to_str().unwrap()]);
        assert_eq!(line(&f, "Name"), ["Label com.a"]);
        // No temp files are left behind.
        assert_eq!(std::fs::read_dir(&apps).unwrap().count(), 1);
        // A non-PNG icon means no icon: the old one is removed and Icon= dropped.
        cat.upsert(&AppEntry {
            icon_png: b"GIF89a".to_vec(),
            ..entry("com.a", 0)
        })
        .unwrap();
        assert!(!ic.exists());
        assert!(!read(&path).contains("Icon="));
    }

    #[test]
    fn upsert_skips_unchanged_bytes() {
        let (_t, dirs) = tmp();
        let path = file(&dirs, "lantern.com.a");
        let ic = icon(&dirs, "lantern.com.a");
        let (mut cat, _) = quiet(dirs);
        cat.start_session("s");
        cat.upsert(&entry("com.a", 0)).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        for p in [&path, &ic] {
            set_mtime(p, old);
        }
        cat.upsert(&entry("com.a", 0)).unwrap();
        for p in [&path, &ic] {
            assert_eq!(mtime(p), old, "{p:?}");
        }
        cat.upsert(&AppEntry {
            label: "New".into(),
            ..entry("com.a", 0)
        })
        .unwrap();
        assert_ne!(mtime(&path), old);
        assert_eq!(mtime(&ic), old);
    }

    #[test]
    fn full_sync_prunes_uninstalled() {
        let (_t, dirs) = tmp();
        let (a, b) = (file(&dirs, "lantern.com.a"), file(&dirs, "lantern.com.b"));
        let b_icon = icon(&dirs, "lantern.com.b");
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
        set_primary(&dirs, "A");
        let (a, w) = (
            file(&dirs, "lantern.com.a"),
            file(&dirs, "lantern.com.w-u10"),
        );
        let wb = file(&dirs, "lantern.com.w-u10-pB");
        let (mut cat, _) = quiet(dirs);
        full_sync(&mut cat, "A", &[("com.a", 0), ("com.w", 10)]);
        full_sync(&mut cat, "B", &[("com.b", 0), ("com.w", 10)]);
        // B's work profile is paused: its next full sync has only user 0's apps.
        assert_eq!(full_sync(&mut cat, "B", &[("com.b", 0)]), 0);
        assert_eq!(cat.phones("com.w", 10), ["A", "B"]);
        // So is A's.
        assert_eq!(full_sync(&mut cat, "A", &[("com.a", 0)]), 0);
        assert!(a.exists() && w.exists() && wb.exists());
        assert_eq!(cat.phones("com.w", 10), ["A", "B"]);
    }

    #[test]
    fn partial_sync_prunes_nothing() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let b = file(&dirs, "lantern.com.b-pB");
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
        let a = file(&dirs, "lantern.com.a-u10");
        let ic = icon(&dirs, "lantern.com.a-u10");
        let (mut cat, _) = quiet(dirs);
        cat.start_session("s");
        cat.upsert(&entry("com.a", 10)).unwrap();
        cat.remove("com.a", 10).unwrap();
        assert!(!a.exists() && !ic.exists());
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
        assert_eq!(read(&foo), mine);
        assert!(other.exists());
    }

    #[test]
    fn remove_serial_drops_one_phone() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let (a, s) = (file(&dirs, "lantern.com.a"), file(&dirs, "lantern.com.s"));
        let (b, sb) = (
            file(&dirs, "lantern.com.b-pB"),
            file(&dirs, "lantern.com.s-pB"),
        );
        let phone_dir = dirs.directories.join("lantern-phone.directory");
        let mut cat = named(dirs.clone());
        full_sync(&mut cat, "A", &[("com.a", 0), ("com.s", 0)]);
        full_sync(&mut cat, "B", &[("com.b", 0), ("com.s", 0)]);
        assert!(menu(&dirs).exists() && phone_dir.exists());
        assert!(folder(&dirs, "A").exists() && folder(&dirs, "B").exists());
        assert_eq!(cat.remove_serial("A").unwrap(), 2);
        assert!(!a.exists() && !s.exists() && b.exists() && sb.exists());
        assert!(!icon(&dirs, "lantern.com.a").exists() && !icon(&dirs, "lantern.com.s").exists());
        assert!(icon(&dirs, "lantern.com.s-pB").exists());
        assert_eq!(cat.phones("com.s", 0), ["B"]);
        assert!(!folder(&dirs, "A").exists() && folder(&dirs, "B").exists());
        let m = read(&menu(&dirs));
        assert!(
            !m.contains(&submenu("A")) && m.contains(&submenu("B")),
            "{m}"
        );
        assert!(phone_dir.exists(), "other entries remain: the menu stays");
        assert_eq!(cat.remove_serial("A").unwrap(), 0);
        assert_eq!(cat.remove_serial("B").unwrap(), 2);
        assert!(!b.exists() && !sb.exists() && !folder(&dirs, "B").exists());
        assert!(
            !menu(&dirs).exists() && !phone_dir.exists(),
            "no entries left: menu removed"
        );
    }

    #[test]
    fn remove_serial_without_names_keeps_folder_names() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let mut cat = named(dirs.clone());
        full_sync(&mut cat, "A", &[("com.a", 0)]);
        full_sync(&mut cat, "B", &[("com.b", 0)]);
        drop(cat);
        // undo-setup's catalogue may not know the names: B's folder keeps its own.
        let (mut cat, _) = quiet(dirs.clone());
        assert_eq!(cat.remove_serial("A").unwrap(), 1);
        assert_eq!(line(&read(&folder(&dirs, "B")), "Name"), ["Phone B"]);
    }

    #[test]
    fn remove_serial_takes_a_phone_off_a_legacy_shared_entry() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.l", 0, "A", Some("A;B;"), "Old");
        let (mut cat, _) = quiet(dirs.clone());
        assert_eq!(cat.remove_serial("A").unwrap(), 1);
        assert_eq!(phones_with(&dirs, "com.l", 0), ["B"]);
        assert_eq!(cat.remove_serial("B").unwrap(), 1);
        assert!(!file(&dirs, "lantern.com.l").exists());
    }

    #[test]
    fn remove_serial_of_an_unknown_phone_writes_nothing() {
        let (_t, dirs) = tmp();
        // Plan 3 files and menu, not migrated yet (e.g. undo-setup before lanternd ran).
        legacy(&dirs, "com.l", 0, "A", Some("A;B;"), "Old");
        std::fs::create_dir_all(&dirs.menus).unwrap();
        std::fs::create_dir_all(&dirs.directories).unwrap();
        let old_menu = "<Menu><Include><Category>X-Lantern</Category></Include></Menu>\n";
        let files = [
            (menu(&dirs), old_menu),
            (dirs.directories.join("lantern-phone.directory"), DIRECTORY),
        ];
        let stamp = SystemTime::now() - Duration::from_secs(3600);
        for (path, text) in &files {
            std::fs::write(path, text).unwrap();
            set_mtime(path, stamp);
        }
        let before = snapshot(&dirs);
        let (mut cat, runs) = quiet(dirs.clone());
        assert_eq!(cat.remove_serial("fake-serial").unwrap(), 0);
        // Only legacy entries: the menu is left alone, even when rewritten.
        assert!(!ensure_menu(&dirs, &[]).unwrap());
        drop(cat);
        assert_eq!(snapshot(&dirs), before);
        for (path, _) in &files {
            assert_eq!(mtime(path), stamp, "{path:?}");
        }
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn construction_writes_no_menu_files() {
        let (_t, dirs) = tmp();
        let (cat, runs) = quiet(dirs.clone());
        drop(cat);
        assert!(!menu(&dirs).exists());
        assert!(!dirs.directories.join("lantern-phone.directory").exists());
        assert!(!dirs.state.join("menu.json").exists());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn menu_and_directory_files() {
        let (_t, dirs) = tmp();
        let (mut cat, _) = quiet(dirs.clone());
        cat.start_session("s");
        assert!(!menu(&dirs).exists(), "no entry yet: no menu");
        cat.upsert(&entry("com.a", 0)).unwrap();
        let m = read(&menu(&dirs));
        for want in [
            "<Menu>",
            "<Name>Applications</Name>",
            "<Name>Phone</Name>",
            "<Directory>lantern-phone.directory</Directory>",
            &submenu("s"),
        ] {
            assert!(m.contains(want), "{want} missing in {m}");
        }
        assert!(!m.contains("<Category>X-Lantern</Category>"), "{m}");
        let d = read(&dirs.directories.join("lantern-phone.directory"));
        assert_eq!(
            d,
            "[Desktop Entry]\nType=Directory\nName=Phone\nIcon=smartphone\n"
        );
        // Without a name the folder is named by the serial.
        assert_eq!(line(&read(&folder(&dirs, "s")), "Name"), ["s"]);
    }

    #[test]
    fn ensure_menu_names_and_sorts_phones() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "A");
        let (mut cat, _) = quiet(dirs.clone());
        full_sync(&mut cat, "A", &[("com.a", 0)]);
        full_sync(&mut cat, "B", &[("com.b", 0)]);
        // A phone without an entry gets no submenu even when named.
        let phones = [
            ("A".to_owned(), Some("Zed".to_owned())),
            ("B".to_owned(), Some("Al & <b>".to_owned())),
            ("C".to_owned(), Some("Cee".to_owned())),
        ];
        ensure_menu(&dirs, &phones).unwrap();
        let m = read(&menu(&dirs));
        assert!(m.find(&submenu("B")) < m.find(&submenu("A")), "{m}");
        assert!(!m.contains(&submenu("C")), "{m}");
        assert!(!folder(&dirs, "C").exists());
        assert_eq!(line(&read(&folder(&dirs, "B")), "Name"), ["Al & <b>"]);
        // Unchanged: nothing written.
        assert!(!ensure_menu(&dirs, &phones).unwrap());
        // A foreign folder file at a phone's path stays.
        let foreign = "[Desktop Entry]\nType=Directory\nName=Mine\n";
        std::fs::write(folder(&dirs, "A"), foreign).unwrap();
        ensure_menu(&dirs, &phones).unwrap();
        assert_eq!(read(&folder(&dirs, "A")), foreign);
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
            Some(PathBuf::from("/s")),
            Path::new("/h"),
            PathBuf::from("/x"),
        );
        assert_eq!(d.applications, Path::new("/d/applications"));
        assert_eq!(d.icons, Path::new("/d/lantern/icons"));
        assert_eq!(d.menus, Path::new("/c/menus/applications-merged"));
        assert_eq!(d.directories, Path::new("/d/desktop-directories"));
        assert_eq!(d.state, Path::new("/s/lantern"));
        assert_eq!(d.exe, Path::new("/x"));
        // Unset or relative values fall back to the defaults (XDG Base Directory spec).
        let d = Dirs::resolve(
            None,
            Some(PathBuf::from("rel")),
            Some(PathBuf::from("rel")),
            Path::new("/h"),
            "/x".into(),
        );
        assert_eq!(d.applications, Path::new("/h/.local/share/applications"));
        assert_eq!(d.menus, Path::new("/h/.config/menus/applications-merged"));
        assert_eq!(d.state, Path::new("/h/.local/state/lantern"));
    }

    // --- notices -------------------------------------------------------------------------------

    /// Plants `com.a`'s per-phone entries, labelled "Alpha", one per serial (the first primary).
    fn plant(dirs: &Dirs, serials: &[&str]) {
        let _ = std::fs::remove_dir_all(&dirs.applications);
        std::fs::create_dir_all(&dirs.applications).unwrap();
        let naming = Naming {
            primary: Some(serials[0].into()),
        };
        let e = AppEntry {
            label: "Alpha".into(),
            ..entry("com.a", 0)
        };
        for serial in serials {
            let stem = naming.app_id("com.a", 0, serial);
            let text = desktop_file(&e, serial, &naming, &dirs.exe, None);
            std::fs::write(file(dirs, &stem), text).unwrap();
        }
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
        let (s, body, ic) = connect_notice(&dirs, &|_| None, "com.a", 0, None);
        assert_eq!(s, "Connect your phone");
        assert_eq!(body, "com.a opens when your phone is connected by USB.");
        assert_eq!(ic, None);
        // Only a non-primary phone has the app: its entry still names it and gives the icon.
        set_primary(&dirs, "P");
        let (mut cat, _) = quiet(dirs.clone());
        cat.start_session("s");
        cat.upsert(&AppEntry {
            label: "Sett\\ings".into(),
            ..entry("com.a", 0)
        })
        .unwrap();
        let (_, body, ic) = connect_notice(&dirs, &|_| None, "com.a", 0, None);
        assert_eq!(body, "Sett\\ings opens when your s is connected by USB.");
        assert_eq!(ic, Some(icon(&dirs, "lantern.com.a-ps")));
    }

    /// An open that named its phone (`--phone A`) gets a notice for A only, even when other
    /// phones have the app too (never "Connect A or B" while B is live).
    #[test]
    fn connect_notice_for_a_named_phone() {
        let (_t, dirs) = tmp();
        plant(&dirs, &["A", "B"]);
        let (s, body, _) = connect_notice_for_phone(&dirs, &names, "com.a", 0, "A");
        assert_eq!(
            (s, body),
            (
                "Connect your Phone A".to_owned(),
                "Alpha opens when your Phone A is connected by USB.".to_owned()
            )
        );
        // A phone without the app (or entry) is still the one named.
        let (s, body, _) = connect_notice_for_phone(&dirs, &names, "com.zz", 0, "s9");
        assert_eq!(s, "Connect your s9");
        assert_eq!(body, "com.zz opens when your s9 is connected by USB.");
    }

    #[test]
    fn paused_notice_names_the_app() {
        let (_t, dirs) = tmp();
        let (s, body, ic) = paused_notice(&dirs, "com.a", 0);
        assert_eq!(s, "Lantern is disconnected");
        assert_eq!(body, "Click Connect in Lantern's tray menu to open com.a.");
        assert_eq!(ic, None);
        let (mut cat, _) = quiet(dirs.clone());
        cat.start_session("s");
        cat.upsert(&AppEntry {
            label: "Settings".into(),
            ..entry("com.a", 0)
        })
        .unwrap();
        let (_, body, ic) = paused_notice(&dirs, "com.a", 0);
        assert_eq!(
            body,
            "Click Connect in Lantern's tray menu to open Settings."
        );
        assert_eq!(ic, Some(icon(&dirs, "lantern.com.a")));
    }

    // --- the one-time migration ----------------------------------------------------------------

    /// Plants a Plan 3 (`serials` given) or Plan 2 entry for `pkg`/`user` under the legacy name,
    /// with an icon whose bytes name the package.
    fn legacy(dirs: &Dirs, pkg: &str, user: u32, marker: &str, serials: Option<&str>, name: &str) {
        let stem = app_id(pkg, user);
        let ic = icon(dirs, &stem);
        std::fs::create_dir_all(&dirs.applications).unwrap();
        std::fs::create_dir_all(&dirs.icons).unwrap();
        std::fs::write(&ic, legacy_icon(pkg, user)).unwrap();
        let user_arg = match user {
            0 => String::new(),
            n => format!(" --user {n}"),
        };
        let mut text = format!(
            "[Desktop Entry]\nType=Application\nVersion=1.5\nName={name}\n\
             Exec=\"/usr/bin/lantern\" open{user_arg} {pkg}\nIcon={}\nCategories=X-Lantern;\n\
             StartupWMClass={stem}\nStartupNotify=true\nX-Lantern-Serial={marker}\n",
            ic.display()
        );
        if let Some(serials) = serials {
            text += &format!("X-Lantern-Serials={serials}\n");
        }
        text += &format!("X-Lantern-Package={pkg}\nX-Lantern-User={user}\n");
        std::fs::write(file(dirs, &stem), text).unwrap();
    }

    fn legacy_icon(pkg: &str, user: u32) -> Vec<u8> {
        let mut bytes = PNG.to_vec();
        bytes.extend(format!("{pkg}:{user}").bytes());
        bytes
    }

    /// Every file under the tree (relative path → bytes), `menu.json` excluded.
    fn snapshot(dirs: &Dirs) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for dir in [
            &dirs.applications,
            &dirs.icons,
            &dirs.directories,
            &dirs.menus,
        ] {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in rd {
                let p = e.unwrap().path();
                out.insert(p.display().to_string(), std::fs::read(&p).unwrap());
            }
        }
        out
    }

    fn stems(dirs: &Dirs) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&dirs.applications)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn migrate_splits_shared_entry_per_phone() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.s", 0, "B", Some("A;B;"), "Shared \\s one");
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("A"), "tie: the smallest serial");
        assert_eq!((m.written, m.removed), (2, 0));
        assert_eq!(Naming::load(&dirs.state).primary.as_deref(), Some("A"));
        assert!(Naming::migrated(&dirs.state));
        assert_eq!(
            stems(&dirs),
            ["lantern.com.s-pB.desktop", "lantern.com.s.desktop"]
        );
        let naming = Naming::load(&dirs.state);
        for (serial, stem) in [("A", "lantern.com.s"), ("B", "lantern.com.s-pB")] {
            let f = read(&file(&dirs, stem));
            let ic = icon(&dirs, stem);
            assert_eq!(line(&f, "Name"), ["Shared \\s one"], "Name verbatim: {f}");
            assert_eq!(
                line(&f, "Exec"),
                [format!("\"/usr/bin/lantern\" open --phone {serial} com.s").as_str()]
            );
            assert_eq!(line(&f, "Icon"), [ic.to_str().unwrap()]);
            assert_eq!(line(&f, "X-Lantern-Serial"), [serial]);
            assert!(!f.contains("X-Lantern-Serials"), "{f}");
            assert_eq!(
                std::fs::read(&ic).unwrap(),
                legacy_icon("com.s", 0),
                "{stem}"
            );
            // The same text a sync writes (Name aside).
            let e = AppEntry {
                label: "Shared  one".into(),
                ..entry("com.s", 0)
            };
            let synced = desktop_file(&e, serial, &naming, &dirs.exe, Some(&ic));
            assert_eq!(
                f.lines()
                    .filter(|l| !l.starts_with("Name="))
                    .collect::<Vec<_>>(),
                synced
                    .lines()
                    .filter(|l| !l.starts_with("Name="))
                    .collect::<Vec<_>>()
            );
        }
        let menu_text = read(&menu(&dirs));
        assert!(
            menu_text.contains(&submenu("A")) && menu_text.contains(&submenu("B")),
            "{menu_text}"
        );
        assert!(folder(&dirs, "A").exists() && folder(&dirs, "B").exists());
    }

    #[test]
    fn migrate_renames_other_phones_only_entries() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.a", 0, "A", Some("A;"), "Alpha");
        legacy(&dirs, "com.a2", 0, "A", Some("A;"), "Alpha 2");
        legacy(&dirs, "com.b", 0, "B", Some("B;"), "Beta");
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("A"), "most entries");
        assert_eq!((m.written, m.removed), (3, 1));
        assert_eq!(
            stems(&dirs),
            [
                "lantern.com.a.desktop",
                "lantern.com.a2.desktop",
                "lantern.com.b-pB.desktop"
            ]
        );
        assert!(!icon(&dirs, "lantern.com.b").exists());
        assert_eq!(
            std::fs::read(icon(&dirs, "lantern.com.b-pB")).unwrap(),
            legacy_icon("com.b", 0)
        );
        assert_eq!(
            line(&read(&file(&dirs, "lantern.com.b-pB")), "Name"),
            ["Beta"]
        );
    }

    #[test]
    fn migrate_plan2_single_serial_file() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.a", 0, "A", None, "Alpha");
        legacy(&dirs, "com.a2", 0, "A", None, "Alpha 2");
        legacy(&dirs, "com.p", 10, "B", None, "Work (work)");
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("A"));
        assert_eq!((m.written, m.removed), (3, 1));
        assert!(!file(&dirs, "lantern.com.p-u10").exists());
        assert!(!icon(&dirs, "lantern.com.p-u10").exists());
        let f = read(&file(&dirs, "lantern.com.p-u10-pB"));
        assert_eq!(
            line(&f, "Exec"),
            ["\"/usr/bin/lantern\" open --phone B --user 10 com.p"]
        );
        assert_eq!(line(&f, "StartupWMClass"), ["lantern.com.p-u10-pB"]);
        assert_eq!(line(&f, "Name"), ["Work (work)"]);
        assert!(icon(&dirs, "lantern.com.p-u10-pB").exists());
        let a = read(&file(&dirs, "lantern.com.a"));
        assert_eq!(
            line(&a, "Exec"),
            ["\"/usr/bin/lantern\" open --phone A com.a"]
        );
        assert_eq!(line(&a, "Categories"), ["X-Lantern-A;"]);
    }

    #[test]
    fn migrate_never_touches_foreign_files() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.s", 0, "A", Some("A;B;"), "Shared");
        legacy(&dirs, "com.a", 0, "A", Some("A;"), "Alpha");
        // B's target of com.t is someone else's file: B's entry stays where it was.
        legacy(&dirs, "com.t", 0, "B", Some("B;"), "Tee");
        std::fs::create_dir_all(&dirs.directories).unwrap();
        let foreign = [
            (
                file(&dirs, "lantern.foreign"),
                "[Desktop Entry]\nName=F\nX-Lantern-Package=foreign\nX-Lantern-User=0\n",
            ),
            (
                file(&dirs, "dev.lantern.Lantern"),
                "[Desktop Entry]\nName=Lantern\nExec=lantern\n",
            ),
            (
                file(&dirs, "org.kde.x"),
                "[Desktop Entry]\nName=X\nX-Lantern-Serial=A\n",
            ),
            (
                file(&dirs, "lantern.com.t-pB"),
                "[Desktop Entry]\nName=Hand-made\n",
            ),
            (
                dirs.directories.join("lantern-phone-x.directory"),
                "[Desktop Entry]\nType=Directory\nName=Mine\n",
            ),
            (
                dirs.directories.join("other.directory"),
                "[Desktop Entry]\nType=Directory\nName=Other\n",
            ),
            (icon(&dirs, "lantern.stray"), "not even a png"),
            (icon(&dirs, "lantern.foreign"), "foreign icon"),
        ];
        let old = SystemTime::now() - Duration::from_secs(7200);
        for (path, text) in &foreign {
            std::fs::write(path, text).unwrap();
            set_mtime(path, old);
        }
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("A"));
        // The catalogue never touches them either, whatever the sessions do.
        let (mut cat, _) = quiet(dirs.clone());
        full_sync(&mut cat, "A", &[("com.a", 0)]);
        full_sync(&mut cat, "B", &[]);
        cat.remove_serial("A").unwrap();
        cat.remove_serial("B").unwrap();
        for (path, text) in &foreign {
            assert_eq!(read(path), *text, "{path:?}");
            assert_eq!(mtime(path), old, "{path:?}");
        }
    }

    #[test]
    fn migrate_keeps_an_entry_whose_target_is_foreign() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.a", 0, "A", None, "Alpha");
        legacy(&dirs, "com.t", 0, "B", None, "Tee");
        std::fs::write(
            file(&dirs, "lantern.com.t-pB"),
            "[Desktop Entry]\nName=Mine\n",
        )
        .unwrap();
        let before = std::fs::read(file(&dirs, "lantern.com.t")).unwrap();
        let m = migrate(&dirs, None).unwrap();
        assert_eq!((m.written, m.removed), (1, 0));
        assert_eq!(std::fs::read(file(&dirs, "lantern.com.t")).unwrap(), before);
        assert!(icon(&dirs, "lantern.com.t").exists());
        assert_eq!(
            read(&file(&dirs, "lantern.com.t-pB")),
            "[Desktop Entry]\nName=Mine\n"
        );
    }

    #[test]
    fn migrate_refuses_a_colliding_tag() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.p", 0, "P", None, "Primary");
        legacy(&dirs, "com.p2", 0, "P", None, "Primary 2");
        legacy(&dirs, "com.s", 0, "a:b", Some("P;a:b;a_b;"), "Shared");
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("P"));
        let f = read(&file(&dirs, "lantern.com.s-pa_b"));
        assert_eq!(
            line(&f, "X-Lantern-Serial"),
            ["a:b"],
            "the smaller serial keeps the tag"
        );
        assert_eq!(phones_with(&dirs, "com.s", 0), ["P", "a:b"]);
        assert_eq!(
            line(&read(&folder(&dirs, "a:b")), "X-Lantern-Serial"),
            ["a:b"]
        );
    }

    /// A phone refused for its tag keeps its legacy file and icon as they are, even when no
    /// other phone is listed in that file.
    #[test]
    fn migrate_keeps_a_refused_phones_files() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.p", 0, "P", None, "Primary");
        legacy(&dirs, "com.p2", 0, "P", None, "Primary 2");
        legacy(&dirs, "com.y", 0, "a:b", None, "Wins the tag");
        legacy(&dirs, "com.x", 0, "a_b", None, "Loses the tag");
        let before = std::fs::read(file(&dirs, "lantern.com.x")).unwrap();
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("P"));
        assert_eq!(std::fs::read(file(&dirs, "lantern.com.x")).unwrap(), before);
        assert_eq!(
            std::fs::read(icon(&dirs, "lantern.com.x")).unwrap(),
            legacy_icon("com.x", 0)
        );
        assert!(!file(&dirs, "lantern.com.x-pa_b").exists());
        assert_eq!(
            line(
                &read(&file(&dirs, "lantern.com.y-pa_b")),
                "X-Lantern-Serial"
            ),
            ["a:b"]
        );
    }

    /// A shared entry is split with the old file (the primary's target) rewritten last: a run
    /// that fails on that write has already written every other phone's file, and the rerun
    /// finishes with both phones' entries.
    #[test]
    fn migrate_split_cut_short_keeps_every_phone() {
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.s", 0, "B", Some("A;B;"), "Shared");
        // The in-place rewrite of `lantern.com.s.desktop` fails: its temporary file's name is
        // taken by a directory.
        let blocker = dirs
            .applications
            .join(format!(".lantern.com.s.desktop.{}.tmp", std::process::id()));
        std::fs::create_dir_all(&blocker).unwrap();
        assert!(migrate(&dirs, None).is_err());
        assert!(!Naming::migrated(&dirs.state));
        assert_eq!(
            line(&read(&file(&dirs, "lantern.com.s")), "X-Lantern-Serials"),
            ["A;B;"],
            "the shared file is still whole"
        );
        assert_eq!(
            line(&read(&file(&dirs, "lantern.com.s-pB")), "X-Lantern-Serial"),
            ["B"],
            "B's file came first"
        );
        std::fs::remove_dir(&blocker).unwrap();
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("A"));
        assert_eq!(phones_with(&dirs, "com.s", 0), ["A", "B"]);
        for (serial, stem) in [("A", "lantern.com.s"), ("B", "lantern.com.s-pB")] {
            let f = read(&file(&dirs, stem));
            assert_eq!(line(&f, "X-Lantern-Serial"), [serial]);
            assert!(!f.contains("X-Lantern-Serials"), "{f}");
        }
    }

    #[test]
    fn migrate_is_idempotent_after_a_partial_run() {
        let (_t, dirs) = tmp();
        let plant_all = |dirs: &Dirs| {
            legacy(dirs, "com.s", 0, "B", Some("A;B;"), "Shared");
            legacy(dirs, "com.b", 0, "B", Some("B;"), "Beta");
        };
        plant_all(&dirs);
        legacy(&dirs, "com.a", 0, "A", Some("A;"), "Alpha");
        legacy(&dirs, "com.a2", 0, "A", Some("A;"), "Alpha 2");
        let first = migrate(&dirs, None).unwrap();
        let after = snapshot(&dirs);
        // A crash before menu.json: the run starts over with some legacy files back.
        std::fs::remove_file(dirs.state.join("menu.json")).unwrap();
        plant_all(&dirs);
        let second = migrate(&dirs, None).unwrap();
        assert_eq!(first.primary, second.primary);
        assert_eq!(snapshot(&dirs), after);
        assert!(Naming::migrated(&dirs.state));
        // No duplicates: one file per (phone, app).
        let mut seen = std::collections::HashSet::new();
        for (path, bytes) in &after {
            if !path.ends_with(".desktop") {
                continue;
            }
            let t = String::from_utf8_lossy(bytes);
            assert!(!t.contains("X-Lantern-Serials"), "{t}");
            let key = (
                line(&t, "X-Lantern-Serial").concat(),
                line(&t, "X-Lantern-Package").concat(),
                line(&t, "X-Lantern-User").concat(),
            );
            assert!(seen.insert(key.clone()), "duplicate {key:?}");
        }
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn migrate_runs_once() {
        let (_t, dirs) = tmp();
        set_primary(&dirs, "Z");
        legacy(&dirs, "com.s", 0, "A", Some("A;B;"), "Shared");
        let (f, ic) = (file(&dirs, "lantern.com.s"), icon(&dirs, "lantern.com.s"));
        let old = SystemTime::now() - Duration::from_secs(3600);
        set_mtime(&f, old);
        set_mtime(&ic, old);
        let before = snapshot(&dirs);
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary.as_deref(), Some("Z"));
        assert_eq!((m.written, m.removed), (0, 0));
        assert_eq!(snapshot(&dirs), before);
        assert_eq!(mtime(&f), old);
        assert!(!menu(&dirs).exists());
    }

    #[test]
    fn migrate_with_nothing_records_no_primary() {
        let (_t, dirs) = tmp();
        let m = migrate(&dirs, None).unwrap();
        assert_eq!(m.primary, None);
        assert!(Naming::migrated(&dirs.state));
        assert!(!menu(&dirs).exists());
        // The first session then becomes primary.
        let (mut cat, _) = quiet(dirs.clone());
        cat.start_session("B");
        assert_eq!(Naming::load(&dirs.state).primary.as_deref(), Some("B"));
    }

    #[test]
    fn primary_is_most_pinned_then_most_entries_then_smallest() {
        let l = |v: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
            v.iter()
                .map(|(stem, s)| (stem.to_string(), s.iter().map(|x| x.to_string()).collect()))
                .collect()
        };
        let legacy_list = l(&[
            ("lantern.com.a", &["A"]),
            ("lantern.com.a2", &["A"]),
            ("lantern.com.b", &["B"]),
        ]);
        let pins = "[Containments][2][Applets][5][Configuration][General]\n\
                    launchers=applications:lantern.com.b.desktop,preferred://browser\n";
        assert_eq!(
            choose_primary(&legacy_list, Some(pins)).as_deref(),
            Some("B")
        );
        assert_eq!(choose_primary(&legacy_list, None).as_deref(), Some("A"));
        // Pins of other apps (or other files) do not count.
        let other =
            "launchers=applications:org.kde.dolphin.desktop,file:///lantern.com.b.desktop\n";
        assert_eq!(
            choose_primary(&legacy_list, Some(other)).as_deref(),
            Some("A")
        );
        // A tie goes to the smallest serial.
        let tie = l(&[("x", &["B"]), ("y", &["A"])]);
        assert_eq!(choose_primary(&tie, None).as_deref(), Some("A"));
        // Pins in several task managers add up.
        let two = "launchers=applications:lantern.com.b.desktop\n\
                   launchers=applications:lantern.com.a.desktop,applications:lantern.com.a2.desktop\n\
                   launchers=applications:lantern.com.b.desktop\n";
        assert_eq!(
            choose_primary(&legacy_list, Some(two)).as_deref(),
            Some("A")
        );
        assert_eq!(choose_primary(&[], Some(pins)), None);

        // The migration follows it: B keeps the old names.
        let (_t, dirs) = tmp();
        legacy(&dirs, "com.a", 0, "A", None, "Alpha");
        legacy(&dirs, "com.a2", 0, "A", None, "Alpha 2");
        legacy(&dirs, "com.b", 0, "B", None, "Beta");
        let m = migrate(&dirs, Some(pins)).unwrap();
        assert_eq!(m.primary.as_deref(), Some("B"));
        assert_eq!(
            stems(&dirs),
            [
                "lantern.com.a-pA.desktop",
                "lantern.com.a2-pA.desktop",
                "lantern.com.b.desktop"
            ]
        );
    }
}
