# Lantern Desktop Integration (Plan 2 of 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Phone apps show up in the KDE menu with their real names and icons, and they track what is
installed. Windows are titled with the app's name. The PC keyboard and PC input method type any
Unicode text, including Vietnamese composition. Text clipboard syncs both ways. The phone-safety
items deferred from Plan 1 are closed first.

**Architecture:**
- A new **companion APK** (`dev.lantern.companion`) runs on the phone as an ordinary app in user 0.
  - It lists launcher apps for every profile through LauncherApps and renders their icons.
  - It is the hidden input method (IME).
  - It never talks to the PC. It connects to the helper over a token-protected abstract socket that
    the helper listens on, using the same frame format as the PC link.
- The **helper**:
  - Relays the companion's app catalogue to `lanternd` on stream 0.
  - Forwards window text (`ime`) to the companion.
  - Syncs the phone clipboard.
  - Journals and restores every setting it touches. That now includes the keyboard, not just
    stay-awake.
- **`lanternd`**:
  - Installs and updates the companion.
  - Writes `.desktop` files, icons and a "Phone" submenu.
  - Bridges the Wayland clipboard through `ext-data-control-v1`.
- **The window:** turns on winit IME (text-input-v3) and sends commit and compose text.

**Tech Stack:**
- Phone: Java 11 source level, javac/d8/aapt2/apksigner from `~/.local/opt/pd-toolchain`, no Gradle.
  This is the same as `spikes/build.py`, which is the working reference for the APK steps.
- PC: Rust, using the crates already in `host/lantern/Cargo.toml`. One crate is added: `base64`, at the
  version already in `~/.cargo/registry` (0.23).
  - The clipboard uses `wayland-client` 0.31 and `wayland-protocols` 0.32 with
    `ext::data_control::v1::client` (staging feature, already enabled).
  - Notifications use `notify-send` (present). Plan 3 moves them to D-Bus, because clicks need it.

**Spec:** `docs/superpowers/specs/2026-11-07-lantern-design.md`. The binding parts are §2 goals 2, 6
and 8; §4; §7.1 (companion); §8.1–8.4; §8.6 keyboard and clipboard; §9b IME. Read
`docs/HANDOFF.md` (Plan 2 list, Known minor issues) and `docs/protocol.md` before starting.

**Out of scope (Plan 3):**
- notifications and the notification listener
- drag-and-drop
- audio
- gamepads
- tray
- encoder pause
- the latency harness

## Global Constraints

- Phones:
  - Xiaomi 11T Pro `20e9318`: Android 17 / SDK 37.
  - Motorola edge 50 pro `ZD222LMDFV`: Android 16. Its user 10 is a managed work profile with
    `no_debugging_features`.
  - No root, only USB debugging.
- Build:
  - Compile against the SDK 36 `android.jar`.
  - Helper: `--min-api 35`.
  - Companion: `minSdkVersion 35`, `targetSdkVersion 36`.
- Companion:
  - Package `dev.lantern.companion`, installed for `--user 0` only.
  - IME id (exact): `dev.lantern.companion/.LanternIme`.
  - It never shows a view (`onEvaluateInputViewShown() == false`) and never logs typed text.
- **Every phone setting Lantern changes is journaled before the change and restored after.** That
  applies to clean ends, helper death, daemon death, USB yank and `lantern undo-setup`.
  - The original value is put back only while the setting still holds the value Lantern wrote. A
    value the user changed since is left alone and logged.
  - An originally absent key is deleted, never written as `null`.
- Settings changed per session (only these):
  - `global/stay_on_while_plugged_in` → `7`
  - `secure/enabled_input_methods` → original plus the Lantern IME
  - `secure/default_input_method` → the Lantern IME, selected with `ime set`
- Wayland `app_id` is unchanged: `lantern.<pkg>` for user 0, `lantern.<pkg>-u<N>` otherwise.
  The `.desktop` file is named `<app_id>.desktop`.
- Protocol stays **v1**. Every addition is a new message type or an optional field that is omitted
  when absent, so existing messages stay byte-identical. The helper jar and the companion APK ship
  inside `lantern` and are always deployed together.
- Size limits:
  - `clip.text`: at most 1 MiB of UTF-8 (`MAX_CLIP = 1 << 20` bytes).
  - `ime.text`: at most 64 KiB (`MAX_IME_TEXT = 1 << 16` bytes).
  - `app.icon`: a PNG of at most 1 MiB before base64.
- Every adb call targets the selected serial (`-s`).
- Never commit signing keys. The companion key lives at
  `~/.local/share/lantern/companion-signing.p12`.

## Review Focus

1. **USB yanked while the Lantern keyboard is selected.** Within 10 s the phone is back on its
   previous keyboard: both the setting and the *bound* IME, with no PC present. Pinned by
   `ImeWatchdogTest` (Task 7), `restoresImeWithSelectIme` (Task 1) and acceptance item 14 (Task 10).
2. **The user changes the keyboard or stay-awake during a session, or after a crash and before the
   next session.** Their value is never overwritten by a stale original. Pinned by Task 1
   `leavesUserChangedValue` and `recoveredEntryTrustedOnlyWhileCurrentIsSet`.
3. **Clipboard edge cases.**
   - Ping-pong: no loop.
   - Text over 1 MiB: not synced.
   - Secrets: a KDE password-manager copy (`x-kde-passwordManagerHint: secret`) and an Android
     clip flagged sensitive are never synced.
   - Pinned by Task 9 `ClipSyncTest` and `clip_sync_*`.
4. **Stale menu entries.**
   - An app uninstalled while the phone was unplugged, or a different phone connected: the entry
     goes away on the next full sync.
   - A partial sync (the link dropped mid-list) removes nothing.
   - Pinned by Task 6 `full_sync_prunes_*` / `partial_sync_prunes_nothing`.
5. **Hostile or odd labels** (newline, `=`, `;`, `%`, quotes, emoji, 10 kB long) and odd packages.
   The `.desktop` file stays valid, `Exec` is never built from the label, and an invalid package name
   is refused. Pinned by Task 6 `desktop_file_escapes_label` / `rejects_bad_package`.

---

## File Structure

```
android/helper/src/dev/lantern/helper/
  Journal.java          NEW  multi-entry journal (replaces Cleanup.Journal), parse v2 + legacy v1
  Change.java           NEW  one planned setting change (setting or IME selection)
  ImeSetup.java         NEW  the two IME changes, enabled-list string logic
  CompanionLink.java    NEW  helper side of the companion socket (auth, relay, send)
  ClipSync.java         NEW  phone-clipboard loop suppression / filters (pure)
  PhoneClipboard.java   NEW  device glue: vendored ClipboardManager + listener -> ClipSync
  Session, Cleanup, JournalStore, SettingsBackend, ShellSettings, Router, Main, Msg,
  DeviceUsers, UserLaunch, AppDisplay, DisplayHandle        MODIFY
android/companion/
  AndroidManifest.xml, res/xml/ime.xml, res/layout/test.xml
  src/dev/lantern/companion/{LinkReceiver,Link,LanternIme,ImeWatchdog,AppCatalog,TestActivity}.java
  test/dev/lantern/companion/{ImeWatchdogTest,AppCatalogTest}.java
host/proto/src/lib.rs                      MODIFY  new Control variants/fields
host/lantern/src/daemon/companion.rs       NEW  install/update, device record, consent notice
host/lantern/src/daemon/catalog.rs         NEW  .desktop/icon/menu writer, sync + prune
host/lantern/src/clipboard/{mod,wayland}.rs NEW  ClipSync (pure) + ext-data-control client thread
host/lantern/src/notify.rs                 NEW  notify-send wrapper (best effort)
host/lantern/src/undo.rs                   NEW  `lantern undo-setup`
host/lantern/src/daemon/{mod,session,ipc}.rs, window/{mod,identity,input,keymap}.rs, main.rs  MODIFY
tools/build.py (companion target), tools/device_test.py (items 9–14), docs/protocol.md, README.md,
docs/HANDOFF.md                                                                          MODIFY
```

---

### Task 1: Phone safety, multi-entry journal, deferred Plan 1 fixes

**Files:**
- Create: `Journal.java`, `Change.java`
- Modify: `Session.java`, `Cleanup.java` (drop the nested `Journal`), `JournalStore.java`,
  `SettingsBackend.java`, `ShellSettings.java`, `Main.java` (`--forget`)
- Modify: `host/lantern/src/daemon/session.rs` (pgrep anchor, prune order)
- Test: `JournalTest.java` (new), `SessionTest.java`, `JournalStoreTest.java`; the Rust unit test in
  `session.rs`

**Interfaces (Produces):**
- `final class Journal`:
  - `static final class Entry { final String ns, key, orig, set; }`. `orig == null` means the key
    was absent. `set` is the value Lantern wrote.
  - `Journal(List<Entry>)`, `List<Entry> entries()`, `Entry find(String ns, String key)`.
  - `String toJson()` produces
    `{"v":2,"entries":[{"ns":..,"key":..,"orig":..|null,"set":..},...]}`.
  - `static Journal parse(String)`. It also accepts the Plan 1 flat form `{"ns","key","orig"}` as one
    entry with `set:"7"`.
  - `void restore(SettingsBackend s, Consumer<String> log)` works on the entries in **reverse**
    order:
    - If `s.get(ns,key)` equals `set`, it puts back `orig`. For `secure/default_input_method` it
      calls `s.selectIme(orig)`, which also updates the setting. A null `orig` means `delete`.
    - Otherwise it logs `journal: <ns>/<key> changed since the session started (now <v>); left as is`.
    - Every entry is attempted even if one throws. The first exception is rethrown at the end.
- `SettingsBackend` gains `void selectIme(String id)`. `ShellSettings` implements it with
  `ime set <id>` and checks the exit code.
- `final class Change`:
  - `static Change setting(String ns, String key, UnaryOperator<String> next)`: `next` gets the
    current value, which may be null.
  - `static Change ime(String id)`: for `secure/default_input_method`, applied with `selectIme`.
  - `final String ns, key`, plus `String next(String current)` and
    `void apply(SettingsBackend s, String value)`.
- `Session.start(SettingsBackend s, Power p, Guard g, JournalStore store, List<Change> changes)`.
  The existing overloads pass `List.of(Session.STAY_ON)`. For each change:
  - `orig` is the recovered entry's `orig` **only if** the current value equals that entry's `set`.
    Otherwise `orig` is the current value.
  - `set = change.next(current)`.
  - The whole journal is saved and the guard armed before the first `apply`.
  - Changes are applied in list order, then wake, then panel off.
- `JournalStore(File file, Consumer<String> log, DirSync dirSync)`, with
  `interface DirSync { void sync(File dir) throws IOException; }`.
  - `device(log)` uses `android.system.Os.open(dir, O_RDONLY, 0)` + `Os.fsync` + `Os.close`.
  - `save` calls `dirSync.sync(parent)` after the rename.
- `Main --forget`:
  1. Loads the persisted journal. If there is one, it restores it as above.
  2. Deletes the journal, `/data/local/tmp/lantern-helper-*.jar` and
     `/data/local/tmp/lantern-companion-*.apk`.
  3. Prints `forgot journal=<restored|none>` and exits 0.
- Rust: `HELPER_PIDS_CMD` only matches `app_process / dev.lantern.helper.Main|Cleanup`. Step 6 fixes
  the exact regex. `start()` calls `wait_for_previous_helpers` **before** `prune_old_jars`.

- [ ] **Step 1: Write failing tests** (`JournalTest`, `SessionTest`, `JournalStoreTest`)
  - `journalV2RoundTrip()`: 3 entries, one with `orig == null`, parse back equal.
  - `parsesLegacyV1AsStayOnEntry()`: `{"ns":"global","key":"stay_on_while_plugged_in","orig":"15"}`
    gives one entry with `set = "7"`.
  - `restoreReversesOrder()`: the fake backend records `put B` before `put A`.
  - `leavesUserChangedValue()`: `orig = "15"`, `set = "7"`, current `"3"`. After restore the value is
    still `"3"`, and the log contains `changed since the session started`.
  - `restoresImeWithSelectIme()`: the `default_input_method` entry with current == set calls
    `selectIme(orig)` and does no plain `put` for that key.
  - `recoveredEntryTrustedOnlyWhileCurrentIsSet()`:
    - The recovered journal has `orig "15"`, `set "7"`, and the current value is `"7"`. The new
      journal has `orig "15"`.
    - The same journal with current `"0"` gives `orig "0"`.
  - `restoreAttemptsEveryEntryWhenOneThrows()`
  - `saveSyncsParentDirAfterRename()`: a recording `DirSync` sees the final file already present.
  - The existing `SessionTest`/`CleanupTest` cases still pass, migrated to `Journal`.
- [ ] **Step 2:** Run `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py helper`.
  Expected: FAIL (`Journal` missing).
- [ ] **Step 3:** Implement the Java side.
  - Extend the hand-written JSON parser in `Cleanup` (move it into `Journal`) to handle one level:
    an object holding `v` and an array of flat objects. It stays plain Java.
- [ ] **Step 4:** Run the build. Expected: PASS.
- [ ] **Step 5: Rust.** Change `HELPER_PIDS_CMD` and the `start()` order. Add the unit test
  `helper_pids_cmd_is_anchored`: the command contains `dev[.]lantern[.]helper[.](Main|Cleanup)` and
  does not match the string `dev.lantern.companion`. Run
  `cd host && export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace`. Expected: PASS.
- [ ] **Step 6: Device check** (Xiaomi).
  1. Start a session (`python3 tools/build.py install`; the phone is plugged in).
  2. Run `adb -s 20e9318 shell ps -A -o PID,ARGS | grep lantern` and confirm the regex matches both
     the Main and Cleanup lines exactly. Adjust the regex to the observed `ARGS` if needed, and
     record the line in the commit message.
  3. Check `pgrep` returns 2 pids during the session and 0 after `systemctl --user stop lanternd`.
  4. Run `lantern-helper --forget` by hand with no journal: it prints `forgot journal=none`.
- [ ] **Step 7:** Commit with
  `fix(helper): multi-entry journal restored only while unchanged; dir fsync; anchored pgrep; prune after wait`.

### Task 2: Protocol additions (both sides)

**Files:**
- Modify: `host/proto/src/lib.rs`, `host/proto/tests/golden.rs` (new golden files), `docs/protocol.md`
- Modify: `android/helper/src/dev/lantern/helper/Msg.java` (`TYPES`)
- Test: proto unit tests, `ProtoTest.java`

**Interfaces (Produces).** These are new `Control` variants. Fields marked optional are omitted when
absent.
- `App { pkg: String, user: u32, label: String, #[skip if false] work: bool, icon: String }`, where
  `icon` is base64 PNG or `""`. Sent helper→daemon on stream 0: one per launcher app.
- `AppsEnd { count: u32 }`, helper→daemon on stream 0: a full catalogue was just sent. `count` is
  the number of `app` messages in it.
- `AppRemoved { pkg: String, user: u32 }`, helper→daemon on stream 0.
- `Companion { state: CompanionState, #[opt] msg: Option<String> }`, helper→daemon on stream 0.
  `CompanionState` is `Ready | Unavailable` (snake_case).
- `Clip { text: String }` on stream 0, both directions.
- `Ime { op: ImeOp, text: String }` on window stream N, window→daemon→helper. `ImeOp` is
  `Commit | Compose | Finish`, and `Finish` carries `text: ""`.
- `Opened` gains `#[opt] label: Option<String>` and `#[skip if false] work: bool`.
- `StatusReport` gains `#[opt] companion: Option<CompanionState>`.
- Constants in `lantern_proto`: `pub const MAX_CLIP: usize = 1 << 20;`,
  `pub const MAX_IME_TEXT: usize = 1 << 16;`, `pub const MAX_ICON: usize = 1 << 20;`.
- Java: `Msg.TYPES` gains `app, apps_end, app_removed, companion, clip, ime`. The same constants
  go in `Proto.java`.

- [ ] **Step 1: Write failing tests.**
  - `opened_without_label_is_v1_bytes()`: `Opened{label:None, work:false, ..}` serializes exactly as
    before.
  - `ime_json_shape()`: produces `{"t":"ime","op":"compose","text":"việ"}`.
  - `clip_roundtrip_unicode()`
  - `app_json_shape()`: `work` is absent when false.
  - New golden files `control_ime.bin` and `control_app.bin`. `ProtoTest.goldenImeMatchesRust()`
    decodes `ime` and checks `op` and `text`.
- [ ] **Step 2:** Run `cargo test -p lantern-proto` and the helper build. Expected: FAIL.
- [ ] **Step 3:** Implement. Regenerate the goldens with `LANTERN_WRITE_GOLDEN=1`.
  - In `docs/protocol.md`, add every message above to the table.
  - Add a section **Companion link (helper ↔ companion)**, worded from Task 3's Interfaces.
  - Add a section **Text, clipboard, catalogue rules**: the size limits, and that the helper drops
    `ime` for a window that is not live.
- [ ] **Step 4:** Run the tests. Expected: PASS. The whole `cargo test --workspace` still passes: fix
  exhaustive `match`es on `Control` in `ipc.rs`/`window` with explicit arms that keep the current
  behaviour (a protocol error for a client, ignored from the helper).
- [ ] **Step 5:** Commit with `feat(proto): catalogue, companion, clipboard and IME messages (v1-compatible)`.

### Task 3: Companion APK, build pipeline, and the helper↔companion link

**Files:**
- Create: `android/companion/AndroidManifest.xml`, `res/xml/ime.xml`, `res/layout/test.xml`,
  `src/dev/lantern/companion/{LinkReceiver,Link,LanternIme,TestActivity}.java` (stubs for the
  IME/catalogue parts, filled in by Tasks 5 and 7)
- Create: `android/helper/src/dev/lantern/helper/CompanionLink.java`
- Modify: `tools/build.py` (target `companion`; `all` = helper + companion + cargo), `.gitignore`
  (`host/lantern/assets/companion.apk`), `Main.java`, `Router.java`
- Test: `CompanionLinkTest.java` (helper, plain Java over piped streams)

**Interfaces (Produces):**
- Manifest:
  - `<queries><intent>` for `MAIN`/`LAUNCHER`.
  - Receiver `.LinkReceiver`: `exported="true"`, `android:permission="android.permission.DUMP"`, so
    only the shell can send to it. Action `dev.lantern.companion.LINK`.
  - IME service `.LanternIme`: `BIND_INPUT_METHOD`, meta-data `@xml/ime` (as in
    `spikes/probe-app`).
  - `.TestActivity`: `android:enabled="false"`, launcher intent filter, label `Lantern test`, one
    `EditText`.
  - Application `allowBackup="false"`, label `Lantern`. No `WRITE_SECURE_SETTINGS`: the companion
    never writes settings.
- `tools/build.py companion`:
  1. aapt2 compile + link (`--min-sdk-version 35 --target-sdk-version 36 --version-code 1`).
  2. javac over `android/companion/src` **plus** the helper's `Proto.java`, `Msg.java`,
     `ControlCodec.java` and `Control.java` (shared sources, not copies).
  3. Host-JVM tests from `android/companion/test`.
  4. d8 `--min-api 35`, zip, zipalign, then apksigner. The key is
     `~/.local/share/lantern/companion-signing.p12`, created with keytool on first use (mode 0600,
     password `lantern-companion`).
  5. Output `build/companion/lantern-companion.apk`, copied to
     `host/lantern/assets/companion.apk`. Print `BUILT … sha256=…`.
- Link wire format: the same frames as `docs/protocol.md`, all on stream 0, with control JSON.
  1. The helper listens on abstract `lantern-c-<16 hex>` with a separate 32-hex link token.
  2. It sends `am broadcast --user 0 -n dev.lantern.companion/.LinkReceiver -a
     dev.lantern.companion.LINK --es socket <name> --es token <token>`.
  3. The companion connects and sends `hello{version:1, token}`.
  4. The helper answers `hello{version:1, token:"", prev_ime:<journal orig of default_input_method,
     or "">}`.
  5. Unauthenticated connections are closed. The helper keeps accepting until the deadline, and one
     authenticated companion at a time is allowed: a new authenticated one replaces the old.
- `final class CompanionLink implements Closeable` (plain Java):
  - `CompanionLink(Listener listener, Broadcaster broadcaster, String token, Consumer<Msg> toDaemon,
    Consumer<String> log)`, with `interface Listener { Conn accept() throws IOException; void
    close(); }` and `interface Conn extends Closeable { InputStream in(); OutputStream out(); }`. On
    the device these wrap a `LocalServerSocket`/`LocalSocket`; in tests, piped streams.
  - `interface Broadcaster { void link(String socket, String token) throws IOException; }`
  - `boolean connect(long timeoutMs)`: broadcast, then wait for an authenticated hello. False on
    timeout. The session goes on without the companion.
  - `void setPrevIme(String id)`
  - `boolean send(Msg m)`: false if not linked.
  - `boolean alive()`
  - `close()`
  - Inbound `app`, `apps_end` and `app_removed` go to `toDaemon` (Task 5 annotates them). Any other
    `t` is logged and dropped.
- `Router.setOnSessionUp(Runnable r)` runs right after the hello reply. `Main` uses it to send
  `companion{state}` to the daemon.
- `Router.setExtras(Extras e)`, with `interface Extras { void ime(String op, String text); void
  clip(String text); }`. The default is a no-op. Tasks 7 and 9 route `ime` and `clip` through it, and
  `Router.sendSession(Msg m)` (stream 0) is the way `Main` sends to the daemon.
- `Main.serve` order: valid hello, then `link.connect(3000)`, then `Session.start` (Task 7 adds the
  IME changes only when linked), then the hello reply, then `companion{state}`.
- `Link` (companion): one thread. It connects, does the hello, and then reads frames until EOF.
  `LinkReceiver.onReceive` calls `Link.start(context, socket, token)`, which replaces any running
  link.

- [ ] **Step 1: Write failing `CompanionLinkTest`**
  - `wrongTokenClosedRightTokenLinks()`: the first client sends a bad token and is closed, the
    second links, and `alive()` is true.
  - `connectTimesOutWithoutCompanion()`: returns false within timeout + 200 ms.
  - `relaysCatalogueDropsOthers()`: `app` reaches `toDaemon`. `clip` from the companion is dropped
    and logged.
  - `helloReplyCarriesPrevIme()`
- [ ] **Step 2:** Run the helper build. Expected: FAIL.
- [ ] **Step 3:** Implement `CompanionLink`, the companion `Link`/`LinkReceiver`, and the build
  target.
- [ ] **Step 4:** Run `python3 tools/build.py helper && python3 tools/build.py companion`. Expected:
  PASS. `apksigner verify` succeeds.
- [ ] **Step 5: Device check** (Xiaomi).
  1. `adb install -r build/companion/lantern-companion.apk`
  2. `python3 tools/build.py install`
  3. With the phone plugged in, `logcat -s lantern` shows `companion linked`.
  4. `dumpsys package dev.lantern.companion` lists the receiver with permission
     `android.permission.DUMP`.
- [ ] **Step 6:** Commit with `feat(companion): APK build, token-authenticated link to the helper`.

### Task 4: Daemon installs the companion; consent notice; `lantern undo-setup`

**Files:**
- Create: `host/lantern/src/daemon/companion.rs`, `host/lantern/src/notify.rs`,
  `host/lantern/src/undo.rs`
- Modify: `daemon/session.rs` (call `companion::ensure` before spawning the helper),
  `daemon/mod.rs`, `daemon/ipc.rs` (status `companion`; handle `Companion` from the helper),
  `main.rs` (subcommand), `tools/build.py` (`uninstall` runs `lantern undo-setup` first, best
  effort)
- Test: unit tests in `companion.rs`, `undo.rs`; `tests/cli.rs` (`undo-setup` refuses while a session
  is live); `tests/fake_adb.rs` extended for `pm path`/`pm install`

**Interfaces (Produces):**
- `pub static COMPANION_APK: &[u8] = include_bytes!("../../assets/companion.apk");`
- `pub enum Install { Current, Installed, Updated }`
- `pub async fn ensure(adb: &Adb) -> Result<Install>`:
  1. `pm path --user 0 dev.lantern.companion`, then `sha256sum <base.apk>`. If that equals the
     embedded SHA-256, the result is `Current`.
  2. Otherwise it pushes the APK to `/data/local/tmp/lantern-companion-<sha[:16]>.apk` and runs
     `pm install -r -d --user 0 <path>`.
  3. On `INSTALL_FAILED_UPDATE_INCOMPATIBLE` it runs `pm uninstall --user 0 dev.lantern.companion`
     and retries once.
  4. It always removes the pushed file.
- `pub fn record_setup(state_dir: &Path, serial: &str, i: Install) -> Result<bool>` writes
  `~/.local/state/lantern/devices/<serial>.json` = `{"companion":true,"since":<unix s>}`. It returns
  true when this is the first record for that serial.
- When `record_setup` returns true, the daemon calls
  `notify::send("Lantern set up your phone", "Installed the Lantern companion app (keyboard and app
  list) on <serial>. Run `lantern undo-setup` to remove it.", None)`.
- If `ensure` fails:
  - The session still starts, without the companion.
  - The daemon calls `notify::send` once per daemon run: `"Lantern couldn't install its companion
    app"` with the error's first line as the body.
  - The log carries the full error.
- `notify::send(summary: &str, body: &str, icon: Option<&Path>)` spawns
  `notify-send -a Lantern [-i icon] -- summary body`. It does not wait, and errors are only logged.
- `lantern undo-setup [--serial S]`:
  1. If `lantern status` reports state `ready` or `starting` for that serial, it exits 1 with
     `stop lanternd first: systemctl --user stop lanternd`.
  2. Otherwise it runs the helper `--forget` (pushing the embedded jar if it's missing) and
     `pm uninstall --user 0 dev.lantern.companion` (not installed is fine).
  3. It removes the catalogue entries for that serial (Task 6's `Catalog::remove_serial`; until
     Task 6 lands, a no-op hook) and the device record.
  4. It prints one line per step. The exit code is 0 only if every step succeeded.
- `StatusReport.companion` is set from the last `Companion` message of the live session, and is
  `None` otherwise.

- [ ] **Step 1: Write failing tests**
  - `ensure_current_when_sha_matches()`
  - `ensure_installs_when_missing()`
  - `ensure_reinstalls_on_incompatible_signature()`
  - `ensure_always_removes_pushed_apk()`: also on install failure.
  - `record_setup_first_time_only()`
  - `undo_setup_refuses_live_session()` (cli)
  - `status_reports_companion_state()` (ipc)
- [ ] **Step 2:** Run `cargo test --workspace`. Expected: FAIL.
- [ ] **Step 3:** Implement. `build.py all` must build the companion before cargo: `include_bytes!`
  needs the file. A fresh checkout already needs `build.py helper` first, and the README says so.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Device check.**
  1. `adb -s 20e9318 uninstall dev.lantern.companion`
  2. `python3 tools/build.py install`
  3. Expected: one KDE notification "Lantern set up your phone", and `lantern status` shows
     `"companion":"ready"`.
  4. Restart lanternd: there is no second notification, and the log says `companion current`.
- [ ] **Step 6:** Commit with `feat(daemon): install/update the companion, setup notice, lantern undo-setup`.

### Task 5: App catalogue on the phone; labels in `opened`

**Files:**
- Create/fill: `android/companion/src/dev/lantern/companion/AppCatalog.java`
- Test: `android/companion/test/.../AppCatalogTest.java`
- Modify: helper `CompanionLink.java` (annotation), `UserLaunch.java` + `DeviceUsers.java`
  (`managedProfile`), `DisplayHandle.java`/`AppDisplay.java` (`work()`), `Router.java` (label/work in
  `opened`), `Main.java`
- Test: `RouterTest.java`, `UserLaunchTest.java`, `CompanionLinkTest.java`

**Interfaces (Produces):**
- Companion `AppCatalog`:
  - **Full sync.** On link-up it sends one `app` for every `LauncherApps.getActivityList(null,
    profile)` entry, for each profile in `LauncherApps.getProfiles()`, then `apps_end{count}`.
    - Only the first launcher activity per package+user is sent.
    - `label = LauncherActivityInfo.getLabel()`.
    - `icon = getBadgedIcon(0)` rendered onto a 192×192 ARGB bitmap, PNG-compressed and base64'd.
      It is `""` if the PNG is over `MAX_ICON`.
  - **Live updates.** A `LauncherApps.Callback` sends `app` for added/changed packages and
    `app_removed` for removed ones, per profile `UserHandle`. Profiles that become available or
    unavailable trigger a full sync.
  - The `user` field is `UserHandle.getIdentifier()` (hidden in the SDK; read it reflectively, with
    a fallback of `handle.hashCode()`, which equals the id on AOSP).
  - Pure part for the test: `static String encodeIcon(byte[] png)`, which returns `""` above
    `MAX_ICON` and base64 NO_WRAP otherwise, and `static Msg appMsg(String pkg, int user, String
    label, String icon)`.
- `UserLaunch.Backend.managedProfile(int user) -> boolean`. The device implementation is
  `IUserManager.getUserInfo(user).isManagedProfile()`, and it returns false on any error.
- `CompanionLink` annotates each relayed `app` with `work:true` when `managedProfile(user)`. It
  also keeps `ConcurrentHashMap<String,String> labels` keyed `pkg + ":" + user`, which `app`
  updates and `app_removed` removes, and exposes `String label(String pkg, int user)`.
- `Router` takes a `Labels labels` (`interface Labels { String label(String pkg, int user); }`,
  default `(p, u) -> null`), set with `setLabels(Labels)`. `opened` gets `label` when it is non-null,
  and `work:true` when `DisplayHandle.work()`.

- [ ] **Step 1: Write failing tests**
  - `AppCatalogTest.iconOverLimitIsEmpty()`
  - `AppCatalogTest.appMsgFields()`
  - `CompanionLinkTest.annotatesWorkProfile()`: the fake backend says user 10 is managed, so the
    relayed `app` has `work=true`.
  - `CompanionLinkTest.labelsTrackAppAndRemoved()`
  - `RouterTest.openedCarriesLabelAndWork()`
  - `RouterTest.openedWithoutLabelHasNoLabelKey()`: v1 bytes.
- [ ] **Step 2:** Run the helper and companion builds. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Builds PASS.
- [ ] **Step 5: Device check** (both phones).
  1. With `LANTERN_LOG=debug`, the daemon log shows `apps_end count=N`, with N within ±2 of the
     launcher's app count.
  2. On the Motorola, the work-profile Files app arrives as `user=10 work=true` with a badged icon.
     Decode one icon to a file and look at it.
- [ ] **Step 6:** Commit with `feat(catalogue): companion lists launcher apps per profile; labels in opened`.

### Task 6: KDE menu entries, icons, window titles, "connect your phone"

**Files:**
- Create: `host/lantern/src/daemon/catalog.rs`
- Modify: `daemon/mod.rs`/`ipc.rs` (feed `App`/`AppsEnd`/`AppRemoved` to the catalogue),
  `window/identity.rs`, `window/mod.rs` (title on `opened`; notification on "phone not connected"),
  `undo.rs` (hook), `Cargo.toml` (`base64`)
- Test: unit tests in `catalog.rs` (temp dirs), `identity.rs`

**Interfaces (Produces):**
- `pub struct Dirs { pub applications: PathBuf, pub icons: PathBuf, pub menus: PathBuf, pub
  directories: PathBuf, pub exe: PathBuf }`, with `Dirs::from_env() -> Result<Dirs>`:
  - `applications`: `$XDG_DATA_HOME/applications`
  - `icons`: `$XDG_DATA_HOME/lantern/icons`
  - `menus`: `$XDG_CONFIG_HOME/menus/applications-merged`
  - `directories`: `$XDG_DATA_HOME/desktop-directories`
  - `exe`: `std::env::current_exe()`
- `pub struct AppEntry { pub pkg: String, pub user: u32, pub label: String, pub work: bool, pub
  icon_png: Vec<u8> }`
- `pub fn app_id(pkg: &str, user: u32) -> String`: the same scheme as `window::identity`. Move it
  to one shared function and use it in both places.
- `pub fn desktop_file(e: &AppEntry, serial: &str, exe: &Path, icon: Option<&Path>) -> String`:
  - `[Desktop Entry]`, `Type=Application`, `Version=1.5`.
  - `Name=<label>`, plus ` (work)` when `work`, or ` (user N)` when the user is not 0 and not work.
  - `Exec=<exe quoted> open [--user N] <pkg>`. The exe is double-quoted per the Desktop Entry spec
    Exec rules (escape `"`, `` ` ``, `$`, `\`), and `%` is doubled.
  - `Icon=<absolute png path>` (omitted without an icon).
  - `Categories=X-Lantern;`, `StartupWMClass=<app_id>`, `StartupNotify=true`,
    `X-Lantern-Serial=<serial>`, `X-Lantern-Package=<pkg>`, `X-Lantern-User=<n>`.
  - Label escaping: `\` becomes `\\`, newline `\n`, tab `\t`, CR `\r`. Other control characters
    are dropped, and the result is truncated to 200 chars on a char boundary. An empty label falls
    back to the package.
- `pub struct Catalog`:
  - `new(dirs: Dirs) -> Self`.
  - `begin_sync(&mut self, serial: &str)`.
  - `upsert(&mut self, e: &AppEntry) -> Result<()>`: rejects a pkg outside `[A-Za-z0-9_.]` or
    empty with `Err`. It writes the icon `<icons>/<app_id>.png` and the desktop file atomically
    (temp + rename), and only when the bytes changed.
  - `remove(&mut self, pkg: &str, user: u32) -> Result<()>`
  - `end_sync(&mut self, count: u32) -> Result<usize>`:
    - If `count` equals the number of upserts since `begin_sync`, it removes every Lantern entry
      (identified by `X-Lantern-Serial=`) whose serial differs or that was not upserted.
    - Otherwise it removes nothing and logs `partial catalogue (got X of Y); not pruning`.
    - It returns the number removed.
  - `remove_serial(&mut self, serial: &str) -> Result<usize>`
  - After any change it schedules `kbuildsycoca6` once, 1 s after the last change.
- `Catalog::new` calls `ensure_menu(&dirs)`, which writes `lantern.menu` (a `<Menu><Name>Applications</Name><Menu><Name>Phone
  </Name><Directory>lantern-phone.directory</Directory><Include><Category>X-Lantern</Category>
  </Include></Menu></Menu>`) and `lantern-phone.directory` (`Name=Phone`,
  `Icon=smartphone`), once.
- A file without `X-Lantern-Serial` is never modified or deleted.
- `Identity::title(&self, label: Option<&str>, work: bool) -> String` returns the label (or pkg),
  plus ` (work)` when `work`, otherwise ` (user N)` when `user != 0`. The window calls
  `set_title` on `opened`.
- When `lantern open` gets `error{"phone not connected"}`, it calls `notify::send("Connect your
  phone", "<Name from the .desktop file if present, else pkg> opens when your phone is connected
  by USB.", icon)`, using the icon PNG if it exists. The exit code stays 1.

- [ ] **Step 1: Write failing tests**
  - `desktop_file_escapes_label()`: label `"A\nB=C;%x \"q\" 🙂"` + 10 kB gives a single `Name=` line,
    no raw newline, ≤ 200 chars, and `Exec` contains no label text.
  - `desktop_file_work_and_user_suffix()`
  - `exec_quotes_exe_with_space()`: exe `/home/a b/lantern`.
  - `rejects_bad_package()`: `"com.x;rm -rf"` gives `Err`, and no file is written.
  - `full_sync_prunes_uninstalled()`
  - `full_sync_prunes_other_serial()`
  - `partial_sync_prunes_nothing()`
  - `never_touches_foreign_desktop_files()`: a hand-made `lantern.foo.desktop` without
    `X-Lantern-Serial` survives `end_sync` and `remove_serial`.
  - `upsert_skips_unchanged_bytes()`: the mtime is unchanged on the second upsert.
  - `title_uses_label_and_suffixes()`
- [ ] **Step 2:** Run `cargo test --workspace`. Expected: FAIL.
- [ ] **Step 3:** Implement. Decode `App.icon` with `base64::engine::general_purpose::STANDARD`. A
  bad base64 string or bytes that don't start with the PNG signature mean no icon.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Device check.**
  1. With the phone plugged in, after ≤ 10 s the KDE launcher's Phone submenu lists the phone's apps
     with icons. KRunner finds "Settings".
  2. Open it from the menu: the taskbar entry shows the Settings icon and the title `Settings`.
  3. With `systemctl --user stop lanternd`, launch an entry: a "Connect your phone" notification
     appears.
- [ ] **Step 6:** Commit with `feat(menu): KDE entries, icons and window titles that follow the phone`.

### Task 7: Lantern keyboard on the phone (session switch, watchdog, routing)

**Files:**
- Create: `android/helper/src/dev/lantern/helper/ImeSetup.java`;
  `android/companion/src/dev/lantern/companion/ImeWatchdog.java`; fill in `LanternIme.java`
- Modify: helper `Main.java` (IME changes when linked; `prev_ime`), `Router.java` (`ime` on stream N),
  `AppDisplay.java` (IME policy if the device check requires it)
- Test: `ImeSetupTest.java`, `RouterTest.java`, `android/companion/test/.../ImeWatchdogTest.java`

**Interfaces (Produces):**
- `ImeSetup`:
  - `static final String ID = "dev.lantern.companion/.LanternIme"`
  - `static String withEnabled(String list, String id)`. The list is `:`-separated entries of the
    form `component[;subtype...]`. It returns `id` when the list is null or empty. It appends
    `":" + id` when no entry's component equals `id`, and otherwise returns the list unchanged.
  - `static List<Change> changes()` returns
    `[setting("secure","enabled_input_methods", l -> withEnabled(l, ID)), ime(ID)]`. These go
    after `Session.STAY_ON`.
- `Main`: when `link.connect` is true, it calls `Session.start(..., [STAY_ON] + ImeSetup.changes())`,
  then `link.setPrevIme(journal.find("secure","default_input_method").orig)`. When false, it uses
  `[STAY_ON]` only and sends `companion{state:"unavailable", msg:"companion did not connect"}`.
- `Router`: `ime{op,text}` on a live window stream calls `extras.ime(op, text)`. On the device that
  is `link.send(Msg.of("ime").put("op",op).put("text",text))`. An unknown `op` or text over
  `MAX_IME_TEXT` is a `ProtocolError`. The daemon screens these first (Task 8).
- `LanternIme extends InputMethodService`:
  - `onEvaluateInputViewShown()` and `onEvaluateFullscreenMode()` return false.
  - `apply(op, text)` works on `getCurrentInputConnection()`:
    - `commit` calls `commitText(text, 1)`.
    - `compose` calls `setComposingText(text, 1)`. An empty `text` gives `setComposingText("", 1)`.
    - `finish` calls `finishComposingText()`.
    - It logs only `ime op=<op> ok=<bool> len=<n>`.
  - Hardware key events pass through (no `onKeyDown` override).
- `ImeWatchdog` (pure):
  - `ImeWatchdog(long graceMs = 5000)`.
  - `void linkUp()`, `void linkLost(long nowMs, String prevIme)`.
  - `String due(long nowMs, boolean weAreCurrent)` returns `prevIme` only when the link is down,
    `nowMs - lostAt >= graceMs`, `weAreCurrent` is true, and `prevIme` is non-empty. It fires at
    most once per loss.
  - `LanternIme` polls it every 500 ms on its handler. When it returns a value, `LanternIme` calls
    `switchInputMethod(prev)` and logs `ime_restored`.
- **IME display policy:** keep `HIDE` on Lantern displays if the Step 5 check passes. If commits do
  not reach the editor under `HIDE`, switch Lantern displays to `LOCAL` (0) and record why in
  `docs/spikes/RESULTS.md`. The Lantern IME draws nothing, so neither policy shows a keyboard.

- [ ] **Step 1: Write failing tests**
  - `ImeSetupTest.addsOnce()`, `keepsSubtypes()`, `nullOrEmptyList()`, `alreadyPresentUnchanged()`
  - `RouterTest.imeOnLiveWindowReachesExtras()`
  - `RouterTest.imeForUnknownWindowIgnored()`
  - `ImeWatchdogTest`:
    - `firesAfterGrace()`
    - `notBeforeGrace()`
    - `linkUpCancels()`: a new session within 5 s must not lose its keyboard.
    - `notWhenAnotherImeCurrent()`
    - `firesOnce()`
- [ ] **Step 2:** Run the builds. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Builds PASS.
- [ ] **Step 5: Device check** (Xiaomi). Text delivery and the display IME policy are checked in
  Task 8 Step 5, once the window can send `ime`.
  1. Start a session. `settings get secure default_input_method` returns the Lantern IME, and
     `dumpsys input_method` shows it bound (on Android 17 the field is `imeToken=… [component]`).
  2. `systemctl --user stop lanternd`: the default and bound IME are back to the original within 5 s.
  3. `kill -9` the helper's Main pid during a session: Cleanup restores both within 5 s.
- [ ] **Step 6:** Commit with `feat(ime): Lantern keyboard selected per session, restored by helper, Cleanup and watchdog`.

### Task 8: PC text input (winit IME, text vs keys)

**Files:**
- Modify: `host/lantern/src/window/{mod,input,keymap}.rs`, `daemon/ipc.rs`
  (`client_frame_for_helper`)
- Test: unit tests in `input.rs`, `keymap.rs`; `tests/ipc.rs`

**Interfaces (Produces):**
- `keymap::us_char(code: KeyCode, shift: bool) -> Option<char>`: the character a US layout gives
  for letters, digits, space and the US punctuation keys. These are what the Android key events
  produce.
- `pub enum Typed { Key, Text(String), Nothing }` and
  `pub fn classify(code: KeyCode, text: Option<&str>, mods: ModifiersState, state: ElementState)
  -> Typed`:
  - Ctrl, Alt or Super held: `Key`.
  - Press with `text` equal to `us_char(code, shift)`: `Key`.
  - Press with a non-empty `text` of no control characters that differs from `us_char`: `Text`.
  - Anything else: `Key`.
  - `InputState` records which keys were sent as `Text`, and their release is `Nothing`, so no
    unmatched key-up is ever sent.
- Window:
  - After creating the window, it calls `set_ime_allowed(true)` and `set_ime_cursor_area` (the
    bottom-left 1/4 of the window).
  - `WindowEvent::Ime`:
    - `Preedit(s, _)` sends `Ime{op:Compose, text:s}`.
    - `Commit(s)` sends `Ime{op:Commit, text:s}`.
    - `Disabled` sends `Ime{op:Finish}` if a preedit is non-empty.
  - Text longer than `MAX_IME_TEXT` is split into commits of at most that size, on char boundaries.
- `client_frame_for_helper` forwards `Control::Ime` only when `text.len() <= MAX_IME_TEXT`. Otherwise
  it is a protocol error for that client.
- `LANTERN_DEBUG_INPUT` gains `text:<s>`, `compose:<s>` and `finish`. `key:` accepts `Ctrl+<Letter>`
  (adds `META_CTRL_ON|META_CTRL_LEFT_ON` and a Ctrl key down/up around it).

- [ ] **Step 1: Write failing tests**
  - `us_char_letters_digits_punct()`
  - `classify_ascii_is_key()`
  - `classify_dead_key_e_acute_is_text()`: `text "é"` on `KeyE` gives `Text`.
  - `classify_ctrl_c_is_key()`
  - `text_press_has_no_release()`
  - `long_commit_splits_on_char_boundary()`
  - `ipc_rejects_oversize_ime()`
  - `ipc_forwards_ime()`
- [ ] **Step 2:** Run `cargo test --workspace`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Device check.**
  1. Enable TestActivity, then run `LANTERN_DEBUG_INPUT="tap:0.5,0.15;text:Tiếng Việt 👍;compose:việ;finish"
     lantern open dev.lantern.companion`.
  2. `logcat -s LanternTest` shows the EditText text equal to `Tiếng Việt 👍việ`. If nothing
     arrives, apply Task 7's IME display policy fallback (`LOCAL`) and re-run.
  3. Type `abc` on the real keyboard; it arrives as keys.
  4. If ibus or fcitx5 is configured on this PC, also try a real Vietnamese composition. Otherwise
     write in the commit body that the PC has no input method running.
- [ ] **Step 6:** Commit with `feat(window): PC input method and non-US text through the Lantern keyboard`.

### Task 9: Text clipboard, both directions

**Files:**
- Create: helper `ClipSync.java`, `PhoneClipboard.java`; host `src/clipboard/mod.rs`,
  `src/clipboard/wayland.rs`
- Modify: helper `Router.java` (`clip` on stream 0 → `extras.clip`), `Main.java`; host
  `daemon/mod.rs`, `daemon/ipc.rs` (`on_helper` stream 0 `Clip`), `lib.rs`
- Test: `ClipSyncTest.java`; unit tests in `clipboard/mod.rs`

**Interfaces (Produces):**
- Java `ClipSync` (pure), `ClipSync(Consumer<String> toHost, Consumer<String> writePhone)`:
  - `void phoneChanged(String text, boolean sensitive)` sends to the host unless the text is null or
    empty, `sensitive` is set, it is over `MAX_CLIP` bytes, or it equals `lastFromHost`.
  - `void fromHost(String text)` sets `lastFromHost = text`, then `writePhone`.
- `PhoneClipboard` (device): uses the vendored `ClipboardManager.addPrimaryClipChangedListener` on
  the helper's main looper thread. It reads `getPrimaryClip()` item 0 as text, and `sensitive` is the
  description extra `android.content.extra.IS_SENSITIVE`. It is started in the `onSessionUp` hook
  and removed at teardown.
- Rust `clipboard::ClipSync` (pure):
  - `fn pc_changed(&mut self, offer: &PcOffer) -> Option<String>`, where
    `PcOffer { mimes: Vec<String>, text: Option<String>, secret: bool }`. It returns the text to send
    unless the text is `None`/empty, `secret` is set, it is over `MAX_CLIP`, or it equals
    `last_from_phone`.
  - `fn from_phone(&mut self, text: String) -> String` sets `last_from_phone` and returns the text
    to put on the PC clipboard.
- `clipboard::wayland::spawn(tx: tokio::sync::mpsc::Sender<PcOffer>) -> ClipHandle`. It is a
  dedicated thread with its own `wayland-client` connection that binds `ext_data_control_manager_v1`
  and gets a device for the first seat.
  - **On `selection`:**
    - The text mime is the first present of `text/plain;charset=utf-8`, `UTF8_STRING`,
      `text/plain`.
    - `secret` = the offer has `x-kde-passwordManagerHint` and reading it gives `secret`.
    - Pipes are read with a 1 s timeout and a `MAX_CLIP + 1` byte cap.
  - **`ClipHandle::set(text: String)`:** creates a source offering the three text mimes, writes the
    text on `send`, and calls `set_selection`.
  - **Primary selection:** ignored.
  - **Compositor errors:** if `WAYLAND_DISPLAY` is unset or the global is missing, it logs one
    warning and retries every 10 s.
- Daemon: on a PC offer while a session is live, if `pc_changed` returns text, it sends
  `Clip{text}` on stream 0. On a helper `Clip`, it calls `from_phone` and then `ClipHandle::set`.
  - Nothing is pushed at session start; only changes sync.

- [ ] **Step 1: Write failing tests**
  - `ClipSyncTest.noEchoOfHostText()`, `skipsSensitive()`, `skipsOversize()`, `skipsEmpty()`
  - Rust:
    - `clip_sync_no_echo()`
    - `clip_sync_skips_kde_secret()`
    - `clip_sync_skips_oversize()`
    - `clip_sync_prefers_utf8_mime()`: a pure `pick_mime(&[String]) -> Option<&str>`.
- [ ] **Step 2:** Run the builds and `cargo test --workspace`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Expected: PASS.
- [ ] **Step 5: Device check.**
  1. Run `qdbus6 org.kde.klipper /klipper setClipboardContents "lantern-pc-$RANDOM"`.
  2. In the TestActivity window, debug `key:Ctrl+V`: the `LanternTest` log shows that text.
  3. Then debug `key:Ctrl+A;key:Ctrl+C` after `text:from-phone-123`:
     `qdbus6 org.kde.klipper /klipper getClipboardContents` returns `from-phone-123` within 2 s.
  4. Copy a password from KeePassXC (or set the hint with a test client): it does not reach the
     phone.
- [ ] **Step 6:** Commit with `feat(clipboard): text clipboard sync both ways with loop and secret suppression`.

### Task 10: Acceptance, install, docs

**Files:**
- Modify: `tools/device_test.py`, `README.md`, `docs/HANDOFF.md`, `docs/protocol.md` (final read-through)

- [ ] **Step 1: Extend `tools/device_test.py`.** Items 1–8 are unchanged, except that `restored()`
  also requires `default_input_method` and `enabled_input_methods` equal to their pre-session
  values. Snapshot both values with the stay-on baseline. New items:
  9. `companion_linked`: `lantern status` shows `"companion":"ready"` within 10 s of `ready`.
  10. `catalogue_tracks_phone`:
      1. `lantern.com.android.settings.desktop` exists with `Name=Settings` and a readable PNG
         `Icon`.
      2. `pm enable dev.lantern.companion/.TestActivity` makes `lantern.dev.lantern.companion.desktop`
         appear within 5 s.
      3. `pm disable` removes it within 5 s.
  11. `title_is_label`: the KWin script reads the Settings window's caption as `Settings`.
  12. `ime_unicode_text`: Task 8 Step 5, automated, with the `LanternTest` log equal to
      `Tiếng Việt 👍việ`.
  13. `clipboard_both_ways`: Task 9 Step 5 items 1–3, automated.
  14. `usb_yank_restores_keyboard` (`--manual`, with Sahaj). Unplug for 10 s: while unplugged, the
      `ime_restored` logcat line is recorded. After replug, before the new session switches it
      again, `default_input_method` equals the original. Stop lanternd before replugging so the
      check reads the phone alone.
- [ ] **Step 2:** Run `python3 tools/build.py install && python3 tools/device_test.py`. Expected:
  13/13 automatic PASS. Run `--manual --only 8 14` with Sahaj.
- [ ] **Step 3:** Run the Motorola work-profile check. Run `lantern open --user 10
  com.google.android.apps.nbu.files` from the menu entry "Files (work)". Expected: title
  `Files (work)` and the badged icon in the taskbar. Typing into its search field works.
- [ ] **Step 4: Docs.**
  - **README:** the companion app, what is changed per session (stay-awake and keyboard), the menu
    entries, and `lantern undo-setup`.
  - **HANDOFF:** Plan 2 done, and what is still unverified.
  - **protocol.md:** final read-through.
- [ ] **Step 5:** Commit with `feat: Plan 2 acceptance (menu, titles, text input, clipboard), docs`.
