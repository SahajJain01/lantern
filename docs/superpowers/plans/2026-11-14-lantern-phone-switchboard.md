# Lantern Phone Switchboard (Plan 4b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Sahaj's standing instruction for execution:** device steps are DEFERRED to one acceptance run
> at the end (Task 7). Implementers still write and run every host test and the helper build,
> and they write each task's `device_test.py` items. They run no `build.py install` and no
> `device_test.py`, and they report each device step as DEFERRED, never as done.

**Goal:** The KDE menu gets one folder per phone, and each entry opens on its own phone. The tray
lists the plugged-in phones as connect/disconnect checkmarks. A new `autoconnect` setting
(default off) decides whether a plugged-in phone connects by itself or Lantern asks with a
notification. A phone switched off stays off across unplugging and lanternd restarts. Still
only one phone is live at a time.

**Architecture:**
- **Per-phone records.** `~/.local/state/lantern/devices/<serial>.json` gains `off`, `live`,
  `connected_at` and `serial`. Undo keeps the record and sets `off`.
- **Naming** (`naming.rs`). One *primary* phone keeps today's names (`lantern.<pkg>[-u<N>]`, the
  `windows.json` keys `<pkg>[:<N>]`), so its pinned launchers keep working. Every other phone gets
  a `-p<tag>` suffix and `@<serial>` keys. The primary is recorded in
  `~/.local/state/lantern/menu.json`.
- **Catalogue.** One `.desktop` file per (phone, app, Android user), with
  `Categories=X-Lantern-<tag>;`. The merged menu nests one submenu per phone under "Phone". A
  one-time migration splits today's shared `X-Lantern-Serials` files.
- **Daemon.** The run loop's `connected`/`paused` lists become a phone table: plugged phones in
  online order, the one *wanted* phone (4b: at most one), the per-phone `off`/`live` flags, a 10 s
  unplug grace per wanted phone, and pending connect-and-open requests (120 s). It publishes a
  per-phone `View` and `status_report.phones`.
- **Window.** `lantern open --phone <serial>`. Without `--phone`, the window picks its phone from
  `lantern status` and the menu entries. It sends the optional `open.phone` (window→lanternd only).
- **Prompts.** A connect prompt on its own bus connection shows "<phone> is plugged in" and
  "Connect your <phone>" notifications with a **Connect** action. It is built on the action
  notification code extracted from `unlock.rs`.
- **Tray.** Check rows per plugged phone, per-phone status and Unlock rows, an Autoconnect
  checkbox, and a "Remove Lantern from a phone…" submenu.

**Tech Stack:** Rust (zbus 5, toml 1, tokio, serde_json: all already dependencies; no new crate),
Python for `tools/device_test.py`. No helper or companion change.

**Spec:** `docs/superpowers/specs/2026-11-14-lantern-multi-phone-design.md` (approved 2026-11-14).
This plan implements goals G1–G5 and every "Launcher", "Tray" and "Connect rules" decision, with
one phone live at a time. G6–G8 (several live sessions) are Plan 4c. The data shapes below are
per phone, so 4c only adds sessions: records, desktop files, app_id/`windows.json` keys, tray
rows, `open.phone`, `View.phones` and `status_report.phones`. Read `docs/HANDOFF.md` ("Plan 3
decisions", "Known minor issues") and `docs/protocol.md` first.

**Out of scope:** several live sessions, per-phone clipboard fan-out, per-phone unlock (4c);
Wi-Fi adb; per-phone autoconnect; a settings window.

## Decisions made in this plan (the design left them to the builder; Sahaj may override)

1. **Primary phone keeps the old names.** The migration picks as primary the phone with the most
   legacy entries pinned in Plasma's task manager. It reads `launchers=` lines in
   `$XDG_CONFIG_HOME/plasma-org.kde.plasma.desktop-appletsrc`, read-only. Ties go to the phone
   with the most legacy entries, then to the smallest serial. On this PC the only pinned Lantern
   launcher is `lantern.com.microsoft.rdc.androidx-u10.desktop`, so the Motorola becomes primary.
   The Xiaomi's apps get new names and need re-pinning once. On a fresh install, the first phone
   that starts a session becomes primary.
2. **Names.** `tag(serial)` keeps `[A-Za-z0-9_]` and turns any other character into `_`, at most
   64 chars. The app_id is `lantern.<pkg>[-u<N>]` for the primary and `lantern.<pkg>[-u<N>]-p<tag>`
   otherwise. `-` never occurs in a package name, so no app_id collides. The `windows.json` key is
   `<pkg>[:<N>]` for the primary, else `<pkg>[:<N>]@<serial>`. The category is `X-Lantern-<tag>`
   and the folder file is `lantern-phone-<tag>.directory`. Two serials with the same tag: the
   second phone's entries are refused with a warning, and the first phone's files stay untouched.
3. **`live` is cleared by** a real unplug (absent longer than the grace), unchecking, Undo, being
   switched off for another phone, the tray's **Quit**, and lanternd starting while that phone
   stays absent for the whole grace. SIGTERM/SIGINT (`systemctl stop/restart`, `build.py install`)
   keep it. The last rule stops a stale flag from connecting a phone plugged in days later.
4. **With autoconnect on and another phone live,** a newly plugged phone is not connected (4b)
   but gets the "is plugged in" notification, so one click switches to it.
5. **`LANTERN_SERIAL`**: lanternd ignores every other serial. Other phones are not listed, never
   prompted and never started. The bypass applies when the phone comes online; an explicit
   Disconnect still switches it off.
6. **Undo** keeps the record. It always sets `off` and clears `live`, and sets `companion:false`
   only when the phone steps succeeded, so setting the phone up again shows "Lantern set up your
   phone" again. A record without `companion:true` counts as "not set up".
7. **No settings part = autoconnect on.** `serve`/`run_loop`/`serve_with` without
   `Parts.settings` (tests and embedders) treat autoconnect as on. lanternd always passes a
   settings handle, whose default is off. This keeps the existing ipc tests' "Online starts a
   session" harness.
8. **`lantern status`** stays additive. The top-level fields keep their meaning for the live
   phone (`paused` = no live phone, and the phone switched off most recently in this run is still
   plugged). New optional `phones` lists every known or plugged phone.
9. **Refused opens are lanternd's notification** when its connect prompt runs: it answers
   `error{"phone not connected; Lantern offered to connect it"}` and the window shows nothing of
   its own. Without a prompt (no session bus), the old errors and the window's own notices stay.
10. **The two-minute request** opens the app with `lantern open --phone S [--user N] -- pkg` once,
    when S's session becomes ready.
11. **The tray's one-at-a-time note** (row 15, "Lantern connects one phone at a time") shows
    while two or more phones are plugged in. The design's "the tray says so" is this row.

## Global Constraints

- Phones: Xiaomi 11T Pro `20e9318` (swipe), Motorola edge 50 pro `ZD222LMDFV` (PIN, work profile
  user 10). No root.
- **Protocol stays v1 between helper and lanternd.**
  - The window→lanternd `open` gains an optional `phone: string`, omitted when absent. lanternd
    strips it before forwarding, as it does `activation_token`.
  - Host-only `daemon` gains an optional `serial`. `status_report` gains an optional `phones`
    (omitted when empty).
  - `git diff --stat host/proto/golden` stays empty for the whole plan.
- **Every adb call names its serial and has a timeout.** This plan adds no adb call outside
  `Adb`.
- **Only `.desktop` files with `X-Lantern-Serial=` are Lantern's.** Neither the migration nor the
  catalogue ever modifies or deletes another file. The directory files Lantern writes are
  `lantern-phone.directory` and `lantern-phone-<tag>.directory`, and the per-phone ones carry
  `X-Lantern-Serial=`.
- Window app_id = the `.desktop` file stem = `StartupWMClass`.
- **One phone live at a time.** Checking B while A is live closes A without setting A's `off`.
- Values: `autoconnect` default `false`. Unplug grace 10 s; the test-only override
  `LANTERN_UNPLUG_GRACE_MS` is read once by `daemon::run`. A pending connect-and-open lasts
  120 s, and a later request for the same phone replaces it.
- Tests that spawn `lantern daemon` set `DBUS_SESSION_BUS_ADDRESS` to a dead socket. Bus tests run
  on a private `dbus-daemon` with a `<servicedir>`-less config (as `tests/tray.rs` `Bus::start`
  does). Every `Notify`/`CloseNotification` call has a 5 s timeout.
- Never log clipboard text, typed text or a PIN. Plan 4a's PIN rules are unchanged.
- Phone names go into notification bodies escaped for Plasma markup (`&` `<` `>`).

## Review Focus

1. **The migration touches a file that isn't Lantern's, or loses an entry when it stops halfway.**
   Expected: foreign files are byte- and mtime-identical, and a rerun after a crash ends in the
   same files with no duplicates. Pinned by Task 2 `migrate_never_touches_foreign_files` and
   `migrate_is_idempotent_after_a_partial_run`.
2. **A phone connects without his say-so with autoconnect off.** Example: lanternd starts with a
   stale `live` flag for a phone that is plugged in only later. Expected: the flag is cleared
   after the grace and the later plug-in prompts. Pinned by Task 3
   `stale_live_flag_cleared_when_absent_at_start` and `autoconnect_off_waits_and_prompts`.
3. **A tray click toggles the wrong phone** after phones plugged or unplugged and the rows moved.
   Expected: a row id stays bound to its serial for the tray's lifetime. Pinned by Task 4
   `ids_stay_with_their_phone`.
4. **Notification storms from a flapping cable.** Expected: offline/online within the grace never
   prompts, and one real plug-in gives one notification, which replaces any earlier one for that
   phone. Pinned by Task 3 `blip_never_prompts` and Task 6 `plugged_in_replaces_previous_notice`.
5. **A pending open fires twice or on the wrong phone.** Cases: a blip after ready, or another
   phone connecting first. Expected: once, only on the requested phone. Pinned by Task 3
   `pending_connect_opens_once` and `pending_is_per_phone`.

---

## File Structure

```
host/proto/src/lib.rs, host/proto/tests/golden.rs        MODIFY  open.phone, daemon.serial, PhoneReport
host/lantern/src/naming.rs                               NEW     tag, Naming (primary), menu.json
host/lantern/src/daemon/companion.rs                     MODIFY  Record flags, known_serials
host/lantern/src/daemon/catalog.rs                       MODIFY  per-phone files/folders, migrate
host/lantern/src/daemon/phones.rs                        NEW     connect rules (pure), Timing
host/lantern/src/daemon/{mod,control,ipc}.rs             MODIFY  phone table, View, commands, routing
host/lantern/src/notices.rs                              NEW     notifications with actions (from unlock.rs)
host/lantern/src/connect.rs                              NEW     plugged-in / Connect-your-phone prompt
host/lantern/src/window/{identity,mod,outcome}.rs        MODIFY  --phone, naming, Notice::Offered
host/lantern/src/window/resolve.rs                       NEW     which phone `lantern open` uses
host/lantern/src/tray/{menu,mod,dbusmenu,sni}.rs         MODIFY  switchboard rows, submenu
host/lantern/src/{settings,unlock,undo,main,lib}.rs      MODIFY
host/lantern/tests/{ipc,tray,unlock,cli,connect}.rs      MODIFY / NEW connect.rs
tools/device_test.py (preflight, readers, items 2,10,11,15,16,19,20,22-26)  MODIFY
docs/protocol.md, README.md, docs/HANDOFF.md                                 MODIFY
```

Test commands:
- Host: `cd host && export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace --offline`
- Lint: `cargo clippy --workspace --offline --all-targets -- -D warnings`
- Helper and companion builds are unchanged, but a fresh worktree needs them before `cargo test`:
  `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py all`.
- Install and device run (Task 7 only): `python3 tools/build.py install && python3 tools/device_test.py`.

---

### Task 1: Records, names and the `phone` field

**Files:**
- Create: `host/lantern/src/naming.rs`
- Modify: `host/proto/src/lib.rs`, `host/proto/tests/golden.rs` (assert unchanged only),
  `daemon/companion.rs`, `daemon/ipc.rs` (strip `open.phone`), `window/identity.rs`,
  `window/mod.rs`, `unlock.rs` (`reopen_args`, `LockEvent::Reopen`), `src/lib.rs`, `docs/protocol.md`
- Test: proto unit tests, `naming.rs`/`identity.rs`/`companion.rs` units, `tests/ipc.rs`, `window/mod.rs` units

**Interfaces (Produces):**
- Proto:
  - `Control::Open { …, #[serde(default, skip_serializing_if = "Option::is_none")] phone: Option<String> }`
  - `Control::Daemon { cmd: DaemonCmd, #[opt] serial: Option<String> }`
  - `StatusReport { …, #[serde(default, skip_serializing_if = "Vec::is_empty")] phones: Vec<PhoneReport> }`
  - `#[serde(rename_all="snake_case")] pub enum PhoneState { Unplugged, Waiting, Off, Starting, Ready, Retrying, Reconnecting, Undoing }`
  - `pub struct PhoneReport { serial: String, #[opt] name: Option<String>, state: PhoneState, plugged: bool, #[skip if false] off: bool, #[opt] connected_at: Option<u64> }`
  - `PhoneState::is_live(self) -> bool` (Starting, Ready, Retrying, Reconnecting)
- `naming.rs`:
  ```rust
  pub fn tag(serial: &str) -> String;                       // decision 2
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct Naming { pub primary: Option<String> }
  impl Naming {
      pub fn load(state_dir: &Path) -> Naming;              // <state>/menu.json; absent/bad = no primary
      pub fn save(&self, state_dir: &Path) -> Result<()>;   // {"version":2,"primary":…}, temp+fsync+rename
      pub fn migrated(state_dir: &Path) -> bool;            // menu.json has version >= 2
      pub fn app_id(&self, pkg: &str, user: u32, serial: &str) -> String;
      pub fn size_key(&self, pkg: &str, user: u32, serial: &str) -> String;
  }
  pub fn category(serial: &str) -> String;                  // "X-Lantern-<tag>"
  pub fn directory_file(serial: &str) -> String;            // "lantern-phone-<tag>.directory"
  ```
- `companion.rs`:
  ```rust
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct Record { pub companion: bool, pub since: Option<u64>, pub name: Option<String>,
                      pub off: bool, pub live: bool, pub connected_at: Option<u64> }
  pub fn read_record(state_dir: &Path, serial: &str) -> Option<Record>;
  /// Creates `{}` if absent; keeps unknown keys; always writes "serial"; temp + fsync + rename.
  pub fn update_record(state_dir: &Path, serial: &str,
                       f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) -> Result<bool>;
  pub fn set_off(state_dir: &Path, serial: &str, off: bool) -> Result<bool>;
  pub fn set_live(state_dir: &Path, serial: &str, live: bool) -> Result<bool>; // true also sets connected_at = now
  pub fn known_serials(state_dir: &Path) -> Vec<String>;  // "serial" key, else the file stem
  ```
  `record_setup` returns true when the record lacks `companion:true`. It then sets `companion:true`
  and `since` if missing, keeps every other key, and goes through `update_record`. `record_name`
  goes through `update_record` too (this fixes the "name temp file isn't fsynced" minor).
- `Identity { pkg, user, phone: Option<String> }`. `parse_args` accepts `--phone S` / `--phone=S`.
  Usage errors: given twice, empty, starts with `-`, contains a control char or whitespace, or
  longer than 200 chars.
- Window:
  - With `id.phone = Some(s)`: app_id = `Naming::load(state).app_id(pkg, user, s)`, and the
    size key = `size_key(…)`.
  - With `None`, until Task 5: today's `identity::app_id` / `Identity::size_key`.
  - The window puts `phone` in its `open`.
- `ipc`: `on_open` strips `phone` before forwarding to the helper. It is otherwise ignored until
  Task 5.
- `unlock`:
  - `reopen_args(pkg, user, phone: Option<&str>)` → `open [--phone S] [--user N] -- pkg`.
  - `LockEvent::Reopen { serial: String, apps: Vec<(String, u32)> }`; the reopen passes `--phone`.

- [ ] **Step 1: Failing tests**
  - Proto:
    - `open_without_phone_is_v1_bytes`: the existing `control_open.bin` golden still matches.
    - `open_phone_json_shape`: `…,"dpi":160,"phone":"ZD222LMDFV"}`.
    - `daemon_serial_optional`: `{"t":"daemon","cmd":"connect"}` round-trips, and so does the
      form with `"serial":"a"`.
    - `status_report_without_phones_is_unchanged`.
    - `phone_report_json_shape`: `{"serial":"a","state":"waiting","plugged":true}`, with `off`,
      `name` and `connected_at` absent.
  - `naming`:
    - `tag_keeps_alnum_and_underscore`: `"192.168.1.5:5555"` gives `"192_168_1_5_5555"`.
    - `app_id_primary_is_legacy`: primary `"P"`: `app_id("com.a",10,"P") == "lantern.com.a-u10"`.
    - `app_id_other_phone_suffix`: `"lantern.com.a-u10-pZD222LMDFV"` and
      `"lantern.com.a-p20e9318"`.
    - `size_key_primary_legacy_else_at_serial`: `"com.a:10"` and `"com.a:10@20e9318"`.
    - `no_primary_means_every_phone_suffixed`.
    - `naming_load_missing_or_bad_is_default`.
    - `save_load_roundtrip`.
  - `identity`:
    - `parses_phone_forms`.
    - `rejects_bad_phone`: `["--phone"]`, `["--phone",""]`, `["--phone","-x","com.a"]`,
      `["--phone","a b","com.a"]`, and given twice.
  - `companion`:
    - `record_flags_roundtrip_keep_unknown_keys`: `{"companion":true,"since":42,"extra":[1]}`,
      then `set_off(true)` and `set_live(true)`: `extra`, `since` and `serial` are kept,
      `connected_at` is a u64, and no temp file is left.
    - `set_flags_create_a_record_that_is_not_set_up`: then `record_setup` returns true once.
    - `record_setup_after_undo_notifies_again`: `companion:false` gives true.
    - `known_serials_reads_serial_key_and_stems`: `devices/_.._x.json` with
      `"serial":"../x"` gives `"../x"`; an old record without the key gives its stem.
  - `tests/ipc.rs` `open_phone_is_stripped_before_helper`: the fake helper's `open` has no
    `phone` key (raw JSON check).
  - `window/mod.rs` `remembered_size_per_phone`: two keys for the same pkg on two phones are
    kept apart in `windows.json`.
  - `unlock` `reopen_args_name_the_phone`.
- [ ] **Step 2:** Run the host tests. Expected: FAIL to compile.
- [ ] **Step 3:** Implement. `naming.rs` is shared by the window and lanternd: it does file IO
  only in `load`/`save`.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS. `git diff --stat host/proto/golden`
  is empty.
- [ ] **Step 5: Docs.** `protocol.md`: `open.phone` (window IPC; stripped before the helper),
  `daemon.serial`, and `status_report.phones` with the `PhoneState` values.
- [ ] **Step 6: Device check:** none of its own. Item 2 (Task 5) covers the window's app_id.
- [ ] **Step 7:** Commit with `feat: per-phone records and names; lantern open --phone; optional open.phone (v1 bytes unchanged)`.

### Task 2: Per-phone menu entries and folders; the one-time migration

**Files:**
- Modify: `daemon/catalog.rs`, `daemon/mod.rs` (`run` migrates before building the catalogue),
  `undo.rs`
- Test: `catalog.rs` units, `undo.rs` units, `tests/fake_adb.rs` (undo record), device item 23 and
  the item 10/15/16 helpers

**Interfaces (Produces):**
- `Dirs` gains `state: PathBuf` (`$XDG_STATE_HOME/lantern`); `Dirs::resolve(data, config,
  state, home, exe)`.
- `desktop_file(e: &AppEntry, serial: &str, naming: &Naming, exe: &Path, icon: Option<&Path>) -> String`,
  with lines in this order:
  - `Name`
  - `Exec=<exe> open --phone <serial> [--user N] <pkg>`. The serial is bare when it matches
    `[A-Za-z0-9._:-]+`, else it is quoted like the exe.
  - `Icon`
  - `Categories=X-Lantern-<tag>;`
  - `StartupWMClass=<app_id>`
  - `StartupNotify=true`
  - `X-Lantern-Serial=<serial>`
  - `X-Lantern-Package`
  - `X-Lantern-User`
  
  There is no `X-Lantern-Serials` line. The icon file is `icons/<app_id>.png`.
- `Catalog`:
  - `with_rebuild(dirs, rebuild)` loads `Naming` from `dirs.state`.
  - `set_names(Names)` names the folders.
  - `start_session(serial)` makes `serial` primary and saves `menu.json` if there is no primary
    yet, then rewrites the menu (a name may have changed).
  - `upsert`, `remove` and `end_sync` act only on the session phone's own files. `upsert` writes
    the entry before the menu, which fixes the Plan 3 minor.
  - `remove_serial(serial)` deletes that phone's entries, its icons and its folder file.
- `pub fn phones_with(dirs: &Dirs, pkg: &str, user: u32) -> Vec<String>`: the serials that have
  an entry for (pkg, user). Legacy multi-serial files count each listed serial.
  `Catalog::phones` delegates to it.
- `pub fn app_name(dirs: &Dirs, pkg: &str, user: u32) -> String`: the Name of any entry for
  (pkg, user), else `pkg`. Task 3's `PlugIn` and Task 5's `OpenRefused` use it.
- `pub fn ensure_menu(dirs: &Dirs, phones: &[(String, Option<String>)]) -> Result<bool>`:
  - It writes `lantern.menu`, with the "Phone" parent containing one `<Menu>` per phone that has
    ≥1 entry, sorted by display name:
    ```
    <Menu><Name>lantern-<tag></Name><Directory>lantern-phone-<tag>.directory</Directory>
      <Include><Category>X-Lantern-<tag></Category></Include></Menu>
    ```
  - It writes `lantern-phone.directory` (unchanged) and, per phone,
    `[Desktop Entry]\nType=Directory\nName=<escaped name or serial>\nIcon=smartphone\nX-Lantern-Serial=<serial>\n`.
  - It removes a `lantern-phone-*.directory` with `X-Lantern-Serial=` whose phone has no entry
    left, and both menu files when no entry is left.
- `pub fn migrate(dirs: &Dirs, pins: Option<&str>) -> Result<Migration>` with `pub struct Migration
  { pub primary: Option<String>, pub written: usize, pub removed: usize }`. It runs only when
  `!Naming::migrated(&dirs.state)`:
  1. List the owned `lantern.*.desktop` files (`X-Lantern-Serial=` present). Their phones are
     `X-Lantern-Serials`, else `[X-Lantern-Serial]`. A file without a valid package/user is
     skipped with a warning and left as is.
  2. Pick the primary with `choose_primary(&legacy, pins)` (decision 1), where `pub fn
     choose_primary(legacy: &[(String /*stem*/, Vec<String>)], pins: Option<&str>) ->
     Option<String>` counts `applications:<stem>.desktop` occurrences in `launchers=` lines.
  3. For each file and each listed phone: write the per-phone text, taking `Name` verbatim and
     copying the old icon file to the new icon path. Then delete the old file and icon if they
     aren't one of the targets.
  4. Write `menu.json` last.
- `daemon::run`: `migrate` before `Catalog::new`, with `pins` read from
  `$XDG_CONFIG_HOME/plasma-org.kde.plasma.desktop-appletsrc` (None if unreadable). It logs
  `menu migrated: primary=… written=… removed=…`.
- `undo::run_steps` step 4 becomes: `update_record` sets `off:true, live:false`, plus
  `companion:false` when the phone steps succeeded. The output line is `device record: kept,
  switched off` or `device record: kept, switched off (phone steps failed)`.

- [ ] **Step 1: Failing tests** (`catalog.rs`; two serials `A`, `B`; primary `A` unless stated)
  - `per_phone_entries_and_folders`: A and B both sync `com.s`. Expected:
    - `lantern.com.s.desktop` lists only A, with `--phone A` and `Categories=X-Lantern-A;`.
    - `lantern.com.s-pB.desktop` exists, with `StartupWMClass=lantern.com.s-pB`.
    - `lantern.menu` holds both submenus; the folder names come from the `names` closure.
  - `exec_quotes_odd_serial`: serial `a b` gives a quoted argument.
  - `full_sync_prunes_only_its_phone`, `remove_serial_drops_one_phone` and
    `full_sync_keeps_absent_profile` are adapted to per-phone files.
  - `phones_with_lists_per_phone_and_legacy_entries`.
  - `first_session_becomes_primary`: no `menu.json`; `start_session("B")` saves primary B.
  - `folder_goes_with_the_phones_last_entry`.
  - `tag_collision_is_refused`: serials `a:b` then `a_b` give an `Err`, and the first file's bytes
    are unchanged.
  - `migrate_splits_shared_entry_per_phone`: legacy `lantern.com.s.desktop` with
    `X-Lantern-Serials=A;B;` plus an icon gives the two per-phone files and two icons, and the old
    icon stays because it is A's target.
  - `migrate_renames_other_phones_only_entries`: B-only `lantern.com.b.desktop` is replaced by
    `lantern.com.b-pB.desktop`, and the old file and icon are gone.
  - `migrate_plan2_single_serial_file`: a file without `X-Lantern-Serials`.
  - `migrate_never_touches_foreign_files`: `lantern.foreign.desktop` (no marker),
    `dev.lantern.Lantern.desktop`, `org.kde.x.desktop`, a non-Lantern `.directory` and a stray
    icon all keep their bytes and mtime.
  - `migrate_is_idempotent_after_a_partial_run`: run, delete `menu.json`, plant the legacy file
    again, run again. The result is the same file set with no duplicates.
  - `migrate_runs_once`: with `menu.json` at version 2 nothing is read or written.
  - `primary_is_most_pinned_then_most_entries_then_smallest`: pins text
    `launchers=applications:lantern.com.b.desktop,preferred://browser` makes B primary although A
    has more entries. With no pins the phone with most entries wins; on a tie, `A`.
  - `undo.rs` `catalogue_entries_of_the_serial_are_removed` is adapted; the other phone's files
    stay.
  - `tests/fake_adb.rs`:
    - `undo_run_steps_keeps_record_when_forget_fails` now expects `device record: kept, switched
      off (phone steps failed)`, with the record holding `off:true`.
    - `undo_keeps_record_switched_off`: on the success path the record holds `companion:false`
      and `off:true`.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. `connect_notice`/`paused_notice` look up the app's name and icon
  from any of its entries (`phones_with`).
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS.
- [ ] **Step 5: Device item 23 `per_phone_folders`** (DEFERRED to Task 7) and helper updates in
  `tools/device_test.py`:
  - New helper `entry_for(serial, pkg, user=0)`: the `(path, keys)` of the `lantern.*.desktop`
    whose `X-Lantern-Serial`, `X-Lantern-Package` and `X-Lantern-User` match. New helper
    `app_id_of(pkg)`: that path's stem for `SERIAL`, or `lantern.<pkg>` when there is none.
  - Items 10 and 15 use `entry_for` instead of `desktop_entry(f"lantern.{pkg}")`. Item 15 plants
    a per-phone file (`lantern.dev.lantern.test.other-plantern_test_other.desktop`,
    `X-Lantern-Serial=lantern-test-other`, `Categories=X-Lantern-lantern_test_other;`) and checks
    it is byte-identical after the sync. Item 16 plants the same per-phone format for
    `dev.lantern.test.absent`.
  - Item 23:
    1. Plant a foreign `lantern.dev.lantern.test.foreign.desktop` (no marker).
    2. Restart lanternd, then wait for ready + companion ready + 10 s.
    3. Check that `lantern.menu` contains `<Category>X-Lantern-<tag(SERIAL)></Category>`.
    4. Check that `lantern-phone-<tag>.directory` has `Name=<device_name>`.
    5. Check that `entry_for(SERIAL, SETTINGS)` has `--phone SERIAL` in its `Exec`.
    6. Check that no `lantern.*.desktop` has an `X-Lantern-Serials=` line.
    7. Check that the foreign file is byte-identical.
    8. In a `finally`, delete the planted file.
- [ ] **Step 6:** Commit with `feat(catalog): one menu folder per phone; per-phone entries; one-time migration keeps the primary phone's names`.

### Task 3: Connect rules in lanternd

**Files:**
- Create: `daemon/phones.rs`
- Modify: `daemon/{mod,control,ipc}.rs`, `settings.rs`, `main.rs`, `unlock.rs` and `tray/{menu,mod}.rs`
  (compile-level only; see below), `tools/device_test.py`
- Test: `phones.rs` units, `settings.rs` units, `tests/ipc.rs`, `tests/cli.rs`, device items 19 and 24

**Interfaces (Produces):**
- `settings.rs`:
  - `Settings.autoconnect: bool` (default false) and `Key::Autoconnect` (`"autoconnect"`).
  - `HEADER` gains the line `# autoconnect: connect a phone as soon as it is plugged in
    (true/false; when false, Lantern asks each time).`
  - `SettingsHandle::fixed(Settings) -> SettingsHandle`, for tests: no file, `set` updates the
    watch only.
- `phones.rs` (pure):
  ```rust
  pub struct Timing { pub unplug_grace: Duration, pub pending_for: Duration } // Default 10 s, 120 s
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum OnPlug { Connect, Prompt, Wait, Ignore }
  pub fn on_plug(serial: &str, configured: Option<&str>, autoconnect: bool, off: bool, live: bool,
                 pending: bool, wanted: Option<&str>) -> OnPlug;
  ```
  It is evaluated in this order:
  1. `configured` is set: Connect for that serial, Ignore for any other.
  2. `pending`: Connect, which switches the wanted phone off.
  3. `live`: Connect if nothing is wanted, else Wait.
  4. `autoconnect && !off`: Connect if nothing is wanted, else Prompt (decision 4).
  5. `autoconnect && off`: Wait.
  6. `!autoconnect`: Prompt.
- `control.rs`:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum Command { Connect { serial: String, open: Option<(String, u32)> },
                     Disconnect { serial: String }, UndoSetup { serial: String }, Quit }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct PhoneView { pub serial: String, pub name: Option<String>, pub state: PhoneState,
                         pub plugged: bool, pub off: bool, pub locked: bool,
                         pub companion: Option<(CompanionState, Option<String>)> }
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct View { pub phones: Vec<PhoneView> }   // plugged in online order, then known by name
  impl View { pub fn live(&self) -> Option<&PhoneView>; pub fn phone(&self, serial: &str) -> Option<&PhoneView>; }
  impl PhoneView { pub fn display_name(&self) -> &str; pub fn report(&self, connected_at: Option<u64>) -> PhoneReport; }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum ConnectEvent {
      PluggedIn { serial: String, name: String },                  // one per plug-in that prompts
      OpenRefused { serial: String, name: String, app: String, pkg: String, user: u32 }, // Task 5 sends
      PlugIn { serial: String, name: String, app: Option<String> }, // Connect for an unplugged phone
      Gone { serial: String },                                     // connected or unplugged: close its notices
  }
  pub type Opener = Arc<dyn Fn(&str, &str, u32) + Send + Sync>;   // (serial, pkg, user)
  ```
  `Phone`, the old `View { phone, name, locked }` and `From<DaemonCmd> for Command` are removed.
- `Parts` gains:
  - `state_dir: Option<PathBuf>`: the records. With `None`, the flags live in memory only.
  - `timing: Timing`
  - `connect_events: Option<mpsc::Sender<ConnectEvent>>`: `try_send`; a full channel logs and
    drops.
  - `opener: Option<Opener>`
- `drive` semantics. The phone table replaces `connected`, `paused` and `serve_next`.
  - **Wanted phone.** One `wanted: Option<String>` is the only start target. Retries keep it.
  - **Online(S):**
    - S is the wanted phone in its grace: the grace ends, and a start is due now with
      `failures = 0`. There is no prompt.
    - Otherwise `on_plug(…)` decides: Connect sets wanted (closing another live phone without
      touching its `off`, and clearing its `live`). Prompt sends `PluggedIn`. Wait and Ignore do
      nothing.
    - Records are read at Online. Without settings, autoconnect counts as on (decision 7).
  - **Offline(S):**
    - S wanted: the session closes, and the grace starts (`Reconnecting`). When it expires: a real
      unplug, so `live:false` is written and wanted is cleared. Nothing else starts.
    - Not wanted: S leaves `plugged` and `Gone{S}` is sent.
  - **Start-up rule:** any record with `live:true` whose phone is not online within
    `timing.unplug_grace` of start gets `live:false` (Review Focus 2).
  - **Ready(S):** `set_live(S, true)` (sets `connected_at`). `Gone{S}` is sent. If a pending
    request for S exists and hasn't expired, it is removed and `opener(S, pkg, user)` is called
    once.
  - **Commands:**
    - `Connect{S, open}`: `set_off(S, false)`. With `open`, pending[S] is set with
      `until = now + pending_for`, replacing an earlier one. If S is plugged it becomes wanted;
      otherwise `PlugIn{S, name, app}` is sent, with `app` from the catalogue's entry name.
    - `Disconnect{S}`: `set_off(S, true)` and `set_live(S, false)` **before** any close. If S is
      wanted, the start is cancelled, the session closes and wanted is cleared. The top-level
      status now reports `paused` with S.
    - `UndoSetup{S}` (S plugged): `set_off(S, true)` and `set_live(S, false)` first; the session
      closes only if S is wanted; then the undo runs. Commands other than Quit are ignored while
      it runs (unchanged).
    - `Quit`: `set_live(wanted, false)`, then the shutdown path.
  - **Stop signal** (SIGTERM/SIGINT): flags untouched.
  - **View.** It is sent on every change. `PhoneView.state` is Undoing, Ready, Starting (start in
    flight or due), Retrying (backoff with failures), Reconnecting (grace), Off (plugged and
    `off`), Waiting (plugged), or Unplugged. Known phones come from
    `companion::known_serials(state_dir)`, re-read after each record write. With `configured`,
    only that serial is listed.
- `Hub`:
  - `set_view(&View)` each loop turn; the status gets `phones` from it, with `connected_at`
    from the records.
  - `take_commands() -> Vec<Command>` resolves a `daemon{cmd, serial: None}`:
    - exactly one phone plugged: that phone;
    - none: reply `error{"no phone is plugged in"}`;
    - several: reply `error{"several phones are plugged in: <name> (<serial>), …; name one:
      lantern connect <serial>"}`.
    
    In both error cases nothing is queued.
- CLI: `lantern connect [<serial>]`, `lantern disconnect [<serial>]`. A third argument is a usage
  error (exit 2). `USAGE` is updated.
- `daemon::run`:
  - `Parts.state_dir = companion::state_dir()`.
  - `timing.unplug_grace` comes from `LANTERN_UNPLUG_GRACE_MS` when it parses.
  - `opener` spawns `lantern open --phone S [--user N] -- pkg` through `desktop::spawn` in
    `spawn_blocking`, as `unlock::reopen` does.
- Tray and unlock (compile-level only; Task 4 replaces the menu):
  - `menu.rs` renders today's rows from `view.live()`, or the most recent Off phone, with ids
    1–13 kept.
  - Row 3 sends `Disconnect{serial}` or `Connect{serial, open: None}` for that phone.
  - Undo sends `UndoSetup{serial}` after kdialog, with the R9 serial check unchanged.
  - `unlock::phone_name` and `begin_asking` read `view.live()`.

- [ ] **Step 1: Failing tests**
  - `phones.rs`: `on_plug_table`, one assert per row above, including `Ignore` for another serial
    under `configured`.
  - `settings.rs`: `autoconnect_defaults_off`, `parses_autoconnect`; `defaults_when_absent` is
    updated.
  - `tests/ipc.rs`, through a `start_ctl_with` variant taking `state_dir`,
    `SettingsHandle::fixed(Settings{autoconnect,..})`, `Timing{unplug_grace: 300 ms, pending_for:
    2 s}`, a `ConnectEvent` receiver and a recording `Opener`:
    - `autoconnect_off_waits_and_prompts`: dev online gives no session, view `Waiting` and one
      `PluggedIn{dev,"Pixel"}`. `Connect{dev}` then gives ready.
    - `autoconnect_on_connects_unless_off`: online gives ready. Disconnect gives the record
      `off:true`. An offline longer than the grace followed by online gives no session and no
      event. A new daemon on the same `state_dir` with dev online gives no session. Connect gives
      ready and `off:false`.
    - `live_flag_reconnects_after_restart_without_prompt`: autoconnect off. Connect, ready, stop
      signal; the record still has `live:true`. A new daemon with dev online gives ready and no
      `PluggedIn`.
    - `quit_clears_live`.
    - `autoconnect_switched_on_connects_nothing_now`: dev waiting with autoconnect off; set it
      on; no session for 3 × the retry max. Only the next plug-in connects.
    - `blip_never_prompts`: ready; offline, then online 100 ms later. The view passes
      `Reconnecting` and ready returns, with no `PluggedIn`. Repeated three times.
    - `unplug_beyond_grace_clears_live`: after the grace the record has `live:false`; the next
      online prompts.
    - `stale_live_flag_cleared_when_absent_at_start`: a record with `live:true` and no Online
      within the grace gives `live:false`; a later Online prompts.
    - `configured_serial_bypasses_autoconnect_and_off`: configured `dev` with the record
      `off:true` and autoconnect off gives ready; `b` online gives no view entry and no event.
    - `checking_b_switches_a_off_without_off_flag`: dev ready; `Connect{b}`. dev gets `bye`, b
      becomes ready, dev's record has `off:false, live:false`, and dev's view state is `Waiting`.
    - `live_phone_gone_starts_nothing`: a ready and b waiting; after a's grace, b is not started.
    - `per_phone_off_flags_are_independent` (two-phone rule): disconnect a, connect b, disconnect
      b, then unplug and replug both with autoconnect on. Neither starts, and both records have
      `off:true`.
    - `pending_connect_opens_once`: b unplugged; `Connect{b, open:("com.x",0)}` gives
      `PlugIn{b,…}`. b online with autoconnect off gives ready and `opener` called once with
      `(b,"com.x",0)`. A blip and ready again don't call it again.
    - `pending_is_per_phone`: a pending request for b, then a connects first: `opener` is not
      called for a.
    - `pending_expires`: online after `pending_for` prompts and doesn't connect.
    - `later_request_replaces`: `com.x` then `com.y` for b: only `com.y` opens.
    - `undo_keeps_record_and_sets_off`: after the fake undo, dev's view state is `Off` and its
      record has `off:true`.
    - `undo_of_waiting_phone_leaves_live_session`: a ready; undo b. No `bye` reaches a's helper,
      and the runner is called with `b`.
    - `daemon_cmd_without_serial_needs_one_plugged`: two plugged give the error naming both; one
      plugged works; an explicit serial works.
    - `status_lists_every_phone`: `phones` has the states and names. After a disconnect the
      top-level is `paused` with that serial.
  - Plan 3 tests whose rule changed, each replaced by a test above:
    - `offline_of_live_phone_starts_the_next_online_phone` and
      `disconnect_starts_the_other_online_phone` → `live_phone_gone_starts_nothing`.
    - `offline_of_live_phone_skips_a_paused_phone` → deleted.
    - `second_disconnect_keeps_the_first_phone_paused` → `per_phone_off_flags_are_independent`.
    - `undo_end_starts_the_other_online_phone` → `undo_end_starts_nothing`.
    - `disconnect_pauses_until_replug`: now stays off after a replug.
  - `tests/cli.rs`: `connect_takes_one_optional_serial` (a third argument gives exit 2).
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Flag writes are small synchronous file writes done on the loop, in
  order, before any close (as `pause` was). A failed write is logged and the in-memory flag still
  applies.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS.
- [ ] **Step 5: `tools/device_test.py` changes and device items** (DEFERRED to Task 7):
  - **Preflight in `main()`:**
    - If `systemctl --user show-environment` lacks `LANTERN_SERIAL=<SERIAL>`, set it and restart
      lanternd.
    - In the final `finally`, restore the previous value (unset or the old value) and restart,
      each step guarded.
    - Print `lanternd LANTERN_SERIAL=<SERIAL> for this run`.
  - **Helpers:**
    - `phone_status(serial)`: the entry of `status()["phones"]` (or `{}`).
    - `manager_env(set={}, unset=[])`: a context manager that changes the systemd user
      environment, restarts lanternd, and restores and restarts on exit.
    - `replug()`: `adb -s SERIAL reconnect`. It waits ≤5 s until the serial is missing or
      `offline` in `adb devices`, then ≤15 s until it is back as `device`. If it never went
      offline it FAILs with `adb reconnect did not drop the phone; run --manual and pull the
      cable`. Under `--manual` it asks Sahaj to pull the cable and plug it back in instead.
  - **Item 19** additionally checks `phone_status(SERIAL)["state"] == "off"` and the record's
    `off:true` after `lantern disconnect`, and `off` absent after `lantern connect`. It now runs
    `lantern disconnect SERIAL` / `lantern connect SERIAL`.
  - **Item 24 `autoconnect_on_remembers_off`:**
    1. Save `settings.toml`, set `autoconnect = true`, and enter `manager_env(set={
       "LANTERN_UNPLUG_GRACE_MS": "0"}, unset=["LANTERN_SERIAL"])`. Wait for ready: the `live`
       flag set earlier in the suite reconnects the phone.
    2. `lantern disconnect SERIAL`; the phone is restored within 5 s.
    3. `replug()`: not ready and state `off` for 15 s.
    4. `systemctl --user restart lanternd`: state `off` for 15 s.
    5. `lantern connect SERIAL`: ready within 15 s.
    6. `replug()`: ready again within 20 s by itself.
    7. Restore the file and the environment in that order, each guarded, and leave lanternd ready.
- [ ] **Step 6:** Commit with `feat(daemon): per-phone off/live flags, autoconnect, replug grace, pending connect-and-open; one phone live`.

### Task 4: The tray as the phone switchboard

**Files:**
- Modify: `tray/{menu,mod,dbusmenu,sni}.rs`, `unlock.rs` (`Request` carries the serial),
  `tools/device_test.py` (items 20, 22)
- Test: `menu.rs` units, `tests/tray.rs`, `tests/unlock.rs`

**Interfaces (Produces):**
- `menu.rs`:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum Action { Connect { serial: String }, Disconnect { serial: String }, ToggleClipboard,
                    ToggleKeyboard, ToggleAutoconnect, ToggleLogin, OpenSettings,
                    UndoSetup { serial: String }, Unlock { serial: String }, Quit }
  pub enum Item { Label{..}, Separator{..}, Button{..}, Check{..},
                  Submenu { id: i32, text: String, children: Vec<Item> } }
  #[derive(Debug, Default)] pub struct Slots { /* serial -> n, never reused */ }
  impl Slots { pub fn slot(&mut self, serial: &str) -> i32; }   // 1, 2, …
  pub fn items(view: &View, settings: &Settings, login: Option<bool>, settings_error: bool,
               slots: &mut Slots) -> Vec<Item>;
  pub fn action(items: &[Item], id: i32) -> Option<Action>;     // searches submenus too
  ```
- Rows, top to bottom (n = the phone's slot):

  | id | row | shown when |
  |---|---|---|
  | `100n` | Check `{name}`, on = live state; action `Disconnect{serial}` when on, else `Connect{serial}` | each plugged phone |
  | `100n+1` | Label: `Connecting…` (Starting) · `Reconnecting…` (Retrying, Reconnecting) · `Locked` · `Companion: {msg}` · `Typing through the Lantern keyboard is off` · `Removing Lantern…` (Undoing) | that state |
  | `100n+2` | Button `Unlock {name}…`, action `Unlock{serial}` | the phone is locked |
  | 1 | Label `No phone plugged in` | no phone plugged |
  | 15 | Label `Lantern connects one phone at a time` | ≥2 phones plugged |
  | 12 | Label `SETTINGS_ERROR` | settings file error |
  | 4 | Separator | always |
  | 5, 6 | the two existing checks | always |
  | 14 | Check `Connect phones when plugged in` (`ToggleAutoconnect`) | always |
  | 7 | Check `Start Lantern at login` | `login` is `Some` |
  | 8 | Button `Open settings folder` | always |
  | 9 | Separator | always |
  | 10 | Submenu `Remove Lantern from a phone…`; child `100n+50` Button `{name}`, enabled when plugged and not Undoing, text `{name} (not plugged in)` when unplugged | any known phone |
  | 11 | Button `Quit Lantern` | always |

  Ids 2, 3 and 13 are retired.
  - `status_line` (tooltip): `Connected to {name}` · `Connecting to {name}…` ·
    `Reconnecting to {name}…` · `{n} phone(s) plugged in, none connected` ·
    `No phone plugged in`.
  - `overlay`: `view-refresh` (a live phone not Ready, or Undoing) · `emblem-warning` (Ready
    with a companion msg) · `media-playback-pause` (plugged, none live) · `""`.
  - `sni_status`: `Passive` with no phone plugged.
- `dbusmenu.rs`:
  - `GetLayout(parent_id)` works for the root and for a submenu.
  - A submenu item has `children-display = "submenu"`, and its children recurse while
    `recursion_depth` allows.
  - `GetGroupProperties`, `GetProperty` and `Event` find nested ids.
- `tray/mod.rs`: `State` holds the `Slots`. `ToggleAutoconnect` goes through the existing
  `toggle`. Undo asks kdialog with the clicked phone's name. On Remove it sends
  `UndoSetup{serial}` only if that serial is still plugged in the current view; otherwise it shows
  "Undo phone setup cancelled" (R9).
- `unlock.rs`: `Request::ask(serial: String)`. The prompt opens the dialog only if a stretch is
  open and `serial` is the live phone, and the answer is pinned to that serial (as before). The
  notification action uses the live phone's serial.

- [ ] **Step 1: Failing tests**
  - `menu.rs`:
    - one test per phone state with exact rows;
    - `no_phone_plugged_row`;
    - `two_phones_show_the_note_and_both_rows`;
    - `remove_submenu_lists_known_phones` (an unplugged phone is disabled with
      `(not plugged in)`);
    - `action_follows_check_state`: id `100n` is Connect when off and Disconnect when live.
  - `ids_stay_with_their_phone`: view [a, b], then [b] (a unplugged), then [c, b, a]. Id 200
    always maps to b's actions, and a new phone c gets slot 3.
  - `tests/tray.rs`:
    - `registers_and_serves_layout`: rows for one Ready phone.
    - `click_phone_row_sends_connect_or_disconnect`.
    - `submenu_layout_and_click`: `GetLayout(10,-1,[])` gives the children; `Event(150,
      "clicked")` reaches the confirm path, with kdialog faked by a PATH script exiting 0, and
      gives `UndoSetup{serial}`.
    - `autoconnect_toggle_writes_settings`.
    - `unlock_row_click_prompts` is adapted.
  - `tests/unlock.rs`: `tray_request_for_another_phone_opens_nothing`.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS.
- [ ] **Step 5: Device items 20 and 22** (DEFERRED to Task 7):
  - Item 20 `tray_menu`:
    1. Take the bus name after `baseline()` (unchanged).
    2. Parse `GetLayout 0 -1` top-level children with their `label` and `toggle-state`.
    3. Find the Check row whose label is `device_name` (underscores escaped). Its id is
       `100n`, and `toggle-state` is 1.
    4. Check rows 11 `Quit Lantern` and 14 exist.
    5. Click the phone row: within 5 s `phone_status(SERIAL)["state"] == "off"` and the phone is
       restored, and the row's `toggle-state` is 0.
    6. Click it again: ready within 15 s.
    7. Take the screenshot (unchanged).
  - Item 22: no row label starts with `Unlock` and no label equals `Locked` (rows 13 and 1
    are gone).
- [ ] **Step 6:** Commit with `feat(tray): one checkmark per plugged phone, per-phone status and Unlock, Autoconnect, Remove submenu`.

### Task 5: `lantern open` picks its phone; lanternd routes by phone

**Files:**
- Create: `host/lantern/src/window/resolve.rs`
- Modify: `window/{mod,outcome}.rs`, `daemon/{ipc,mod}.rs`, `daemon/catalog.rs` (`app_name`),
  `tools/device_test.py` (items 2, 11, 16, `kwin_on`)
- Test: `resolve.rs` units, `outcome.rs` units, `tests/ipc.rs`, `tests/cli.rs`

**Interfaces (Produces):**
- `resolve.rs` (pure):
  ```rust
  #[derive(Debug, PartialEq, Eq)]
  pub enum Target { Phone(String), Connect(Option<String>) }
  pub fn resolve(phones: &[PhoneReport], have: &[String]) -> Target;
  ```
  - live = phones whose `state.is_live()`.
  - (1) live ∩ have non-empty: `Phone(the one with the largest connected_at)`.
  - (2) have empty and live non-empty: `Phone(the most recent live)`. An app with no entry yet
    goes to the phone, as today.
  - (3) have non-empty, none live: `Connect(Some(the have-phone with the largest
    connected_at, unknown = 0, ties by serial))`.
  - (4) otherwise: `Connect(None)`.
- Window `run`, when `id.phone` is None:
  1. `ipc::query_status` (an error means today's NoPhone path).
  2. `catalog::phones_with(dirs, pkg, user)`.
  3. `resolve`.
  
  Then:
  - `Phone(s)` or `Connect(Some(s))`: the window acts as `--phone s` and sends `open{phone: s}`.
  - `Connect(None)`: it sends `open` without `phone`.
  
  The app_id and size key use the resolved serial (`Naming`).
- `ipc::CONNECT_OFFERED: &str = "phone not connected; Lantern offered to connect it"`.
  `Hub::on_open` with `phone: Some(s)`:
  - **s is the live session's phone:** forwarded (stripped), skipping `not_on_this_phone`.
  - **Otherwise, with the connect prompt present** (`Hub::set_connect_events` was called):
    reply `error(CONNECT_OFFERED)`, close, and queue `(s, pkg, user)`. The loop takes it with
    `Hub::take_refused()` and sends `ConnectEvent::OpenRefused{serial, name, app, pkg, user}`,
    where `app` comes from `catalog::app_name` (Task 2).
  - **Otherwise:** `PAUSED` if s's view state is `Off`, else `NOT_CONNECTED`.
  
  `phone: None` behaves as today.
- `outcome::Notice::Offered` comes from `CONNECT_OFFERED` on stream 0 before `opened`. The
  window shows no notification of its own. It logs `connect notice: Connect your {name}`, with
  `name` from the device records or the serial, and exits 1.

- [ ] **Step 1: Failing tests**
  - `resolve.rs`:
    - `one_live_phone_with_the_app`
    - `two_live_newest_wins`: shaped for 4c; two Ready phones with different `connected_at`.
    - `unknown_app_goes_to_live_phone`
    - `none_live_names_last_connected_owner`
    - `nothing_known_is_connect_none`
  - `outcome.rs`: `offered_is_a_notice_without_own_notification`.
  - `tests/ipc.rs`:
    - `open_naming_the_live_phone_is_forwarded`
    - `open_naming_another_phone_is_offered`: a `ConnectEvent::OpenRefused` with the entry's name
      as `app`, and the helper sees no `open`.
    - `open_naming_an_off_phone_without_prompt_is_paused_error`
    - `open_without_phone_keeps_plan3_rules` (the old refusal tests run unchanged).
  - `tests/cli.rs`: `open_with_phone_reaches_the_daemon_step` (exit 1 with no daemon, as today).
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. `daemon::run` calls `hub.set_connect_events` only when the connect
  prompt runs (Task 6 adds the prompt; until then nothing calls it).
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS.
- [ ] **Step 5: Device item updates** (DEFERRED to Task 7):
  - `kwin_on` substitutes `app_id_of(pkg)` for `CLASS`, and item 2 checks
    `resourceClass=<app_id_of(SETTINGS)>`. This proves the window without `--phone` resolved
    `SERIAL` and named itself after that phone's entry.
  - Item 11 also checks that `~/.local/state/lantern/windows.json` gained the key
    `Naming.size_key(SETTINGS, 0, SERIAL)` after the window closed. Compute it in Python: the bare
    package when `menu.json` primary == SERIAL, else `<pkg>@<SERIAL>`.
  - Item 16 keeps its check `connect notice: Connect your Test Phone` in the window's stderr; it
    passes through both the offered and the fallback path.
- [ ] **Step 6:** Commit with `feat(window): lantern open picks its phone; lanternd routes open.phone and offers Connect`.

### Task 6: Plugged-in and Connect-your-phone notifications

**Files:**
- Create: `host/lantern/src/notices.rs`, `host/lantern/src/connect.rs`, `host/lantern/tests/connect.rs`
- Modify: `unlock.rs` (uses `notices`), `daemon/mod.rs` (`run` spawns the prompt and wires
  `connect_events` and `set_connect_events`), `src/lib.rs`, `tools/device_test.py` (items 25, 26)
- Test: `tests/connect.rs` (private no-servicedir bus and a fake `org.freedesktop.Notifications`,
  as in `tests/unlock.rs`; move the shared `Bus` and fake server into `tests/common/mod.rs`),
  `tests/unlock.rs` (unchanged, must still pass)

**Interfaces (Produces):**
- `notices.rs`: the `Notifier` extracted from `unlock.rs`, generalized.
  ```rust
  pub struct Notices { /* conn, server: Option<UniqueName>, warned */ }
  impl Notices {
      pub async fn new(conn: Connection) -> Result<(Notices, MessageStream)>; // ActionInvoked match rule
      pub async fn notify(&mut self, replaces: u32, summary: &str, body: &str,
                          actions: &[(&str, &str)]) -> u32;   // 0 on failure; 5 s timeout; one warning
      pub async fn close(&mut self, id: u32);
      pub fn action(&self, message: &zbus::Message) -> Option<(u32, String)>; // only the server's own signal
  }
  pub fn escape_markup(text: &str) -> String;                 // & < >
  ```
- `connect.rs`:
  ```rust
  pub fn plugged_notice(name: &str) -> (String, String);
  pub fn offered_notice(name: &str, app: &str) -> (String, String);
  pub fn plug_in_notice(name: &str, app: Option<&str>) -> (String, String);
  pub async fn run(conn: Connection, events: mpsc::Receiver<ConnectEvent>,
                   commands: mpsc::Sender<Command>) -> Result<()>;
  pub async fn spawn(events: mpsc::Receiver<ConnectEvent>, commands: mpsc::Sender<Command>)
      -> Option<JoinHandle<()>>;                              // own bus connection, 5 s; None = one warning
  ```

  Notices (exact text; `name` and `app` escaped):

  | Event | summary | body | action |
  |---|---|---|---|
  | `PluggedIn` | `{name} is plugged in` | `Connect it to use its apps on this PC. Its screen turns off while it is connected.` | `connect` / `Connect` |
  | `OpenRefused` | `Connect your {name}` | `{app} is on your {name}. Connect it to open {app} here.` | `connect` / `Connect` |
  | `PlugIn` | `Plug in your {name}` | `{app} opens if you plug it in within 2 minutes.`, or `It connects if you plug it in within 2 minutes.` without an app | none |

  Each serial has at most one notice on screen: a new event for that serial replaces its id.
  `Gone{serial}` closes it. A `connect` action on a shown id sends `Command::Connect{serial,
  open}` with `try_send` (`open` is `Some((pkg,user))` for `OpenRefused`), then closes that
  notice. Signals from other senders or for unknown ids are ignored.
- `daemon::run`:
  - `connect::spawn(connect_rx, cmd_tx.clone())`.
  - When it returns `Some`: `Parts.connect_events = Some(connect_tx)`, and the hub gets
    `set_connect_events`.
  - When it returns `None`: neither is set (plug-ins wait silently, and refusals use the old
    errors).

- [ ] **Step 1: Failing tests** (`tests/connect.rs`)
  - `plugged_in_notice_and_connect_click_sends_command`
  - `plugged_in_replaces_previous_notice`: two `PluggedIn` for dev give the second `Notify` with
    `replaces_id` = the first id, so one notice remains.
  - `refused_open_connect_carries_the_app`: the action gives `Connect{dev, open:Some(("com.x",0))}`.
  - `plug_in_has_no_action`
  - `gone_closes_the_notice`
  - `action_from_another_sender_is_ignored`
  - `no_notification_server_is_harmless`: no `Notify` handler; one warning; events keep being
    consumed.
  - `names_are_escaped`: name `A&B <1>` is sent as `A&amp;B &lt;1&gt;`.
  - `tests/unlock.rs` passes unchanged on the shared `notices` code.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Move the code; don't copy it: `unlock.rs` keeps only its own state
  machine.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS.
- [ ] **Step 5: Device items** (DEFERRED to Task 7). Both run inside `manager_env(set={
  "LANTERN_UNPLUG_GRACE_MS": "0"}, unset=["LANTERN_SERIAL"])` with `settings.toml` saved and
  restored, and a `busctl --user monitor org.freedesktop.Notifications` log as item 22 keeps one.
  Only matched summaries are printed.
  - **Item 25 `autoconnect_off_asks_on_plug`** (automatic):
    1. Set `autoconnect = false`. The restart reconnects the phone through its `live` flag:
       ready, and no `is plugged in` Notify for 5 s.
    2. `replug()`: within 5 s a Notify `<device_name> is plugged in`; state `waiting` and the
       phone restored for 10 s.
    3. Click the phone's tray row (item 20's lookup): ready within 15 s.
    4. Leave lanternd ready.
  - **Item 26 `connect_from_entry_opens_app`** (`--manual`, add to `MANUAL`):
    1. `lantern disconnect SERIAL`, then wait for the phone to be restored.
    2. `lantern open --phone SERIAL com.android.settings` exits 1 within 5 s, and within 5 s a
       Notify `Connect your <device_name>` carries action `connect`.
    3. Ask Sahaj to click **Connect** on that notification.
    4. Ready within 20 s; a window with `resourceClass=<app_id_of(SETTINGS)>` within 20 s; and
       after 10 s more, `status()["windows"]` holds exactly one `com.android.settings`.
    5. Close it through KWin.
- [ ] **Step 6:** Commit with `feat: "<phone> is plugged in" and "Connect your <phone>" notifications with Connect`.

### Task 7: Docs and device acceptance

**Files:**
- Modify: `docs/protocol.md` (daemon rules, window-IPC errors, `CONNECT_OFFERED`, the catalogue
  layout), `README.md` (tray, autoconnect, `lantern connect|disconnect [serial]`, `lantern open
  --phone`, re-pinning once), `docs/HANDOFF.md` (status, decisions 1–11, Known minor issues from
  the reviews, Next = Plan 4c), `tools/device_test.py` (docstring, item list)

- [ ] **Step 1: Docs.**
  - In `protocol.md`, replace "Daemon commands" with the per-phone rules (`off`, `live`,
    autoconnect, grace, pending).
  - In the window-IPC section, describe `open.phone`.
  - State the catalogue rule (one file per phone, `X-Lantern-Serial` only, primary names).
- [ ] **Step 2: Acceptance run.** On the real desktop, unlocked, with the Xiaomi (two phones
  attached is fine; the preflight pins `LANTERN_SERIAL`):
  `python3 tools/build.py install && python3 tools/device_test.py`.
  - Expected: all automatic items PASS: 1–7, 9–13, 15–20, 22–25.
  - Then `python3 tools/device_test.py --manual --only 26`. Give Sahaj the step ("click Connect on
    the notification") in chat before starting it.
  - Record the summary line and timings in HANDOFF.
  - Tell Sahaj in the status line that the install restarts lanternd (panel off, stay-awake 7).
- [ ] **Step 3: Migration check on the real menu**, run before Step 2's install.
  - Before: copy `~/.local/share/applications/lantern.*.desktop` and the Plasma appletsrc
    `launchers=` line to scratch.
  - After: the journal shows `menu migrated: primary=ZD222LMDFV …`, and
    `lantern.com.microsoft.rdc.androidx-u10.desktop` still exists, so the pinned Windows App
    launcher still works.
  - The Xiaomi's apps are `…-p20e9318.desktop`.
  - No `X-Lantern-Serials=` line is left.
  - `dev.lantern.Lantern.desktop` is unchanged.
  - Kickoff shows Phone → "Xiaomi 11T Pro" and Phone → "edge 50 pro". Ask Sahaj to look, and take
    a screenshot.
  - List which Xiaomi pins need re-pinning (none were pinned on 2026-11-14).
- [ ] **Step 4: Two-phone hand check** (both phones plugged in, `LANTERN_SERIAL` unset,
  autoconnect off):
  - The tray shows two unchecked rows, row 15, and two "is plugged in" notifications after a
    lanternd restart. Phones without a `live` flag are prompted.
  - Check the Xiaomi: ready. Check the Motorola: the Xiaomi is restored, its record has
    `off:false, live:false`, and the Motorola is ready.
  - Uncheck the Motorola: its record has `off:true`.
  - Opening a Xiaomi entry from Kickoff gives "Connect your Xiaomi 11T Pro". Its Connect button
    connects the Xiaomi and opens the app.
  - Tell Sahaj which phone was left connected.
- [ ] **Step 5:** Commit with `docs: Plan 4b acceptance, protocol and README updates`.
