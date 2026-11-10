# Lantern Phones and Tray (Plan 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The KDE menu keeps the apps of every phone Sahaj has used. Switching phones no longer
rewrites the menu or breaks pins. A system tray icon from `lanternd` shows the connection state
and holds Lantern's actions and settings: Connect/Disconnect, Undo phone setup, Quit, clipboard
sync, the Lantern keyboard, and start at login. The settings live in one TOML file.

**Architecture:**
- **Catalogue:** keeps one `.desktop` file per app_id, as before. Each file lists the phones
  that have the app (`X-Lantern-Serials=a;b;`). A full sync from phone S only takes S off the
  entries it no longer lists; a file goes away when no phone lists it. The device record gains
  the phone's friendly name, which the "Connect your …" notices use.
- **Settings:** `~/.config/lantern/settings.toml`, read through `settings::SettingsHandle` (a
  `tokio::sync::watch` of typed values). The tray writes it, and hand edits are picked up within
  2 s.
  - Clipboard sync applies at once.
  - The keyboard setting rides on the session `hello` as an optional field and applies at the
    next connection.
- **Daemon control:** `lanternd`'s run loop takes `Command`s (Connect, Disconnect, UndoSetup,
  Quit) and publishes a `View` (phone state + name). The tray, the new `lantern connect` /
  `lantern disconnect` CLI and the tests all use that one path.
- **Tray:** a thin view written directly against zbus 5. It serves
  `org.kde.StatusNotifierItem` and `com.canonical.dbusmenu`, registers with
  `org.kde.StatusNotifierWatcher`, builds its menu from `View` + `Settings` with a pure
  function, and turns clicks into `Command`s or settings writes.

**Tech Stack:**
- Rust, using the crates in `host/lantern/Cargo.toml` plus two new ones. Both are verified to
  resolve and build with `--offline` against the current `Cargo.lock` (2026-11-10):
  - `zbus = { version = "5", default-features = false, features = ["tokio"] }` (5.19.0)
  - `toml = { version = "1", default-features = false, features = ["parse", "display", "serde"] }`
    (1.1.4)
  - Do not use `toml_edit`: its offline resolution needs `toml_writer` 1.2, which isn't in the
    registry.
- Java (helper) as in Plan 2.
- Desktop helpers that are on PATH: `kdialog`, `xdg-open`, `systemctl`, `dbus-daemon`,
  `busctl`, `notify-send`, `kbuildsycoca6`.

**Spec:** `docs/superpowers/specs/2026-11-07-lantern-design.md`. The binding parts are §2 goal 2,
§4, §8.1 (consented, reversible setup), §8.4 (menu entries while disconnected) and §9a (status
tray). **This plan overrides §9a's "One phone at a time … the previous phone's entries are
removed"** by Sahaj's decision of 2026-11-10 (`docs/HANDOFF.md`, "Next: Plan 3"); Task 7 updates
the spec text. Read `docs/HANDOFF.md` ("Plan 2 architecture", "Known minor issues") and
`docs/protocol.md` before starting.

**Out of scope (Plan 4):**
- notifications from the phone
- drag-and-drop
- audio
- gamepads
- encoder pause
- the latency harness
- choosing between two phones that are plugged in at the same time (the first one that came
  online wins; see Task 1)
- a settings window

## Decisions made in this plan (Sahaj may override)

1. **Friendly name.** The name is `settings get global device_name` ("Xiaomi 11T Pro"). If that is
   empty, it falls back to `ro.product.marketname`, then `<ro.product.manufacturer>
   <ro.product.model>`. HANDOFF suggested `ro.product.model`, but on the Xiaomi that reads
   `2107113SG`.
2. **Disconnect pauses that phone.** It stays disconnected until it is unplugged and plugged in
   again, or until Connect is clicked. Undo phone setup leaves it paused the same way, so Lantern
   doesn't reinstall itself straight away.
3. **The keyboard setting applies at the next connection.** The menu item says so. Switching the
   keyboard mid-session would need a second journaled IME change while windows are open.
   Disconnect then Connect applies it at once.
4. **Start at login is systemd's enable state** (`systemctl --user is-enabled lanternd.service`), not
   a TOML key, so there is only one source of truth.
5. **Quit** stops lanternd with exit 0, and the unit's `Restart=on-failure` does not restart it.
   `build.py install` adds a **Lantern** app-menu entry that runs `systemctl --user start
   lanternd.service` to bring it back.
6. **Undo phone setup from the tray asks first** (a `kdialog` warning with Continue/Cancel) and
   then runs the same steps as `lantern undo-setup`, inside lanternd. The CLI still refuses while
   lanternd runs. Its message now also points to the tray.
7. **Writing `settings.toml` regenerates its header comment and drops other comments.** Unknown
   keys are kept. A file that doesn't parse is never overwritten: the tray reports the error
   instead.
8. **Opening an app that the connected phone doesn't have** gives "Connect your Xiaomi 11T Pro" (or
   the right name). The daemon refuses only after the session's first full catalogue arrived,
   so an app installed since the last sync is never refused by a stale list.

## Global Constraints

- Phones: Xiaomi 11T Pro `20e9318` (Android 17 / SDK 37), Motorola edge 50 pro `ZD222LMDFV`
  (Android 16, work profile user 10). No root, USB debugging only.
- **Protocol stays v1.** Every addition is a new host-only message, a new host-only enum value,
  or an optional field that is omitted when it has its default. Existing bytes, including the
  golden vectors, stay identical. The helper jar ships inside `lantern`.
- **Every phone setting Lantern changes is journaled before the change and restored after**,
  only while it still holds Lantern's value. This plan adds no new phone setting. A session with
  the keyboard setting off changes only `stay_on_while_plugged_in`, like Plan 2's keyboard-less
  fallback.
- Wayland `app_id` and the `.desktop` file name are unchanged (`lantern.<pkg>[-u<N>]`). There is
  never a serial in an app_id.
- `.desktop` files Lantern owns are exactly those with `X-Lantern-Serial=`. A file without it is
  never modified or deleted.
- Never log clipboard or typed text, only lengths.
- Every adb call targets the selected serial (`-s`) and has a timeout.
- Any desktop command lanternd spawns (`kdialog`, `xdg-open`, `notify-send`) gets
  `WAYLAND_DISPLAY`, `DISPLAY` and `XDG_CURRENT_DESKTOP` from `systemctl --user
  show-environment` when lanternd's own environment lacks them, the same fallback as
  `clipboard/wayland.rs::socket_path`. Move that lookup into one shared function,
  `crate::desktop::session_env() -> Vec<(String, String)>`.
- The tray is optional. With no session bus or no tray host, lanternd runs as before and logs one
  warning.

## Review Focus

1. **Switching phones.**
   - A's entries survive B's full sync.
   - An app on both phones is one file listing both serials.
   - Unplugging changes no file.
   - A Plan 2 file with only `X-Lantern-Serial=` is read as listing that one serial.
   - Pinned by Task 1 `full_sync_keeps_other_phones_entries`, `shared_app_lists_both_phones`,
     `legacy_entry_reads_as_one_phone`.
2. **Opening an app right after switching phones.** Before B's first full catalogue completes, an
   open is never refused. After it, an open is refused with a notice naming A only when B lacks
   the app. Pinned by Task 2 `open_before_first_sync_is_forwarded` and
   `open_of_other_phones_app_is_refused_after_sync`.
3. **Disconnect or Undo while a session is live or about to restart.**
   - The phone ends restored.
   - No restart races the undo: the pause is set before the session closes, and an `Online`
     event for a paused serial starts nothing.
   - Undo waits for the previous helpers.
   - Pinned by Task 5 `disconnect_pauses_until_replug`, `online_while_paused_starts_nothing`,
     `undo_runs_without_a_session`, and acceptance item 19.
4. **A hand-edited or broken `settings.toml`.**
   - lanternd never crashes.
   - A wrong type falls back to that key's default.
   - A file that doesn't parse gives the defaults (logged once per change) and is never
     overwritten by the tray.
   - Unknown keys survive a tray write.
   - Pinned by Task 3 `bad_value_type_uses_default`, `unparseable_file_is_not_overwritten`,
     `set_keeps_unknown_keys`.
5. **No tray host, plasmashell restarts, or no session bus.** The daemon is unaffected. The tray
   registers again when `org.kde.StatusNotifierWatcher` gets a new owner. Pinned by Task 6
   `reregisters_when_watcher_returns` and `no_bus_leaves_daemon_running`.

---

## File Structure

```
host/lantern/src/daemon/catalog.rs     MODIFY  multi-phone entries, phones(), synced, connect_notice
host/lantern/src/daemon/companion.rs   MODIFY  friendly name in the device record
host/lantern/src/daemon/control.rs     NEW     Command, Phone, View
host/lantern/src/daemon/{mod,ipc,session}.rs  MODIFY  Parts, pause/commands, open refusal, keyboard
host/lantern/src/settings.rs           NEW     Settings, SettingsHandle (file + watch + poll)
host/lantern/src/desktop.rs            NEW     session_env(), spawn helpers for desktop commands
host/lantern/src/tray/{mod,menu,sni,dbusmenu}.rs  NEW  the tray
host/lantern/src/{undo,main,lib}.rs, window/{mod,outcome}.rs, clipboard/wayland.rs  MODIFY
host/lantern/tests/{ipc,tray}.rs       MODIFY/NEW
host/proto/src/lib.rs, host/proto/tests/golden.rs, host/proto/golden/*.bin  MODIFY
android/helper/src/dev/lantern/helper/{Router,Main,ImeSetup}.java + tests  MODIFY
tools/build.py (Lantern launcher entry), tools/device_test.py (items 15-20)  MODIFY
docs/protocol.md, docs/superpowers/specs/2026-11-07-lantern-design.md (§9a), README.md,
docs/HANDOFF.md                                                              MODIFY
```

Test commands used below:
- Host: `cd host && export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace --offline`
- Helper: `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py helper`
- Install: `python3 tools/build.py install`. This restarts lanternd from this build: the phone
  panel goes off and stay-awake reads 7.

---

### Task 1: Menu entries for every phone

**Files:**
- Modify: `host/lantern/src/daemon/catalog.rs`, `host/lantern/src/daemon/mod.rs`
  (`select_serial`), `host/lantern/src/daemon/ipc.rs` (`session_started`),
  `host/lantern/src/undo.rs`
- Test: unit tests in `catalog.rs`, `daemon/mod.rs`, `undo.rs`; `tools/device_test.py` item 15

**Interfaces (Produces):**
- `desktop_file(e: &AppEntry, serials: &[String], exe: &Path, icon: Option<&Path>) -> String`.
  - `X-Lantern-Serial=<the serial whose sync wrote this file>` stays as the ownership marker.
  - The new line is `X-Lantern-Serials=` followed by each serial `escape_value`d, with `;` written
    as `\;`, each followed by `;`. Serials are sorted and deduplicated.
- `Owned` gains `serials: Vec<String>`. When a file has no `X-Lantern-Serials`, the list is
  `[X-Lantern-Serial]` (Plan 2 files).
- `Catalog::start_session(&mut self, serial: &str)` replaces `begin_sync` in `Hub::session_started`.
  It opens the sync window and sets `synced = false`.
- `Catalog::synced(&self) -> bool` is true once an `end_sync` whose count matched has run since
  `start_session`.
- `Catalog::phones(&self, pkg: &str, user: u32) -> Vec<String>` returns the serials of that app's
  entry, or empty when there is no Lantern entry.
- Semantics, with S the session serial:
  - `upsert(e)`: serials = existing ∪ {S}. Name and icon are written from `e` (the last syncer
    wins).
  - `remove(pkg, user)` (live): S comes off that entry, and the file and icon are deleted when the
    list is left empty.
  - `end_sync(count)`, only when count matches: for every owned entry that lists S, whose user
    appeared in this sync and whose (pkg, user) was not kept, S comes off. Entries of other
    serials are never touched. Returns how many entries S came off.
  - `remove_serial(serial)` (undo): that serial comes off every entry, emptied files go, and the
    Phone menu files go when no owned entry is left. Returns how many entries it came off.
- `Catalog::with_rebuild` no longer writes the menu files. The first `upsert` writes them (fixes
  the Known minor issue "undo-setup rewrites the menu files").
- `select_serial(configured: Option<&str>, online: &[String], current: Option<&str>) ->
  Option<String>`:
  - `configured` wins.
  - Otherwise `current` if it is still online.
  - Otherwise the first of `online`, which is kept in the order devices came online.
  - The personal fallback `20e9318` is removed (a Known minor issue).

- [ ] **Step 1: Write the failing tests** (`catalog.rs`, `mod.rs`)
  - `full_sync_keeps_other_phones_entries`: A syncs `com.a`, `com.x`. B's full sync
    `[com.b]` → `com.a`/`com.x` files unchanged (bytes and mtime), and `com.b` lists only B.
  - `shared_app_lists_both_phones`: A then B both list `com.s` →
    `X-Lantern-Serials=A;B;`, and `phones("com.s",0) == ["A","B"]`.
  - `full_sync_drops_only_that_phone`: shared `com.s`, then B's full sync without `com.s` → the
    file stays and lists `A;`. A's sync without it → the file and icon are gone.
  - `live_remove_drops_only_that_phone` (same shape, through `remove`).
  - `legacy_entry_reads_as_one_phone`: a hand-written Plan 2 file with only
    `X-Lantern-Serial=A` → `phones == ["A"]`. B's upsert gives `A;B;`.
  - `serial_with_semicolon_round_trips`: serial `a;b` → `phones == ["a;b"]`.
  - `remove_serial_drops_one_phone` replaces `remove_serial_removes_only_that_serial`. The
    menu files stay while any entry is left.
  - `construction_writes_no_menu_files`: after `with_rebuild` alone, neither file exists.
    `menu_and_directory_files` now does one upsert first.
  - `synced_after_matching_end_sync_only`: false after `start_session`, false after a
    mismatched `end_sync`, true after a matching one, false again after `start_session`.
  - Delete `full_sync_prunes_other_serial`. Keep `full_sync_keeps_absent_profile` and
    `partial_sync_prunes_nothing`, run with two serials.
  - `select_serial_prefers_current_then_first_online`: `(None, ["b","a"], Some("a")) → a`;
    `(None, ["b","a"], None) → b`; `(None, [], None) → None`; `(Some("x"), ["a"], None) → x`.
  - `undo.rs`: `catalogue_entries_of_the_serial_are_removed` now expects the shared entry to stay
    with the other serial.
- [ ] **Step 2:** Run the host tests. Expected: FAIL to compile (`start_session`, `phones`, new
  `select_serial` signature).
- [ ] **Step 3:** Implement. In `drive`, `connected` becomes a `Vec<String>` in online order. The
  live session's serial is `current`.
- [ ] **Step 4:** Run the host tests. Expected: PASS.
- [ ] **Step 5: Device item 15 `phones_keep_their_entries`** in `tools/device_test.py`:
  1. Plant `lantern.dev.lantern.test.other.desktop`, written with `desktop_file`'s format
     (`X-Lantern-Serial=lantern-test-other`, `X-Lantern-Serials=lantern-test-other;`,
     `X-Lantern-Package=dev.lantern.test.other`, `X-Lantern-User=0`).
  2. `systemctl restart` lanternd, wait for ready + companion ready, then wait 10 s for the full
     sync.
  3. Check the planted file is byte-identical, and that
     `lantern.com.android.settings.desktop` lists `20e9318` in `X-Lantern-Serials`.
  4. In a `finally`, delete the planted file.

  Run `python3 tools/build.py install && python3 tools/device_test.py --only 15 10`. Expected:
  both PASS.
- [ ] **Step 6:** Commit with `feat(catalog): keep every phone's menu entries (X-Lantern-Serials); first-online phone wins`.

### Task 2: Phone names and "Connect your …"

**Files:**
- Modify: `host/lantern/src/daemon/companion.rs`, `catalog.rs` (`connect_notice`), `ipc.rs`
  (open refusal), `window/outcome.rs`, `window/mod.rs` (`notify_connect_phone`)
- Test: unit tests there, `host/lantern/tests/ipc.rs`, device item 16

**Interfaces (Produces):**
- `companion::clean_name(raw: &str) -> Option<String>`:
  - Trims the value; `""` and `"null"` give `None`.
  - Drops control characters and keeps at most 64 chars.
- `companion::read_phone_name(adb: &Adb) -> Option<String>` tries, in order:
  1. `settings get global device_name`
  2. `getprop ro.product.marketname`
  3. `<getprop ro.product.manufacturer> <getprop ro.product.model>`

  Each read goes through `clean_name` and uses `ADB_CALL_TIMEOUT`.
- `companion::record_name(state_dir: &Path, serial: &str, name: &str) -> Result<bool>`:
  - Rewrites an **existing** record atomically (temp + rename), setting `"name"` and keeping
    every other key. Returns whether it changed.
  - With no record it does nothing and returns `Ok(false)`. The record's existence keeps
    meaning "Lantern set up this phone", so the first-setup notice still fires.
- `companion::device_name(state_dir: &Path, serial: &str) -> Option<String>`.
- `Setup::prepare` calls `read_phone_name` + `record_name` right after `record_setup` succeeds.
  A failure is only logged.
- `pub type Names = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>`. `companion::names()`
  builds one over `state_dir()`.
- `catalog::connect_notice(dirs: &Dirs, names: &dyn Fn(&str) -> Option<String>, pkg: &str, user:
  u32, here: Option<&str>) -> (String, String, Option<PathBuf>)`, with `App` = the entry's Name
  (else pkg) and `n(s)` = `names(s)`, else `s`:

  | phones listed | summary | body |
  |---|---|---|
  | none (no entry) | `Connect your phone` | `{App} opens when your phone is connected by USB.` (unchanged) |
  | `[A]`, `here` none | `Connect your {n(A)}` | `{App} opens when your {n(A)} is connected by USB.` |
  | `[A]`, `here` = H | `Connect your {n(A)}` | `{App} is on your {n(A)}, not on {n(H)}.` |
  | `[A, B]` | `Connect {n(A)} or {n(B)}` | `{App} opens when one of them is connected by USB.` |
  | 3 or more | `Connect one of your phones` | `{App} is on {n(A)}, {n(B)} and N more.` |

- `ipc::NOT_ON_THIS_PHONE: &str = "app not on this phone"`. In `Hub::on_open`, after the
  `helper` check: if `catalog.synced()` and `phones(pkg,user)` is non-empty and does not contain
  the session serial, reply `error(NOT_ON_THIS_PHONE)` on stream 0 and close. No window id is
  used.
- `outcome::Notice { NoPhone, NotOnThisPhone }` and `State::notice(&self, stream: u32, msg: &str) ->
  Option<Notice>` replace `connect_notice`. Both messages qualify only on stream 0 before
  `opened`.
- `window::notify_connect_phone(id, notice)`: `here` is `ipc::query_status(...).serial` for
  `NotOnThisPhone` (`None` on any error). The window logs `connect notice: <summary>` at info
  level (device item 16 reads it).

- [ ] **Step 1: Write the failing tests**
  - `clean_name_trims_and_rejects_null`: `" Xiaomi 11T Pro\n"` gives `"Xiaomi 11T Pro"`;
    `"null"` and `""` give `None`; 100 × `x` keeps 64; `"a\u{7}b"` gives `"ab"`.
  - `record_name_keeps_other_keys_and_needs_a_record`.
  - `connect_notice_*`: one test per table row, with exact strings.
  - `outcome::notice_only_for_refused_open` (replaces
    `connect_notice_only_for_a_refused_open_without_phone`).
  - `tests/ipc.rs` `open_before_first_sync_is_forwarded`: a catalogue whose `com.x` entry
    lists only `other`; the session `dev` is up and no `apps_end` has been sent. `open com.x`
    reaches the fake helper.
  - `tests/ipc.rs` `open_of_other_phones_app_is_refused_after_sync`: after `apps_end{count}`
    matched, `open com.x` gets `error{msg:"app not on this phone"}` and EOF, and the helper sees
    no `open`. `open` of an app that lists `dev` still goes through.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests. Expected: PASS.
- [ ] **Step 5: Device item 16 `not_on_this_phone_notice`:**
  1. Plant an entry for `dev.lantern.test.absent` that lists `lantern-test-other`.
  2. Write `devices/lantern-test-other.json` = `{"companion":true,"since":0,"name":"Test
     Phone"}`.
  3. Wait for the session and its first full sync (10 s after companion ready).
  4. Run `lantern open dev.lantern.test.absent`. It exits 1 within 5 s, and its stderr has
     `connect notice: Connect your Test Phone`.
  5. Check `devices/20e9318.json` has a non-empty `name`.
  6. In a `finally`, remove both planted files.
- [ ] **Step 6:** Commit with `feat: phone names in the device record; "Connect your <phone>" for another phone's app`.

### Task 3: Settings file, live clipboard toggle

**Files:**
- Create: `host/lantern/src/settings.rs`, `host/lantern/src/desktop.rs`
- Modify: `host/lantern/Cargo.toml` (`toml`, `zbus`; see Tech Stack), `src/lib.rs`,
  `daemon/mod.rs` (`Parts`), `daemon/ipc.rs`, `clipboard/wayland.rs` (use `desktop::session_env`)
- Test: unit tests in `settings.rs`; `tests/ipc.rs`; device item 17

**Interfaces (Produces):**
- `settings.rs`:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Settings { pub clipboard_sync: bool, pub lantern_keyboard: bool } // Default: true, true
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Key { ClipboardSync, LanternKeyboard }        // name(): "clipboard_sync", "lantern_keyboard"
  pub const POLL: Duration = Duration::from_secs(2);
  pub fn default_path() -> Option<PathBuf>;               // $XDG_CONFIG_HOME|~/.config + lantern/settings.toml
  pub fn parse(text: &str) -> Result<(Settings, Vec<String>), String>; // Err = TOML syntax error (line); Vec = per-key warnings
  pub fn load(path: &Path) -> (Settings, Option<String>);  // absent -> defaults; Err text when unparseable
  pub fn set(path: &Path, key: Key, value: bool) -> Result<Settings>;
  pub fn ensure_file(path: &Path) -> Result<()>;           // writes HEADER + defaults if absent
  #[derive(Clone)] pub struct SettingsHandle { /* path, watch::Sender<Settings> */ }
  impl SettingsHandle {
      pub fn new(path: Option<PathBuf>) -> Self;           // loads once; None = defaults, writes refused
      pub fn subscribe(&self) -> watch::Receiver<Settings>;
      pub fn get(&self) -> Settings;
      pub fn path(&self) -> Option<&Path>;
      pub fn set(&self, key: Key, value: bool) -> Result<()>; // set() + send_replace
      pub fn spawn_poll(&self) -> tokio::task::JoinHandle<()>; // reload when mtime/len change, every POLL
  }
  ```
- `set` works like this:
  - It reads the file. If it is absent, it starts from an empty table.
  - If the file doesn't parse, it bails with `settings.toml has an error: <msg>; fix it first`
    and writes nothing.
  - Otherwise it sets the key and keeps every other key.
  - It writes atomically (temp + rename, mode 0600, dir created 0700) as `HEADER` + the table.
- `HEADER` (exact):
  ```
  # Lantern settings. Lantern's tray menu changes this file; edits here apply within a few seconds.
  # clipboard_sync: copy text between the PC and the phone (true/false).
  # lantern_keyboard: type on the phone through the Lantern keyboard (true/false; applies at the next connection).
  ```
- Value handling: a wrong-typed value for a known key gives that key's default plus a warning
  `<key> must be true or false; using <default>`. Unknown keys are ignored when reading.
- `load` logs its error or warnings once per file change, never repeatedly while the file stays
  the same.
- `desktop::session_env() -> Vec<(String, String)>`: our own `WAYLAND_DISPLAY`, `DISPLAY`,
  `XDG_CURRENT_DESKTOP`, else the values from `systemctl --user show-environment` (5 s timeout).
- `desktop::spawn(program: &str, args: &[&str]) -> std::io::Result<tokio::process::Child>` runs a
  command with `session_env()`, stdin null, `kill_on_drop(false)`.
- `daemon::Parts`, `#[derive(Default)]`, with `catalog: Option<Catalog>`, `clipboard:
  Option<PcClipboard>`, `settings: Option<SettingsHandle>`. Task 5 adds fields.
  `serve_with(listener, parts: Parts, configured, events, starter, shutdown, policy)` replaces
  `serve_with_catalog`. Existing tests call it with `Parts { catalog, clipboard,
  ..Default::default() }`.
- `Hub::set_settings(watch::Receiver<Settings>)`:
  - While `clipboard_sync` is false, `on_pc_clip` sends nothing and `on_phone_clip` sets
    nothing. Each logs `clipboard sync off; <len> chars not synced` at debug level.
  - A change from false to true resets `ClipSync`.
- `daemon::run` creates `SettingsHandle::new(settings::default_path())` and calls `spawn_poll`.

- [ ] **Step 1: Write the failing tests**
  - `defaults_when_absent`
  - `parses_both_keys`
  - `bad_value_type_uses_default`: `clipboard_sync = "no"` → `true`, with one warning naming
    the key.
  - `unparseable_file_is_not_overwritten`: `clipboard_sync = ` → `load` gives the defaults and
    `Some(err)`; `set` errs and the bytes are unchanged.
  - `set_keeps_unknown_keys`: `future = 3` survives, and the file starts with `HEADER`.
  - `set_creates_dir_and_file_0600`
  - `poll_picks_up_hand_edit` (tokio, paused clock not used; real 2 s): edit the file, and the
    receiver sees the change within 3 s.
  - `tests/ipc.rs` `clipboard_off_syncs_nothing_either_way`: with the fake clipboard,
    settings off → a PC offer doesn't reach the helper and a phone `clip` doesn't reach the PC
    handle. Settings on → both flow again.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests. Expected: PASS. `cargo build --offline` must not touch the
  network. Commit the `Cargo.lock` changes (zbus/toml additions plus the `toml_edit`/`winnow`
  downgrade that resolution picks).
- [ ] **Step 5: Device item 17 `clipboard_setting_off`:**
  1. Save the bytes of `~/.config/lantern/settings.toml` (or note that it is absent) and Klipper's
     text.
  2. Set `clipboard_sync = false` by writing the file. Wait 3 s.
  3. Copy on the phone through the TestActivity path from item 13. Klipper must not get the text
     within 3 s, and the helper log shows no `sent to the PC` for it.
  4. Restore the file, then Klipper's text, in that order and each guarded.
- [ ] **Step 6:** Commit with `feat(settings): settings.toml with live reload; clipboard sync on/off`.

### Task 4: Keyboard setting (hello.keyboard)

**Files:**
- Modify: `host/proto/src/lib.rs`, `host/proto/tests/golden.rs`, new
  `host/proto/golden/control_hello_keyboard_off.bin`
- Modify: `host/lantern/src/daemon/session.rs`, `host/lantern/src/daemon/mod.rs` (the starter
  reads `settings.get().lantern_keyboard` at each start)
- Modify: `android/helper/src/dev/lantern/helper/{Router,Main,ImeSetup}.java`
- Test: `RouterTest`, `ImeSetupTest`, `ProtoTest`, proto unit tests, device item 18

**Interfaces (Produces):**
- `Control::Hello { version: u32, token: String, #[serde(default = "yes", skip_serializing_if =
  "is_true")] keyboard: bool }`. Every existing `Hello` byte sequence is unchanged.
  `{"t":"hello","version":1,"token":"t","keyboard":false}` is the new golden.
- `session::handshake(stream, token: &str, keyboard: bool)`, `session::start(adb, keyboard:
  bool)`, `start_with_companion(adb, setup, keyboard: bool)`.
- Helper `Router`:
  - Reads the optional `keyboard`. Absent means true; a non-boolean is a `ProtocolError`.
  - It is read before `sessionStarter` is called, and exposed as `boolean keyboardWanted()`.
- Helper `Main`:
  - `ImeSetup.start(linked[0] && routerRef[0].keyboardWanted(), …)`.
  - `ImeSetup.companionState(boolean linked, String imeFailure, boolean keyboardWanted)`: linked
    and keyboard not wanted gives `companion{state:"ready", msg:"keyboard off in Lantern
    settings"}`. All other cases are unchanged.
  - The `ime` relay drops frames while `lanternIme` is false. It logs one line per dropping
    stretch, `ime dropped: the Lantern keyboard is not selected`, never the text.
- `Hub` keeps the last `companion.msg` next to its state (`Hub::companion() ->
  Option<(CompanionState, Option<String>)>`), for Task 5's `View`.

- [ ] **Step 1: Write the failing tests**
  - Proto (`fn yes() -> bool { true }` and `fn is_true(b: &bool) -> bool` next to `is_false`):
    - `hello_without_keyboard_is_v1_bytes`
    - `hello_keyboard_false_json_shape`
    - `hello_missing_keyboard_decodes_true`
    - golden `control_hello_keyboard_off.bin`
  - `ProtoTest.goldenHelloKeyboardOff()`
  - `RouterTest`:
    - `helloKeyboardFalseReachesStarter`
    - `helloKeyboardAbsentIsTrue`
    - `helloKeyboardNotBooleanIsProtocolError`
  - `ImeSetupTest.companionStateKeyboardOff()`
  - `session.rs` unit `handshake_sends_keyboard_only_when_off` (over a local TCP pair).
- [ ] **Step 2:** Run the host tests and the helper build. Expected: FAIL.
- [ ] **Step 3:** Implement. Regenerate the goldens with `LANTERN_WRITE_GOLDEN=1 cargo test -p
  lantern-proto --test golden`, then check with `git diff --stat host/proto/golden` that only the
  new file appears.
- [ ] **Step 4:** Run the host tests and the helper build. Expected: PASS.
- [ ] **Step 5: Device item 18 `keyboard_setting_off`:**
  1. Record the originals: `default_input_method`, `enabled_input_methods`,
     `stay_on_while_plugged_in`.
  2. Save `settings.toml`, set `lantern_keyboard = false`, `systemctl restart` lanternd and wait
     for ready.
  3. During the session `default_input_method` and `enabled_input_methods` equal the originals,
     stay-awake is 7, `lantern status` companion is `ready`, and logcat shows `keyboard off in
     Lantern settings`.
  4. Restore the file, restart, and check the Lantern keyboard is selected during the session
     again.
  5. Finally, with lanternd stopped through item 7's path, check all three settings equal the
     originals. If they don't, restore them by hand and FAIL.
- [ ] **Step 6:** Commit with `feat: keyboard setting (hello.keyboard, v1-compatible); stay-awake-only session when off`.

### Task 5: Daemon control: Connect, Disconnect, Undo, Quit

**Files:**
- Create: `host/lantern/src/daemon/control.rs`
- Modify: `daemon/mod.rs` (`drive`, `Parts`), `daemon/ipc.rs`, `host/proto/src/lib.rs`
  (host-only additions), `undo.rs`, `main.rs`, `window/outcome.rs`, `window/mod.rs`
- Test: `tests/ipc.rs`, `tests/fake_adb.rs`, proto unit tests, device item 19

**Interfaces (Produces):**
- `control.rs`:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Command { Connect, Disconnect, UndoSetup, Quit }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum Phone {
      None,                                   // no target phone online
      Starting { serial: String },
      Ready { serial: String, companion: Option<(CompanionState, Option<String>)> },
      Retrying { serial: String },            // retry backoff, phone still online
      Paused { serial: String },              // Disconnect or after Undo, until replug/Connect
      Undoing { serial: String },
  }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct View { pub phone: Phone, pub name: Option<String> } // name = names(serial)
  pub type UndoRunner = Arc<dyn Fn(String) -> BoxFuture<'static, (bool, String)> + Send + Sync>;
  ```
- `Parts` gains `commands: Option<mpsc::Receiver<Command>>`, `view: Option<watch::Sender<View>>`,
  `names: Option<Names>` and `undo: Option<UndoRunner>`. `daemon::run` wires the real ones:
  `undo` runs `undo::run_steps` and collects its output lines.
- `drive` semantics. `paused: Option<String>` is set **before** any close:
  - **Disconnect:** applies when there is a target serial S (live, starting or retrying). It
    sets paused = S, cancels a start, closes the session (bye → the helper restores the phone)
    and clears `retry_at`. With nothing live it does nothing.
  - **Connect:** clears paused. If the target is online, it sets `failures = 0` and
    `retry_at = now`.
  - **`Offline(S)` while paused == S:** clears paused. **`Online(S)` while paused == S:** starts
    nothing.
  - **UndoSetup:** needs a target S that is online. It pauses S, closes or cancels, and the phone
    becomes `Undoing`. It then runs `undo(S)` as a polled future, like `starting`.
    - When it finishes: `Paused{S}`, plus a notification. On success the summary is `Lantern
      removed itself from {name}` and the body is the output lines. On failure the summary is
      `Undo phone setup failed on {name}` and the body is the output lines.
    - Commands other than Quit are ignored while undoing. Quit waits for the undo to finish.
  - **Quit:** the same path as the shutdown future, and `serve_with` returns `Ok(())`.
  - The `view` is sent on every state change, never repeated with an identical value.
- `undo::run_steps(adb: &Adb, state_dir: &Path, remove_entries: &dyn Fn(&str) ->
  Result<usize>, out: &mut dyn Write) -> Result<bool>` holds steps 1–4 of today's `run`.
  - `run` = `check_daemon` + `run_steps(…, &remove_catalogue_entries, …)`.
  - `STOP_FIRST` becomes `stop lanternd first (systemctl --user stop lanternd), or use "Undo
    phone setup…" in Lantern's tray menu`.
- Proto, all host-only:
  - `SessionState::Paused` (serialized `"paused"`). `status_report.serial` is the paused serial.
  - `Control::Daemon { cmd: DaemonCmd }` with `DaemonCmd { Connect, Disconnect }`
    (snake_case). It comes from a client as its first frame. The daemon queues the command and
    replies with the current `status_report`, then closes.
  - Any other first-frame use stays the protocol error it is today.
- `ipc::PAUSED: &str = "phone disconnected in Lantern"`: an `open` while paused gets this error.
  `outcome::Notice` gains `Paused`, and its notice reads summary `Lantern is disconnected`, body
  `Click Connect in Lantern's tray menu to open {App}.`
- CLI: `lantern connect`, `lantern disconnect`. Each prints the report it got and exits 0
  (1 if lanternd is unreachable). `USAGE` gains both.

- [ ] **Step 1: Write the failing tests** (`tests/ipc.rs`, using a `start_daemon_parts` helper
  with a command channel, a view receiver and a fake `UndoRunner`)
  - `disconnect_pauses_until_replug`: ready → Disconnect → the fake helper sees `bye`, the view
    is `Paused{dev}` and `lantern status` gives `paused`. No new start within 3 × the fast retry
    policy. `Offline(dev)` then `Online(dev)` → ready again.
  - `online_while_paused_starts_nothing`
  - `connect_resumes_after_disconnect`
  - `quit_command_ends_daemon_ok`: `serve_with` returns `Ok` and the socket file is gone.
  - `undo_runs_without_a_session`: the fake runner records that it was called with `dev` while
    no session is live (the helper's connection was already closed). The view goes `Undoing` →
    `Paused`. A Connect sent during the undo is ignored.
  - `open_while_paused_gets_paused_error`
  - `daemon_cmd_over_socket`: `{"t":"daemon","cmd":"disconnect"}` gets a `status_report`, then
    the session ends.
  - Proto: `session_state_paused_json`, `daemon_cmd_json_shape`.
  - `tests/fake_adb.rs`: `undo_run_steps_keeps_record_when_forget_fails`. Use the existing fake
    with `forget_fail` present. The output has `helper --forget: FAILED`, then `companion:`,
    `catalogue:` (the `remove_entries` closure was called with the serial) and `device record:
    kept`; the result is `Ok(false)`.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests. Expected: PASS.
- [ ] **Step 5: Device item 19 `disconnect_and_connect`:**
  1. Record the originals (`stay_on`, keyboard, enabled list).
  2. `lantern disconnect`. Within 5 s the phone is restored (originals, panel on, no helper
     pids), and `lantern status` stays `paused` for 15 s.
  3. `lantern connect`. Ready again within 15 s.
  4. Leave lanternd ready.
- [ ] **Step 6:** Commit with `feat(daemon): Connect/Disconnect/Undo/Quit commands, paused state, lantern connect|disconnect`.

### Task 6: The tray

**Files:**
- Create: `host/lantern/src/tray/{mod.rs, menu.rs, sni.rs, dbusmenu.rs}`,
  `host/lantern/tests/tray.rs`
- Modify: `daemon/mod.rs` (`run` spawns the tray), `src/lib.rs`, `tools/build.py`
  (`install`/`uninstall` add or remove the Lantern launcher)
- Test: `menu.rs` unit tests, `tests/tray.rs`, device item 20

**Interfaces (Produces):**
- `menu.rs` (pure):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Action { Connect, Disconnect, ToggleClipboard, ToggleKeyboard, ToggleLogin,
                    OpenSettings, UndoSetup, Quit }
  #[derive(Debug, Clone, PartialEq)]
  pub enum Item { Label { id: i32, text: String }, Separator { id: i32 },
                  Button { id: i32, text: String, enabled: bool, action: Action },
                  Check { id: i32, text: String, on: bool, action: Action } }
  pub fn items(view: &View, settings: &Settings, login: Option<bool>) -> Vec<Item>;
  pub fn action(items: &[Item], id: i32) -> Option<Action>;
  pub fn status_line(view: &View) -> String;
  pub fn overlay(view: &View) -> &'static str;      // icon name or ""
  pub fn sni_status(view: &View) -> &'static str;   // "Passive" for Phone::None, else "Active"
  ```
  `name` below is `view.name`, or the serial when the name is unknown. Ids are fixed per row
  (below), so clicks stay valid across layout updates.

  | id | row | text (exact) | shown when |
  |---|---|---|---|
  | 1 | Label | `status_line`: `No phone connected` · `Connecting to {name}…` · `Connected to {name}` · `Reconnecting to {name}…` · `Disconnected from {name}` · `Removing Lantern from {name}…` | always |
  | 2 | Label | `Companion: {msg}` (unavailable), `Typing through the Lantern keyboard is off` (msg `keyboard off in Lantern settings`) | Ready with a msg |
  | 3 | Button | `Disconnect` (Starting/Ready/Retrying) or `Connect` (Paused) | not None/Undoing |
  | 4 | Separator | | always |
  | 5 | Check | `Sync clipboard` | always |
  | 6 | Check | `Type with the Lantern keyboard (from the next connection)` | always |
  | 7 | Check | `Start Lantern at login` | `login` is `Some` |
  | 8 | Button | `Open settings folder` | always |
  | 9 | Separator | | always |
  | 10 | Button | `Undo phone setup…`, enabled only when a serial is known and the phone is not `Undoing` | always |
  | 11 | Button | `Quit Lantern` | always |

  `overlay`:

  | Phone state | overlay |
  |---|---|
  | None, or Ready with no msg | `""` |
  | Ready with a msg | `emblem-warning` |
  | Starting, Retrying, Undoing | `view-refresh` |
  | Paused | `media-playback-pause` |
- `dbusmenu.rs`: a zbus `#[interface(name = "com.canonical.dbusmenu")]` served at `/MenuBar`:
  - Properties: `Version` u32 = 3, `TextDirection` = `"ltr"`, `Status` = `"normal"`,
    `IconThemePath` = `[]`.
  - `GetLayout(parent_id: i32, recursion_depth: i32, property_names: Vec<String>) -> (u32,
    (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>))`. Root id 0 has
    `children-display = "submenu"`. Each child is a `(i32, a{sv}, av)` struct wrapped in a
    variant.
  - Item properties: `label`; `enabled` (false for Label rows); `visible`; `type =
    "separator"`; `toggle-type = "checkmark"` and `toggle-state` (0/1) for Check rows.
  - `GetGroupProperties(ids: Vec<i32>, names: Vec<String>) -> Vec<(i32, HashMap<…>)>`,
    `GetProperty(id, name) -> OwnedValue`.
  - `Event(id, event_id: String, data: OwnedValue, timestamp: u32)`: only `"clicked"` acts.
  - `EventGroup(Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32>` returns unknown ids.
  - `AboutToShow(id) -> bool` returns false. `AboutToShowGroup(ids) -> (Vec<i32>, Vec<i32>)`
    returns empty lists.
  - Signals `LayoutUpdated(revision: u32, parent: i32)` and `ItemsPropertiesUpdated`. The tray
    emits `LayoutUpdated(rev, 0)` with `rev += 1` on every view, settings or login change.
- `sni.rs`: `#[interface(name = "org.kde.StatusNotifierItem")]` at `/StatusNotifierItem`:
  - Properties:
    - `Category` = `"Hardware"`, `Id` = `"lantern"`, `Title` = `"Lantern"`
    - `Status` = `sni_status`, `IconName` = `"smartphone"`, `OverlayIconName` = `overlay`
    - `AttentionIconName` = `""`, `IconPixmap` / `OverlayIconPixmap` /
      `AttentionIconPixmap` = `[]`
    - `ToolTip` = `("smartphone", [], "Lantern", status_line)`
    - `ItemIsMenu` = `true`, `Menu` = `/MenuBar`, `WindowId` = `0`
  - Methods `Activate(x, y)`, `SecondaryActivate`, `ContextMenu` and `Scroll` are no-ops.
  - Signals `NewStatus(s)`, `NewIcon`, `NewOverlayIcon` and `NewToolTip` are emitted when their
    value changes.
- `mod.rs`: `pub async fn run(conn: zbus::Connection, view: watch::Receiver<View>, settings:
  SettingsHandle, commands: mpsc::Sender<Command>) -> Result<()>`. It works in this order:
  1. Serves both objects and requests the name `org.kde.StatusNotifierItem-<pid>-1`.
  2. Calls `org.kde.StatusNotifierWatcher.RegisterStatusNotifierItem(<that name>)` on
     `/StatusNotifierWatcher`.
  3. Watches `NameOwnerChanged` for `org.kde.StatusNotifierWatcher` and registers again on
     every new non-empty owner.

  A missing watcher at start is logged once at info level (it registers when one appears).
- `tray::spawn(view, settings, commands) -> Option<JoinHandle<()>>` connects to the session bus.
  On failure it logs one warning `no tray: <error>` and returns `None`.
- Actions:
  - **Connect/Disconnect/Quit** send the `Command`.
  - **ToggleClipboard/ToggleKeyboard** call `settings.set(key, !current)`. An error gives a
    notification with summary `Lantern couldn't save its settings` and the error as the body.
  - **ToggleLogin** runs `systemctl --user enable|disable lanternd.service` (5 s timeout), then
    re-reads `is-enabled`. `login` is `Some(stdout == "enabled")`, or `None` if systemctl
    fails.
  - **OpenSettings** runs `settings::ensure_file`, then `desktop::spawn("xdg-open",
    [<dir>])`.
  - **UndoSetup** runs `desktop::spawn("kdialog", ["--title", "Lantern", "--continue-label",
    "Remove", "--warningcontinuecancel", "Remove Lantern from {name}? This puts back the phone's
    keyboard and settings, uninstalls the Lantern companion app and removes {name}'s apps from
    the Phone menu. Plug the phone in again to set it up again."])`. Exit code 0 sends
    `Command::UndoSetup`. If kdialog is missing, the tray sends a notification instead and
    does nothing.
- `daemon::run` creates `(cmd_tx, cmd_rx)`, a `watch::channel(View{phone: Phone::None, name:
  None})`, and calls `tray::spawn`. The tray must never block `drive`: it only reads the watch
  and sends on the channel (capacity 8, `try_send`; a full channel logs and drops the click).
- `tools/build.py install` writes `~/.local/share/applications/dev.lantern.Lantern.desktop`:
  - `Name=Lantern`, `Comment=Phone apps as PC windows`
  - `Exec=systemctl --user start lanternd.service`
  - `Icon=smartphone`, `Categories=Utility;`, `Type=Application`
  - no `X-Lantern-Serial`

  `uninstall` removes it.

- [ ] **Step 1: Write the failing tests**
  - `menu.rs`, one test per `Phone` variant: exact rows (ids, texts, enabled, shown) for
    `items`, plus `status_line`, `overlay` and `sni_status`. Also
    `action_maps_ids_across_states`: id 3 is Disconnect when Ready and Connect when Paused.
  - `tests/tray.rs`. Start a private bus with `dbus-daemon --session --nofork --print-address=1
    --address=unix:dir=<scratch>` and kill it on drop. Register a fake
    `org.kde.StatusNotifierWatcher` on that bus that records registrations.
    - `registers_and_serves_layout`: the watcher got `org.kde.StatusNotifierItem-<pid>-1`, and
      `GetLayout(0,-1,[])` gives the Ready rows with the exact labels. The SNI `Status` property
      is `Active`.
    - `click_sends_command`: `Event(3,"clicked",…)` on Ready → `Command::Disconnect` is
      received.
    - `toggle_writes_settings`: `Event(5,"clicked")` writes `clipboard_sync = false` to the
      scratch `settings.toml`. A second click sets it true.
    - `view_change_emits_layout_updated`: the revision increases and the label changes to
      `Disconnected from Test Phone`.
    - `reregisters_when_watcher_returns`: drop the fake watcher's name and request it again.
      A second registration is recorded within 2 s.
    - `no_bus_leaves_daemon_running`: `tray::spawn` with `DBUS_SESSION_BUS_ADDRESS` pointing
      at a dead socket returns `None` without panicking. Run this test in a child process
      (`std::process::Command` on the test binary with `--exact` and an env flag), because env
      vars are process-wide.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests. Expected: PASS.
- [ ] **Step 5: Device item 20 `tray_menu`** (real session bus, desktop unlocked):
  1. Find lantern's item in the watcher's `RegisteredStatusNotifierItems`.
  2. `busctl --user --json=short call <name> /MenuBar com.canonical.dbusmenu GetLayout iias 0 -1
     0` shows `Connected to <device_name>` and `Quit Lantern`.
  3. `Event 3 clicked` gives `paused` within 5 s, and the phone is restored.
  4. `Event 3 clicked` again gives ready within 15 s.
  5. Take a screenshot of the system tray area (`spectacle -b -n -o <scratch>.png`) and attach
     it to the task report for Sahaj. Opening the menu by hand is a deferred check, not
     automated.
- [ ] **Step 6:** Commit with `feat(tray): StatusNotifierItem + dbusmenu over zbus; Lantern launcher entry`.

### Task 7: Acceptance and docs

**Files:**
- Modify: `docs/protocol.md`, `docs/superpowers/specs/2026-11-07-lantern-design.md` (§9a),
  `README.md`, `docs/HANDOFF.md`, `tools/device_test.py` (docstring, `ITEMS`)

- [ ] **Step 1: Docs.**
  - `protocol.md`:
    - `hello.keyboard` in the table and the session rules.
    - `companion` msg `keyboard off in Lantern settings`.
    - `status_report` state `paused`.
    - `daemon{cmd}` under "Status".
    - The window-IPC errors `app not on this phone` and `phone disconnected in Lantern`.
    - The catalogue rule for multiple phones (`X-Lantern-Serials`).
  - Spec §9a: replace "One phone at a time … removed" with the union rule and the date of
    Sahaj's decision. Expand the tray bullet with the settings items.
  - `README.md`: the tray, `settings.toml` keys, `lantern connect|disconnect`, and the Lantern
    launcher.
  - `HANDOFF.md`: Plan 3 status, Plan 3 decisions, Known minor issues added from the reviews,
    and Next = the Plan 4 list.
- [ ] **Step 2: Acceptance run** on the real desktop, unlocked:
  `python3 tools/build.py install && python3 tools/device_test.py`. Expected: 18/18 automatic
  PASS (items 1–7, 9–13 and 15–20; 8 and 14 stay `--manual`). Record the summary line and any timings in HANDOFF.
- [ ] **Step 3:** Run the Motorola check if it is attached (HANDOFF deferred check 3). Add: its
  name in the device record, and the Xiaomi's apps still in the menu after its full sync (Review
  Focus 1 with two real phones). Otherwise list it as deferred.
- [ ] **Step 4:** Commit with `docs: Plan 3 acceptance, protocol and spec updates`.
