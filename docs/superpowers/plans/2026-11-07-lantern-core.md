# Lantern Core (Plan 1 of 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Running `lantern open <package>` on the PC opens that phone app as its own native KDE window.
The window gets live video, mouse, touch and scroll input, and key input. Resizing the window reflows
the app; closing the window quits the app. Sessions start and stop cleanly when the phone is plugged
in or unplugged.

**Architecture:**
- **Phone:** a shell-uid Java helper (`app_process`) gives each app its own virtual display and
  hardware encoder. It talks to `lanternd` over one `adb forward` socket using a framed, multiplexed
  protocol.
- **`lanternd`** (Rust) owns the phone session and serves window processes over a Unix socket.
- **`lantern open`** (Rust) is the per-app window process. Its Wayland `app_id` is per-package. It
  decodes HEVC/H.264 (VA-API, with software fallback), renders with wgpu, and sends input back.

**Tech Stack:**
- Phone: Java 11 source level, built with javac/d8 from `~/.local/opt/pd-toolchain`, using vendored
  scrcpy 5.0.1 hidden-API wrappers.
- PC: Rust (cargo 1.99) with tokio, winit 0.30 (Wayland), wgpu, ffmpeg-next/ffmpeg-sys-next (FFmpeg 8,
  VA-API), serde_json, and anyhow/thiserror.

**Spec:** `docs/superpowers/specs/2026-11-07-lantern-design.md` (binding: §7, §8.2–8.4, §9b). Spike evidence:
`docs/spikes/RESULTS.md`.

**Out of scope here (Plans 2–3):**
- `.desktop` entries and icons
- companion APK and hidden IME
- clipboard
- notifications
- drag-and-drop
- audio
- gamepads
- tray

Pausing encoders for minimised windows (spec §9) is also in Plan 3. Plan 1 types keys only as
Android key events.

## Global Constraints

- Device: Xiaomi 11T Pro `20e9318`, **Android 17 / SDK 37**, no root, only USB debugging. Compile against the
  SDK 36 `android.jar` already in the toolchain. `--min-api 35` for d8.
- PC: KDE Plasma 6.6 Wayland, Intel UHD (iHD VA-API driver). Linux only. One phone at a time.
- Virtual display flags (exact): `PUBLIC | PRESENTATION | OWN_CONTENT_ONLY | SUPPORTS_TOUCH |
  ROTATES_WITH_CONTENT | DESTROY_CONTENT_ON_REMOVAL | TRUSTED | OWN_DISPLAY_GROUP | ALWAYS_UNLOCKED |
  TOUCH_FEEDBACK_DISABLED | OWN_FOCUS | DEVICE_DISPLAY_GROUP`. **Never** `SHOULD_SHOW_SYSTEM_DECORATIONS`.
  The values are in `docs/spikes/RESULTS.md` / `spikes/probe-shell/.../VdBench.java`.
- Display IME policy `HIDE` (2) on every Lantern display.
- Close = quit: `DESTROY_CONTENT_ON_REMOVAL` plus an explicit task removal.
- Session start wakes the phone (no unlock), turns the physical panel off and keeps the phone awake.
  Session end, or any helper death, restores the panel and `stay_on_while_plugged_in` exactly; an
  originally absent key is deleted, never written as `null`.
- Helper jar push path is content-addressed: `/data/local/tmp/lantern-helper-<sha256[:16]>.jar`. Never
  overwrite a jar a running process may load (this crashed a spike probe).
- Vendored scrcpy code lives in `android/helper/vendor/` with headers intact plus `UPSTREAM` (scrcpy
  v5.0.1, commit a60891ae). Apache-2.0 `LICENSE` at the repo root.
- Wayland `app_id` = `lantern.<package>` (user 0). Plan 2 adds `.desktop` files with that exact name.
- Protocol version 1. A version mismatch is a hard, explained error, never a silent fallback.
- Every adb call targets the selected serial explicitly (`-s`).

## Review Focus

1. **USB yanked mid-stream.** Windows show "Phone disconnected" and then close. On the phone the helper
   restores panel power and stay-awake within 5 s and leaves no virtual display behind. Pinned by the
   Task 3 teardown test and the Task 9 acceptance step.
2. **The app exits by itself** (Back out of its root activity, or a crash). Its window closes, and the
   display and encoder are released. Pinned by Task 4's `taskVanished` test and Task 9.
3. **Opening an app that's already open** focuses the existing window; no second display is created.
   Pinned by Task 6's registry test.
4. **Rapid resize drag.** Resizes are debounced, the final size always applies, and the encoder never
   errors. Pinned by Task 8's debouncer test and Task 4's resize test.
5. **`lanternd` killed while windows are open.** Window processes exit within 2 s with an error dialog
   on stderr. The helper sees the socket EOF and tears down, and no stranded displays or panel-off
   state remain. Pinned by Task 7's socket-EOF test and Task 9.

---

## File Structure

```
LICENSE                              Apache-2.0
README.md                            build/run (short)
tools/build.py                       builds helper jar (javac+d8), then cargo build
tools/device_test.py                 Task 9 acceptance driver
docs/protocol.md                     wire protocol v1 (Task 1)
android/helper/src/dev/lantern/helper/
  Main.java                          entry: args, server socket, lifecycle
  Proto.java                         frame read/write (mirrors Rust)
  Session.java                       wake/panel/stay-awake journal + restore
  Cleanup.java                       separate-process restorer (helper-death safety)
  AppDisplay.java                    one virtual display + encoder + task for one window
  Encoder.java                       MediaCodec surface encoder, reconfigure/recreate
  Input.java                         pointer/scroll/key injection, focus
  Router.java                        demux control/input to AppDisplays, mux video out
android/helper/test/dev/lantern/helper/*Test.java   plain-java tests (no Android runtime)
android/helper/vendor/               scrcpy 5.0.1 wrappers + FakeContext/Workarounds/... + UPSTREAM
host/Cargo.toml                      workspace
host/proto/src/lib.rs                frames, control messages, input encoding (shared)
host/lantern/src/main.rs             CLI: `daemon`, `open <pkg>`, `status`
host/lantern/src/adb.rs              adb subprocess wrapper, device tracking
host/lantern/src/daemon/{mod,session,registry,ipc}.rs
host/lantern/src/window/{mod,decode,render,input,keymap,geometry,debounce}.rs
host/lantern/assets/helper.jar       build output, embedded via include_bytes! (gitignored)
packaging/lanternd.service           systemd --user unit
```

---

### Task 1: Wire protocol v1, Rust side

**Files:**
- Create: `docs/protocol.md`, `host/Cargo.toml`, `host/proto/Cargo.toml`, `host/proto/src/lib.rs`
- Test: `host/proto/src/lib.rs` (`#[cfg(test)]`), `host/proto/tests/golden.rs`, `host/proto/golden/*.bin`

**Interfaces (Produces):**
- `pub struct Frame { pub kind: Kind, pub stream: u32, pub payload: Vec<u8> }`
- `pub enum Kind { Control = 1, Video = 2, Input = 3 }`
- `pub fn write_frame(w: &mut impl Write, f: &Frame) -> io::Result<()>`
- `pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>>`: returns `None` on clean EOF at a
  frame boundary.
- Async variants `write_frame_async` / `read_frame_async` over tokio `AsyncRead`/`AsyncWrite`.
- `pub const MAX_PAYLOAD: u32 = 8 << 20;`
- `#[serde(tag = "t", rename_all = "snake_case")] pub enum Control` with these variants:
  - `Hello { version: u32, token: String }`
  - `Open { pkg: String, user: u32, w: u32, h: u32, dpi: u32 }`
  - `Opened { display: i32, task: i32, codec: String, w: u32, h: u32 }`
  - `Resize { w: u32, h: u32, dpi: u32 }`
  - `Resized { w: u32, h: u32 }`
  - `Focus`
  - `Close`
  - `Closed { reason: CloseReason }`, where `CloseReason` is `Closed | AppExited | Error`
  - `Error { msg: String }`
  - `Bye`
- `pub struct VideoPacket { pub pts_us: u64, pub config: bool, pub key: bool, pub data: Vec<u8> }`, with
  `encode()` and `decode(&[u8])`.
- `pub enum InputEvent` with these variants, plus `encode()` and `decode()`:
  - `Pointer { action: PointerAction, id: u64, x: f32, y: f32, buttons: u32, pressure: f32 }`
  - `Scroll { x: f32, y: f32, h: f32, v: f32 }`
  - `Key { action: KeyAction, keycode: u32, meta: u32, repeat: u32 }`

  Here x and y are normalized to 0..1 of the display.

Wire format (copy verbatim into `docs/protocol.md`):
- All integers are big-endian.
- Frame: `u32 len` (bytes after this field), `u8 kind`, `u32 stream`, `payload[len-5]`.
- Control payload: UTF-8 JSON. Stream `0` is the session (Hello/Bye/Error). Stream `N>0` is the window id
  assigned by `lanternd`.
- Video payload: `u64 pts_us`, `u8 flags` (bit0 config, bit1 key), then the codec bytes (Annex-B).
- Input payload: `u8 type` (1 pointer, 2 scroll, 3 key), then:
  - pointer: `u8 action` (0 down, 1 up, 2 move, 3 hover-move, 4 cancel), `u64 id`, `f32 x`, `f32 y`,
    `u32 buttons`, `f32 pressure`
  - scroll: `f32 x`, `f32 y`, `f32 h`, `f32 v`
  - key: `u8 action` (0 down, 1 up), `u32 keycode` (Android), `u32 meta`, `u32 repeat`

- [ ] **Step 1: Write failing tests**
  - `frame_roundtrip`: writes 3 frames of different kinds into a `Vec`, reads them back equal, and the
    next read is `Ok(None)`.
  - `truncated_frame_is_error`: header claims 10 bytes, only 3 are given, so the result is `Err` with
    `UnexpectedEof`.
  - `oversize_rejected`: len = `MAX_PAYLOAD + 6`, so the result is `Err(InvalidData)`.
  - `control_json_shape`: `Control::Open{pkg:"com.android.settings",user:0,w:1280,h:800,dpi:160}`
    serializes to `{"t":"open","pkg":"com.android.settings","user":0,"w":1280,"h":800,"dpi":160}`.
  - `input_roundtrip` for all three types.
  - `video_flags_roundtrip`.
- [ ] **Step 2:** Run `cd host && cargo test -p lantern-proto`. Expected: FAIL (not defined).
- [ ] **Step 3:** Implement `lib.rs`.
- [ ] **Step 4:** Write `host/proto/tests/golden.rs`. It writes `golden/{control_open,input_pointer,
  input_key,video_key}.bin` if `LANTERN_WRITE_GOLDEN=1`, and otherwise asserts the encoding equals the
  file. Generate the files once and commit them. Task 2's Java tests read the same files.
- [ ] **Step 5:** Run `cargo test -p lantern-proto`. Expected: PASS.
- [ ] **Step 6:** Commit with `feat(proto): wire protocol v1 with golden vectors`.

### Task 2: Helper build, vendored scrcpy, Java protocol

**Files:**
- Create: `tools/build.py`, `android/helper/vendor/**`, `android/helper/vendor/UPSTREAM`, `LICENSE`,
  `android/helper/src/dev/lantern/helper/Proto.java`, `.../Main.java` (version and ping only)
- Test: `android/helper/test/dev/lantern/helper/ProtoTest.java`

**Interfaces (Produces):**
- `final class Proto`: `static Frame read(DataInputStream in)`, which returns `null` on clean EOF and
  throws `EOFException` mid-frame; and `static void write(DataOutputStream out, int kind, int stream,
  byte[] payload)`, which is `synchronized` on `out`.
- `record`-style `final class Frame { final int kind, stream; final byte[] payload; }`.
- `static InputEvent decodeInput(byte[])`, mirroring Rust.
- Control JSON uses `org.json` (platform).
- `tools/build.py helper`:
  - Builds `build/helper/lantern-helper.jar` (dex) and copies it to `host/lantern/assets/helper.jar`.
  - Runs the Java tests on the host JVM first; they use only `java.*`, and Android classes are not
    loaded by the tests.
- `tools/build.py all` runs `helper` and then `cargo build --release`.

- [ ] **Step 1:** Vendor scrcpy 5.0.1 (`git clone --depth 1 --branch v5.0.1
  https://github.com/Genymobile/scrcpy` into the scratch directory).
  - Copy `server/src/main/java/com/genymobile/scrcpy/{FakeContext,Workarounds,AndroidVersions}.java`,
    `wrappers/*.java`, `util/Ln.java`, `util/LogUtils.java` if referenced, `display/DisplayInfo.java`,
    and the `server/src/main/java/android/**` fake stubs and `aidl/**` that these need.
  - Add whatever else `javac` reports missing until it compiles, and nothing more.
  - Keep the package names. Write `UPSTREAM` with the tag, the commit, and the list of copied paths.
- [ ] **Step 2: Write failing `ProtoTest`**
  - `goldenControlOpenMatchesRust()`: decodes `host/proto/golden/control_open.bin` and checks that the
    JSON `t`, `pkg` and `w` match.
  - `goldenInputPointerMatchesRust()`
  - `goldenVideoKeyMatchesRust()`
  - `writeThenReadRoundTrip()`
  - `cleanEofReturnsNull()`
  - `midFrameEofThrows()`
- [ ] **Step 3:** Run `python3 tools/build.py helper`. Expected: test FAIL (`Proto` missing).
- [ ] **Step 4:** Implement `Proto.java`, plus `Main.java` with `--version`, which prints
  `lantern-helper 1 sdk=<SDK_INT>`.
- [ ] **Step 5:** Run `python3 tools/build.py helper`. Then run `adb -s 20e9318 push build/helper/lantern-helper.jar
  /data/local/tmp/lantern-helper-test.jar && adb -s 20e9318 shell CLASSPATH=/data/local/tmp/lantern-helper-test.jar
  app_process / dev.lantern.helper.Main --version`. Expected: tests pass, then `lantern-helper 1 sdk=37`.
  Remove the test jar afterwards.
- [ ] **Step 6:** Commit with `feat(helper): build pipeline, vendored scrcpy 5.0.1 wrappers, protocol`.

### Task 3: Helper session: wake, panel-off, stay-awake, guaranteed restore

**Files:**
- Create: `Session.java`, `Cleanup.java`
- Modify: `Main.java`
- Test: `SessionTest.java` (journal logic with a fake settings backend)

**Interfaces (Produces):**
- `interface SettingsBackend { String get(String ns, String key); void put(String ns, String key, String v); void delete(String ns, String key); }`.
  The real implementation uses the vendored scrcpy settings wrapper, or `settings` via
  `Runtime.exec` if the wrapper is missing.
- `final class Session implements AutoCloseable`:
  - `static Session start(SettingsBackend s, Power p)`: journals the original
    `global/stay_on_while_plugged_in` (or "absent"), writes `7`, wakes the phone and turns panel 0 off.
  - `void close()`: idempotent, restores in reverse order and turns the panel on.
- `interface Power { void wake(); void setPanel(boolean on); }`.
  - The real implementation wakes with `KEYCODE_WAKEUP` (224), injected through the vendored
    InputManager.
  - The panel uses the vendored `DisplayManager.requestDisplayPower(0, on)` on API ≥ 35.
- `Cleanup`: `Main` starts it as a second `app_process` (`dev.lantern.helper.Cleanup <journal-json>`)
  with a pipe to the parent. When the pipe reaches EOF (the parent died) it applies the journal, then
  exits. A clean parent close first writes `done\n`, so Cleanup exits without acting.

- [ ] **Step 1: Write failing tests**
  - `restoresExistingValue()`: original "15", then start and close, and the backend holds "15".
  - `deletesOriginallyAbsentKey()`: never writes the string "null".
  - `closeIsIdempotent()`
  - `panelTurnedOnAtClose()`: the fake Power records `wake, panel(false), panel(true)` in order.
  - `journalJsonRoundTrip()`: Cleanup parses what Session wrote.
- [ ] **Step 2:** Run `python3 tools/build.py helper`. Expected: FAIL.
- [ ] **Step 3:** Implement `Session`, `Cleanup`, the real backends, and `Main --session-test <seconds>`,
  which starts a session, sleeps, and closes.
- [ ] **Step 4: Device check.**
  - Run `--session-test 5`.
  - During those 5 s, `dumpsys power` shows `mWakefulness=Awake`, and
    `settings get global stay_on_while_plugged_in` returns `7`.
  - After it ends, the value is back to the original.
  - Then run `--session-test 60` and `kill -9` the helper pid.
  - Expected: within 5 s the setting is restored by `Cleanup`, which `logcat -s lantern` reports.
- [ ] **Step 5:** Commit with `feat(helper): session power/stay-awake with crash-safe restore`.

### Task 4: Helper app displays: open, stream, resize, close, app-exit

**Files:**
- Create: `AppDisplay.java`, `Encoder.java`
- Test: `EncoderTest.java` (pure size/bitrate policy only)

**Interfaces (Consumes):** `Proto`, `Session`. **(Produces):**
- `final class AppDisplay implements AutoCloseable`:
  - `static AppDisplay open(int win, String pkg, int user, int w, int h, int dpi, VideoSink sink)`.
    It creates the display with the Global-Constraints flags, sets IME policy HIDE, starts the
    encoder, and launches the app with `ActivityOptions.setLaunchDisplayId`. Its task id is read back
    from `getTasks` filtered by displayId within 3 s.
  - `void resize(int w, int h, int dpi)`
  - `int displayId()`, `int taskId()`
  - `boolean taskVanished()`: true when no task remains on its display.
  - `void close()`: removes every task on the display (`IActivityTaskManager.removeTask`) and then
    releases the display and the encoder. Idempotent.
- `interface VideoSink { void packet(int win, long ptsUs, boolean config, boolean key, ByteBuffer data); }`
- `final class Encoder`: `Encoder(String mime, int w, int h)`, `Surface surface()`,
  `void reconfigure(int w, int h)`, `void drainTo(VideoSink, int win)` (blocking loop, run on a
  per-display thread), `void close()`.
  - Policy: HEVC (`c2.qti.hevc.encoder`) if present, otherwise AVC. Bitrate = `clamp(w*h*fps*0.1, 4,
    20) Mbps`. I-frame interval 10 s, plus `PARAMETER_KEY_REQUEST_SYNC_FRAME` on resize, on focus, and
    on new subscriber.
  - Low latency: `KEY_LATENCY=0` / `KEY_LOW_LATENCY=1` when supported. Max B-frames 0. Repeat the
    previous frame after 100 ms (`KEY_REPEAT_PREVIOUS_FRAME_AFTER`) so static apps still emit frames.
  - Width and height are aligned down to multiples of 16. The display keeps the exact size, and the
    encoder size is aligned.
- Resize strategy (spec §9b):
  1. First try `codec.stop(); codec.configure(newFormat); new input surface; display.setSurface(...)`
     on the **same** `MediaCodec`.
  2. If that throws, fall back to release and recreate.
  3. Log `resize_ms` and which path was used.
- Orientation (spec §9a, the app wins):
  - A `DisplayManager.DisplayListener` on the display detects when the app rotates it (the display
    has `ROTATES_WITH_CONTENT`).
  - When the rotation changes, the helper reconfigures the encoder to the rotated size and sends
    `Resized{w,h}` with the swapped dimensions.

- [ ] **Step 1: Write failing `EncoderTest`**
  - `bitrateClamps()`: 1280×800@60 gives 6,144,000. 4000×4000 gives 20 Mbps. 320×200 gives 4 Mbps.
  - `alignsTo16()`: 1281×801 gives 1280×800.
  - `prefersHevcWhenListed()`: given a codec name list.
- [ ] **Step 2:** Run the build. Expected: FAIL.
- [ ] **Step 3:** Implement `Encoder` and `AppDisplay`. Add `Main --display-test <pkg>`, which opens the
  app at 1280×800/160 and writes packets to `/data/local/tmp/lantern-display-test.h265` for 8 s. It
  then resizes to 1000×1000, 800×1280 and 1280×800 (2 s each), then closes.
- [ ] **Step 4: Device check.**
  - Run `--display-test com.android.settings` and pull the file.
  - `ffprobe -count_frames` reports ≥ 300 frames, and the frame size changes appear in
    `ffprobe -show_frames -show_entries frame=width,height`.
  - After close, `cmd display get-displays` shows no Lantern display, and `dumpsys activity activities`
    shows no Settings task on a display other than 0.
  - Record `resize_ms` for both paths in `docs/spikes/RESULTS.md` § S6 follow-up.
- [ ] **Step 5: `taskVanished` device check.** Run `--display-test org.lineageos.jelly` with an injected
  BACK at 3 s; Back on the browser's root closes it. Expected: the log prints `app_exited` within 1 s
  of the task disappearing (poll every 250 ms).
- [ ] **Step 6:** Commit with `feat(helper): per-app virtual display, encoder, resize, close=quit`.

### Task 5: Helper input, focus and the server loop

**Files:**
- Create: `Input.java`, `Router.java`
- Modify: `Main.java`
- Test: `InputTest.java` (coordinate scaling, action mapping), `RouterTest.java` (with fakes)

**Interfaces (Consumes):** `Proto`, `AppDisplay`, `Session`. **(Produces):**
- `final class Input`:
  - `static MotionEvent toMotion(InputEvent.Pointer p, int w, int h, long downTime)`: x = p.x·w.
  - `void inject(int displayId, InputEvent e)`: injects asynchronously (mode 0).
    - Mouse buttons map to `SOURCE_MOUSE` with `setActionButton`. Touch (id ≥ 1 and buttons == 0)
      maps to `SOURCE_TOUCHSCREEN`.
    - Keys use `SOURCE_KEYBOARD` with the event's display id set to the target display.
  - `void focus(int taskId)`: calls `IActivityTaskManager.setFocusedTask`.
- `final class Router`:
  - `Router(DataInputStream in, DataOutputStream out, AppDisplayFactory f)`.
  - `void run()`: loops until EOF, a `Bye`, or an error.
    1. Requires `Hello{version:1, token}` matching the token from argv. Otherwise it sends `Error` and
       exits.
    2. Then handles Open, Resize, Focus, Close and Input per window id.
  - It polls `taskVanished` every 250 ms and sends `Closed{AppExited}`.
  - On exit, it closes all `AppDisplay`s and then the `Session`.
- `Main` (no flags): `dev.lantern.helper.Main serve <socket-name> <token>`.
  1. Listens on a `LocalServerSocket(<socket-name>)` and accepts exactly one client.
  2. Starts `Cleanup`, then `Session`, then runs `Router`.
  3. On return, closes everything and exits 0.
  - The token is 32 random hex characters from `lanternd`.
  - The socket name is `lantern-<token[:8]>`, so a stale helper never collides.

- [ ] **Step 1: Write failing tests**
  - `InputTest.scalesNormalizedToDisplayPixels()`: (0.5, 0.25) on 1280×800 gives (640, 200).
  - `InputTest.mouseVsTouchSource()`
  - `RouterTest.rejectsWrongToken()`: the reply is `Error` and `run()` returns.
  - `RouterTest.openThenCloseCallsFactoryAndClose()`
  - `RouterTest.eofClosesAllDisplays()`: Review Focus 5.
  - `RouterTest.appExitSendsClosedAppExited()`
- [ ] **Step 2:** Run the build. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the build. Expected: PASS.
- [ ] **Step 5:** Commit with `feat(helper): input injection, focus, authenticated server loop`.

### Task 6: `lanternd`: device watch, deploy, session, registry

**Files:**
- Create: `host/lantern/Cargo.toml`, `src/main.rs`, `src/adb.rs`,
  `src/daemon/{mod,session,registry}.rs`, `packaging/lanternd.service`
- Test: unit tests in `registry.rs`, `adb.rs` (parsing); `tests/fake_helper.rs` (an in-process fake
  helper on a TCP socket)

**Interfaces (Consumes):** `lantern-proto`. **(Produces):**
- `adb.rs`:
  - `pub struct Adb { serial: String }` with `shell(&[&str]) -> Result<String>`,
    `push(local, remote)`, `forward(local_tcp: u16, abstract_name) -> Result<()>`,
    `remove_forward(u16)`, and `spawn_shell(cmd) -> tokio::process::Child`.
  - `pub fn track_devices() -> impl Stream<Item = DeviceEvent>`, which parses `adb track-devices`.
  - `DeviceEvent::{Online(serial), Offline(serial)}`.
- `session.rs`:
  - `pub async fn start(adb: &Adb) -> Result<SessionConn>`:
    1. Pushes the embedded `helper.jar` to the content-addressed path if it's missing (checked with
       `sha256sum`).
    2. Generates a token and spawns `app_process ... Main serve`.
    3. Forwards a random local port and connects, retrying for up to 3 s.
    4. Sends `Hello` and waits for the helper's `Hello` echo.
  - `SessionConn { tx: mpsc::Sender<Frame>, rx: mpsc::Receiver<Frame> }`.
- `registry.rs`: `pub struct Registry`:
  - `fn claim(&mut self, pkg_user: &str) -> Claim` returns `Claim::New(win_id)` or
    `Claim::Existing(win_id)`.
  - `fn release(win_id)`.
  - `fn route(win_id) -> Option<ClientHandle>`.
  - Window ids start at 1 and increase monotonically, never reused within a session.
- `lantern daemon`:
  1. Runs a `track_devices` loop, starting a session for serial `20e9318`, or for the only device if
     `LANTERN_SERIAL` is unset.
  2. Tears down on `Offline`.
  3. Logs to the journal (stderr).
- `packaging/lanternd.service`: `ExecStart=%h/.local/bin/lantern daemon`, `Restart=on-failure`.

- [ ] **Step 1: Write failing tests**
  - `registry_second_claim_is_existing()`: Review Focus 3.
  - `registry_ids_monotonic_after_release()`
  - `parse_track_devices_lines()`: the `adb track-devices` hex-length-prefixed format
    (`0018 20e9318\tdevice\n`).
  - `session_handshake_against_fake_helper()`: the fake replies with Hello. A wrong version gives an
    `Err` whose message contains "version".
- [ ] **Step 2:** Run `cargo test -p lantern`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p lantern`. Expected: PASS.
- [ ] **Step 5: Device check.** Run `LANTERN_LOG=debug lantern daemon` with the phone attached.
  Expected log: `session ready serial=20e9318 helper=<sha>`. The phone panel goes off. Ctrl-C restores
  the panel and settings, verified by reading `stay_on_while_plugged_in` back.
- [ ] **Step 6:** Commit with `feat(daemon): device tracking, helper deploy, session handshake, registry`.

### Task 7: Daemon ↔ window IPC

**Files:**
- Create: `src/daemon/ipc.rs`
- Modify: `src/daemon/mod.rs`
- Test: `tests/ipc.rs`

**Interfaces (Produces):**
- The socket is `$XDG_RUNTIME_DIR/lantern/daemon.sock`, in a directory with mode 0700 and created with
  the umask set.
- The client's first frame is `Control::Open` on stream 0.
  - The daemon answers `Opened` (new) on the assigned stream, or `Control::Focus` with stream =
    existing id (already open). In the second case the client must request activation of the existing
    window and then exit 0.
  - After that, the client sends `Resize`, `Focus`, `Close` and `Input` on its stream, and receives
    `Video`, `Resized` and `Closed` on its stream.
- The daemon rewrites nothing except stream ids, which are already equal to `win_id` end to end.
- Single-instance focusing:
  - The existing window process receives `Focus` from the daemon. It calls winit `focus_window()`,
    using the XDG activation token passed by the second `lantern open` via its
    `XDG_ACTIVATION_TOKEN` environment variable.
  - The token travels as an extra `Open` field, `activation_token: Option<String>`. **Extend the proto
    enum here** with `#[serde(default)]` and update the golden test.
- When the daemon socket closes, the window exits.

- [ ] **Step 1: Write failing tests** (in-process daemon with a fake session):
  - `second_open_same_pkg_gets_focus_not_opened()`
  - `client_disconnect_sends_close_to_helper()`: an abrupt client death means close = quit.
  - `daemon_drop_eofs_clients()`: Review Focus 5. The client's read returns `None` within 2 s.
  - `socket_dir_is_0700()`
- [ ] **Step 2:** Run `cargo test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit with `feat(daemon): window IPC with single-instance focus`.

### Task 8: Window process: Wayland window, decode, render, input

**Files:**
- Create: `src/window/{mod,decode,render,input,keymap,geometry,debounce}.rs`
- Test: unit tests in `keymap.rs`, `geometry.rs`, `debounce.rs`; `tests/decode.rs` (decodes a fixture)

**Interfaces (Consumes):** the IPC from Task 7. **(Produces):**
- `geometry.rs`:
  - `fn video_rect(window: (u32,u32), video: (u32,u32)) -> Rect`: aspect-fit and centered, for the
    letterbox period while a resize is in flight.
  - `fn to_normalized(rect, px, py) -> Option<(f32,f32)>`: `None` outside the video.
  - `fn display_request(logical: (u32,u32), scale: f64) -> (w, h, dpi)`: w and h are the physical
    pixels (logical·scale) aligned down to 16, and dpi = round(160·scale), with a minimum of 120.
- `debounce.rs`:
  - `struct Debouncer { quiet: Duration }` with `fn push(&mut self, now, size)` and
    `fn due(&mut self, now) -> Option<size>`.
  - It emits the latest size once the size has been stable for `quiet`, which is 120 ms. It also emits
    at most every 500 ms during a continuous drag.
- `keymap.rs`: `fn android_keycode(k: winit::keyboard::KeyCode) -> Option<u32>`. It covers letters,
  digits, F1–F12, arrows, Enter=66, Escape=111, Backspace=67, Tab=61, Space=62, Delete=112, Home=122,
  End=123, PageUp=92, PageDown=93, and modifiers. `fn meta_state(mods) -> u32`, using Android
  META_* bits.
- Window-level key mapping: **Escape → Android BACK (4)**. Super+Escape is passed through as ESCAPE.
- `decode.rs`: `struct Decoder` with `fn new(codec: &str) -> Result<Self>`, which tries VA-API
  (`/dev/dri/renderD128`) and falls back to software. `fn push(&mut self, pkt: &VideoPacket) ->
  Result<Vec<Nv12Frame>>`. `Nv12Frame { w, h, y: Vec<u8>, uv: Vec<u8>, y_stride, uv_stride }`.
  - It uses ffmpeg-sys-next for the hwdevice and `av_hwframe_transfer_data`.
  - It logs `decoder=vaapi|software` once.
- `render.rs`: wgpu surface. NV12 is uploaded to two textures and converted to RGB in a WGSL shader
  (BT.709 limited range). The pipeline is `PresentMode::Mailbox` if available, else `Fifo`, and frames
  are drawn on arrival.
- `mod.rs`:
  - The winit window uses `WindowAttributesExtWayland::with_name("lantern.<pkg>", "")`, with the title
    set to the package name for now (Plan 2 supplies labels).
  - The initial logical size is 1280×800, or the size remembered per package in
    `~/.local/state/lantern/windows.json`.
  - On `CloseRequested`, it sends `Close`, waits for `Closed` for up to 1 s, then exits.
  - On `Closed{AppExited}`, it exits.
  - On daemon EOF, it draws "Phone disconnected" for 1.5 s, then exits.
  - On a focus gain, it sends `Focus`.
  - On a `Resized` whose aspect differs from the window's by more than 2% and that was not requested
    by this window (an app rotation), it calls `request_inner_size` with the long edge kept. If KWin
    refuses (maximised or tiled), the video stays letterboxed via `video_rect`.

- [ ] **Step 1: Write failing tests**
  - `geometry_letterbox_wide_video_in_tall_window()`
  - `geometry_outside_returns_none()`
  - `display_request_scale_1_25()`: logical 1280×800 at 1.25 gives (1600, 1000, 200).
  - `rotation_swaps_window_keeping_long_edge()`: window 1280×800 with a `Resized` of 800×1280 requests
    a 500×800 logical size.
  - `debounce_emits_final_size_after_quiet()`
  - `debounce_emits_during_long_drag_every_500ms()`: Review Focus 4.
  - `keymap_letters_and_specials()`: KeyA=29, Enter=66, Escape handled as BACK at the window level.
  - `decode_fixture_software()`: decodes `tests/fixtures/settings.h265`, the 2 s capture saved from
    Task 4 Step 4, into ≥ 1 frame of 1280×800.
- [ ] **Step 2:** Run `cargo test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS, including the decode fixture test.
- [ ] **Step 5: Manual smoke.**
  - With `lantern daemon` running, run `lantern open com.android.settings`.
  - Expected: a KDE window shows live Settings, clicking and scrolling work, and the taskbar shows
    its own entry.
  - The log shows `decoder=vaapi`.
  - The window's `app_id` is checked in Task 9 through a KWin script.
- [ ] **Step 6:** Commit with `feat(window): per-app Wayland window with VA-API decode and input`.

### Task 9: Install, acceptance on device, docs

**Files:**
- Create: `tools/device_test.py`, `README.md`
- Modify: `tools/build.py` (adds an `install` target that copies `lantern` to `~/.local/bin` and enables
  `lanternd.service` with `systemctl --user`)

- [ ] **Step 1:** Write `tools/device_test.py`. It runs these checks in sequence and prints PASS/FAIL per
  line. It uses `lantern status` (add the subcommand: it prints the session state and the open windows
  as JSON) and adb readbacks.
  1. `session_starts_on_plug`: the daemon reports `ready` within 10 s of `adb wait-for-device`.
  2. `open_settings_creates_one_display`: `lantern open com.android.settings` exits after opening, and
     `cmd display get-displays` has exactly one Lantern display.
  3. `second_open_focuses`: running `open` again leaves the count at 1.
  4. `resize_reflows`: uses the KWin scripting D-Bus to resize the window to 900×900. Within 1 s the
     helper reports `Resized{w:896,h:896}` (aligned to 16).
  5. `close_quits`: closing the window via the KWin script leaves no Lantern display and no Settings
     task off display 0.
  6. `app_exit_closes_window`: opens Jelly, sends BACK twice, and the window process exits.
  7. `daemon_kill_restores_phone`: `systemctl --user kill lanternd` leaves the panel on and the setting
     restored within 5 s.
  8. `usb_yank` (manual prompt): asks Sahaj to unplug for 10 s and replug. The phone is restored, and
     the daemon starts a fresh session.
- [ ] **Step 2:** Run `python3 tools/build.py install && python3 tools/device_test.py`. Expected: 8/8
  PASS. Item 8 needs Sahaj.
- [ ] **Step 3:** Write `README.md` covering build, install, `lantern open <pkg>`, what it changes on
  the phone, and how to undo.
- [ ] **Step 4:** Commit with `feat: install target, device acceptance suite, README`.

## Deferred to Plan 3

- **Latency harness and target.** The spec §11 latency measurement harness (on-screen timestamp
  app vs PC capture, compared against scrcpy 5.0.1) and the spec §9a target, glass-to-glass no
  more than 5 ms worse than scrcpy on the same phone, cable and settings, are Plan 3 work. Plan 1
  does not measure latency, so the target is unverified until then.
