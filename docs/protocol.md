# Lantern wire protocol v1

Spoken between the host (`lanternd`, Rust: `host/proto`, crate `lantern-proto`) and the phone-side
server (Java) over a single byte stream.

## Wire format

- All integers are big-endian.
- Frame: `u32 len` (bytes after this field), `u8 kind`, `u32 stream`, `payload[len-5]`.
- Control payload: UTF-8 JSON. Stream `0` is the session (hello/bye/error, and the session-wide
  catalogue, `companion` and `clip` messages). Stream `N>0` is the window id assigned by `lanternd`.
- Video payload: `u64 pts_us`, `u8 flags` (bit0 config, bit1 key), then the codec bytes (Annex-B).
- Input payload: `u8 type` (1 pointer, 2 scroll, 3 key), then:
  - pointer: `u8 action` (0 down, 1 up, 2 move, 3 hover-move, 4 cancel), `u64 id`, `f32 x`, `f32 y`,
    `u32 buttons`, `f32 pressure`
  - scroll: `f32 x`, `f32 y`, `f32 h`, `f32 v`
  - key: `u8 action` (0 down, 1 up), `u32 keycode` (Android), `u32 meta`, `u32 repeat`

## Details

- Frame kinds: `1` Control, `2` Video, `3` Input. Any other kind is a protocol error.
- Max payload is `8 << 20` bytes (8 MiB), so `len` must be in `5 ..= MAX_PAYLOAD + 5`; anything else
  is rejected as invalid data before the payload is read.
- EOF exactly at a frame boundary is a clean end of stream. EOF inside a frame (including inside the
  4-byte `len`) is an error.
- Pointer and scroll `x`/`y` are normalized to `0..1` of the video, i.e. of the part of the display the
  encoder captures: its top-left `align16(w)×align16(h)` (sizes aligned down to 16). The helper scales
  by that size, not the display's. `f32` is IEEE-754 binary32.
- Input payloads must be exactly the size of their type; trailing bytes are an error.

## Control messages

JSON objects tagged by `"t"` (snake_case). Example:
`{"t":"open","pkg":"com.android.settings","user":0,"w":1280,"h":800,"dpi":160}`

| `t` | fields |
|---|---|
| `hello` | `version: u32`, `token: string` |
| `open` | `pkg: string`, `user: u32` (Android user/profile id, `0..=i32::MAX`), `w: u32`, `h: u32`, `dpi: u32`, optional `activation_token: string` |
| `opened` | `display: i32`, `task: i32`, `codec: string`, `w: u32`, `h: u32`, optional `moved: bool` (absent = false), optional `label: string` (the app's launcher label), optional `work: bool` (absent = false; the app runs in a work profile) |
| `resize` | `w: u32`, `h: u32`, `dpi: u32` |
| `resized` | `w: u32`, `h: u32` |
| `focus` | optional `activation_token: string` |
| `close` | none |
| `closed` | `reason`: one of `"closed"`, `"app_exited"`, `"error"` |
| `error` | `msg: string`, optional `code: string`, optional `version: u32` |
| `bye` | none |
| `app` | `pkg: string`, `user: u32`, `label: string`, optional `work: bool` (absent = false), `icon: string` (base64 PNG, or `""`). Helper → daemon, stream 0 |
| `apps_end` | `count: u32` (the number of `app` messages in the full catalogue just sent). Helper → daemon, stream 0 |
| `app_removed` | `pkg: string`, `user: u32`. Helper → daemon, stream 0 |
| `companion` | `state`: one of `"ready"`, `"unavailable"`; optional `msg: string`. Helper → daemon, stream 0 |
| `clip` | `text: string`. Stream 0, both directions |
| `ime` | `op`: one of `"commit"`, `"compose"`, `"finish"`; `text: string` (`""` for `finish`). Window stream `N`, window → daemon → helper |

## Session rules (phone helper)

- The first frame must be `hello` on stream 0 with `version: 1` and the helper's token. The helper
  replies `hello{version:1, token:""}`; on a wrong token (or anything else first) it sends one
  `error{msg}` on stream 0 and closes the connection. On a wrong version it sends
  `error{msg, code:"version", version:<the helper's own version>}` instead (the helper never
  answers `hello` with another version). Phone state (stay-awake, keyboard, panel) only changes
  after a valid hello.
- `lanternd` treats a handshake `error` with `code:"version"` (or, from a helper predating
  `code`, a `msg` starting `protocol version mismatch`) as a version mismatch and exits 3 (see
  Exit statuses); any other handshake `error` is retried with backoff.
- `opened` is sent before any video for that window. `codec` is `"h265"` or `"h264"`.
  `moved:true` means the task on the display existed before the launch (the app was already open
  on the phone or in its recents, and Android moved that task onto the Lantern display: LauncherApps
  cannot request a new task, and singleTask apps reuse theirs). Closing the window quits it. The
  helper only sends `moved` when true, so an ordinary `opened` is the v1 bytes; the client prints
  one line about it on stderr. Before launching, the helper records the app's tasks for that user
  on every display; if no task reaches the Lantern display within 3 s, it removes any task of that
  app and user that was not in the record (wherever it landed, e.g. the phone's own screen), never
  one that existed before, and fails the open.
- `open.user`: the helper resolves `pkg`'s launcher activity in that user's package manager and
  starts it as that user (`am start --user N` semantics). For a user with the
  `no_debugging_features` restriction (Android bars the shell uid from such users) that is a profile
  of the current user (work profile, private space, clone), it resolves and starts the activity
  through LauncherApps instead (`ILauncherApps.getLauncherActivities` / `startActivityAsUser`, the
  launcher's profile-group-checked path). Before creating a display it refuses, as a per-window
  failure, a user that does not exist, is not running or not unlocked, bars adb and is not such a
  profile (or LauncherApps refuses it), and a `pkg` that is not installed for that user or has no
  launcher activity. If the helper's hidden user/package checks themselves fail on user 0, it falls
  back to `getLaunchIntentForPackage`. The `error.msg` is meant for the person at the PC (e.g.
  `open com.x failed: com.x is not installed for Android user 10`).
- `resized{w,h}` answers `resize`, and is also sent unprompted when the app rotates its display.
- A per-window failure (open, resize or the video stream) is `error{msg}` followed by
  `closed{reason:"error"}` on that window's stream; the session continues.
- Frames for an unknown or already-closed window are ignored. A protocol error (bad JSON, unknown
  `t`, bad input payload, video from the host) ends the session with `error{msg}` on stream 0.
- `bye` (or EOF) ends the session: the helper closes every window without sending `closed`.

`code` and `version` on `error` are omitted when absent, so a plain error is still
`{"t":"error","msg":"..."}`. The only `code` defined in v1 is `"version"`.

Pause/resume (Plan 3) will be added within protocol v1, without a version bump: the helper jar is
embedded in and deployed by `lanternd`, so both sides always ship together.

`label` and `work` on `opened`, `work` on `app`, and `msg` on `companion` are omitted when absent
(`work` when false), so an `opened` without them is still the v1 bytes.

## Text, clipboard, catalogue rules

- Size limits, in UTF-8 bytes of the JSON string value (before escaping), the same constants on
  both sides (`lantern_proto::MAX_*`, `Proto.MAX_*`):
  - `clip.text`: at most `MAX_CLIP = 1 << 20` (1 MiB). Larger clipboard text is not sent.
  - `ime.text`: at most `MAX_IME_TEXT = 1 << 16` (64 KiB). Longer input is split into several
    `commit`s on character boundaries by the window. The daemon does not forward a larger `ime`
    (a protocol error for that client); the helper treats one as a protocol error.
  - `app.icon`: the PNG is at most `MAX_ICON = 1 << 20` bytes (1 MiB) before base64; a larger
    icon is sent as `""`.
- `ime` ops: `commit` inserts `text` at the cursor (replacing any composing text); `compose` sets
  the composing (preedit) text to `text` (`""` clears it); `finish` ends composition, keeping the
  composing text, and carries `text: ""`.
- `ime` is only meaningful for a live window: the helper drops (logs and ignores) an `ime` for a
  window that is unknown, closing or already closed, like any other window frame. An unknown `op`
  or a `text` over `MAX_IME_TEXT` is a protocol error (checked before the window lookup).
  `ime` on stream 0 is a protocol error.
- Catalogue: once the session is up and the companion is linked, the helper relays a full
  catalogue (one `app` per launcher app per profile, then `apps_end{count}`), then live `app`
  (added or changed) and `app_removed` messages. A full catalogue whose `apps_end.count` matches
  the `app` messages received replaces the previous one (apps missing from it are removed); a
  partial one (the link dropped mid-list, or the count differs) removes nothing.
- `companion{state}` is sent after the hello reply: `"ready"` when the companion linked and the
  Lantern keyboard is selected, `"unavailable"` (with a `msg`) otherwise: `"companion did not
  connect"`, or `"keyboard switch failed: <reason>"` when the companion linked but selecting its
  keyboard failed (the catalogue still syncs; only PC text input is missing). Without the
  companion the session still works, with no catalogue and no PC text input.
- `clip` carries plain text only, on stream 0, and only changes sync: nothing is pushed when a
  session starts. Each side suppresses only the echo of the text it last received from the
  other side (setting a clipboard fires its own change notification, and Klipper may re-own the
  PC selection with the same text); it never remembers its own sent text, so a re-copy is always
  sent. Any other local change, synced or not (sensitive or secret, over `MAX_CLIP`, no text or
  empty text), clears the received text, so copying it again later is sent. Android fires its
  listener twice per copy: the helper skips a change whose `ClipDescription.getTimestamp()`
  equals the last handled one. Neither side sends empty
  text or text over `MAX_CLIP`; a `clip` from lanternd over `MAX_CLIP` is a protocol error on
  the helper (checked before anything else). `clip` on a window stream is a protocol error.
- Secrets stay put: the helper never sends a phone clip whose description has the extra
  `android.content.extra.IS_SENSITIVE = true`; lanternd never sends a PC selection that offers
  `x-kde-passwordManagerHint` with content `secret` (or whose hint can't be read).
- lanternd reads the PC clipboard (the regular selection only, never the primary selection)
  through `ext_data_control_v1` on its own thread and connection, preferring the mime types
  `text/plain;charset=utf-8`, `UTF8_STRING`, `text/plain` in that order, and offers all three
  when it sets the selection. Each read has a 1 s timeout and a `MAX_CLIP + 1` byte cap. The
  helper listens for phone clipboard changes on its own `lantern-clip` Looper thread, from the
  session-up hook until teardown. Neither side ever logs clipboard text, only its length.

## Companion link (helper ↔ companion)

The helper talks to the companion app (`dev.lantern.companion`, shipped inside `lantern` with the
helper jar) over its own connection, never through lanternd. It uses the same frames as above,
all on stream 0, with control JSON.

The companion cannot dial the helper: SELinux denies an app `connectto` on the shell's Unix
sockets, and the phone's network policy can block the companion's loopback TCP (the background
firewall chain while it is cached; on Xiaomi also the restricted-networking chain, whatever its
state). So the helper hands the connection over instead:

1. The helper makes a 32-hex link token (not the session token) and two pipes.
2. It acquires the companion's provider `content://dev.lantern.companion.link` as an external
   client and calls `call("link", <token>, {in, out})`: `in` is the read end of the
   helper→companion pipe, `out` the write end of the companion→helper pipe. The provider requires
   `android.permission.DUMP` and refuses any caller but the shell (or root); it answers
   `{ok:true}`, or `{ok:false}` for a malformed token or missing pipes. The helper holds the
   provider until the session ends, which keeps the companion's process un-frozen
   (`ext-provider`). If the provider is missing (companion not installed) the link fails at once.
   Before the call the helper checks the provider's owner: its package must be
   `dev.lantern.companion`, its uid must be that package's uid in user 0, and the package must
   have exactly one current signer whose certificate SHA-256 equals the pin `tools/build.py`
   generates into the helper from the companion signing key. On any mismatch the helper releases
   the provider, logs `companion link refused: <reason>` and treats the companion as unavailable,
   so an app squatting the authority never sees the token or the pipes.
3. The companion sends `hello{version:1, token}` on its pipe.
4. The helper answers `hello{version:1, token:""}`.
5. A connection whose first frame is not a `hello` with the right version and token within 2 s is
   closed. One authenticated companion is linked at a time: a new authenticated one replaces the
   old.

The helper waits up to 3 s for the companion's hello before it starts the session (twice on the
keyboard-less fallback path, about 9 s in all), so lanternd allows 20 s for the hello reply.

The companion's `LinkReceiver` (also DUMP-only) takes one test-only command, `am broadcast --user
0 -n dev.lantern.companion/.LinkReceiver -a dev.lantern.companion.TEST_ACTIVITY --es enabled
true|false`, which enables its `TestActivity` or returns it to disabled. The shell may not
`pm enable` another package's component (only a testOnly package's), so device tests use this.
`TestActivity` is one EditText that logs `text=<content>` with tag `LanternTest` on every change;
it is disabled outside tests. On every new link the companion returns it to disabled if it was
left enabled (a test run killed before its cleanup); no device test spans a relink.

Messages on the link:

| direction | `t` | fields |
|---|---|---|
| companion → helper | `app`, `apps_end`, `app_removed` | as above; the helper relays them to lanternd on stream 0 |
| helper → companion | `ime` | `op`, `text`, as above (the helper forwards a window's `ime`), plus the link-only `display: u32`, the Android display of the window it was typed in |
| helper → companion | `ime_prev` | `id: string`: the keyboard to restore if the link is lost (the session journal's original `default_input_method`, or `""`) |
| helper → companion | `sync` | none: send the full app catalogue now |

`ime_prev` and `sync` are link-only: they never travel between the helper and lanternd. So is
`ime.display`: lanternd's `ime` (v1) has no `display`; the helper adds it, from the window's
stream, only on the link. The
helper sends `ime_prev` once the session's journal exists, and again to a newly linked companion.
Any other `t` from the companion is logged and dropped.

Keyboard on the link:

- When the companion linked, the session's journaled changes are `stay_on_while_plugged_in`,
  then `secure/enabled_input_methods` with `dev.lantern.companion/.LanternIme` appended (unless
  an entry's component already is it; subtypes kept), then `default_input_method` set to it
  (`ime set`). They are restored in reverse at the end, by Cleanup if the helper dies, each only
  while it still holds Lantern's value. Without the companion only `stay_on_while_plugged_in`
  changes. `ime_prev.id` is the journal's original `default_input_method`, or `""` when it was
  absent or is Lantern's own.
- The keyboard is optional: if the session start with the keyboard changes fails (e.g. `ime set`
  refused) and its own rollback succeeded (every setting back, that attempt's Cleanup disarmed),
  the helper starts the session again with `stay_on_while_plugged_in` only, under a fresh Cleanup,
  sends no `ime_prev`, keeps the companion link (catalogue), and reports
  `companion{state:"unavailable", msg:"keyboard switch failed: <reason>"}`. If that rollback
  itself failed, the start fails as before (the phone may be mid-restore; Cleanup and the journal
  own it).
- The helper forwards a live window's `ime` in order from its own sender thread, so a companion
  that stops reading never stalls the session; at most 256 wait, newer ones are dropped. An `ime`
  that arrives while no companion is linked is dropped. Each dropping stretch is logged once,
  never per frame, and never with the text. Writes to a companion never hold the link's lock, so
  a jammed companion cannot block the link's close or a new link.
- The companion's `LanternIme` draws nothing (no input view, never fullscreen) and lets hardware
  keys pass. `commit` is `commitText(text, 1)`, `compose` is `setComposingText(text, 1)`,
  `finish` is `finishComposingText()`. It logs only `ime op=<op> ok=<bool> len=<n>`.
- Text is bound to its window: the IME applies an `ime` only if its own display (the IME lives on
  the focused editor's display under `LOCAL`; read from its window, else its window context)
  equals `ime.display`. Otherwise (e.g. right after a switch to another Lantern window, before
  the IME moved) it drops it, logging `ime op=<op> dropped len=<n>: typed for display <a>, the
  keyboard is on display <b>`, never the text. If either display is unknown it applies the
  `ime` (typing never breaks), logging that once per IME instance.
- Lantern displays use the IME display policy `LOCAL` only while the session selected the Lantern
  keyboard, and `HIDE` otherwise (the keyboard-less fallback session). Under `HIDE` Android keeps
  the input connection on the launcher on display 0, so commits would never reach the app; under
  `LOCAL` any keyboard that draws (the phone's own) would appear inside the PC window.
- The companion persists the last non-empty `ime_prev.id` in its device-protected storage
  (readable before the first unlock), so it survives a reboot or battery death. An empty
  `ime_prev` never erases it; the next non-empty one replaces it; a watchdog restore that took
  clears it. Each new link marks the in-memory `ime_prev` stale but keeps the persisted one, so a
  link lost before the new session's `ime_prev` still restores the previous original (checked
  against the enabled keyboards, with the fallback below).
- Watchdog: it arms when the link is lost, or when the Lantern IME is created while no link is up
  (the phone rebooted or died mid-session: the setting survives, the helper and Cleanup do not,
  and the system binds the still-selected Lantern IME). The IME (polling every 500 ms) then
  switches to the restore target (this link's `ime_prev.id`, else the persisted one) once 5 s
  have passed, if the Lantern IME is still the current keyboard. With no target known it goes
  straight to the fallback below. A link that comes up within the 5 s (a new session) cancels
  it. A link that a newer one replaced does not count as lost.
- A watchdog switch that throws or does not take is retried every 2 s while the Lantern IME is
  still current, up to 5 attempts. After that, or at once if the target is not among the
  enabled keyboards (`InputMethodManager.getEnabledInputMethodList()`) or no target is known, it
  switches to the first enabled keyboard that is neither Lantern's nor the original, logging
  `ime_restored fallback=<id>`. A fallback that does not take, or no candidate, is logged
  (`ime_restore gave up: <reason>`) and it stops; the Lantern keyboard then stays selected until
  the next session ends or the user picks another keyboard. Once another keyboard is current
  after a switch, it is done for that loss (`ime_restore done`, and the persisted target is
  cleared).
- The watchdog switches only `default_input_method` (the companion has no settings permission):
  after a reboot mid-session, `enabled_input_methods` keeps the Lantern entry and the journal
  stays on the phone until the next session, which restores both from it as usual.

Catalogue on the link:

- The companion starts a full catalogue only when it receives `sync`. The helper sends `sync`
  right after its `hello` reply to lanternd (and the `companion{state}` that follows it), so the
  catalogue goes straight to lanternd instead of being held while the session starts. A
  companion that links later while the session is up gets `sync` right after its own `hello`
  reply (after `ime_prev`, if any).
- Full catalogue: for every profile in `LauncherApps.getProfiles()`, one `app` per package (its
  first `getActivityList` entry): `label` is the activity's label, `icon` its badged icon drawn
  at 192×192 ARGB, PNG, base64 without line breaks (`""` if the PNG is over `MAX_ICON`); then
  `apps_end{count}`. `user` is the profile's Android user id.
- Live updates (`LauncherApps.Callback`, per profile): `app` for an added or changed package
  (or `app_removed` if it has no launcher activity left), `app_removed` for a removed one. A
  profile becoming available or unavailable, or being added or removed, triggers a full
  catalogue. Updates that happen before the first `sync` are dropped (the sync covers them).
- The helper never relays a companion message as is. It rebuilds `app` as
  `pkg, user, label, [work], icon`, setting `work:true` itself when `user` is a managed (work)
  profile (`IUserManager.getUserInfo(user).isManagedProfile()`; false on any error) and ignoring
  any `work` the companion sent. It drops (and logs) an `app`, `app_removed` or `apps_end` with a
  missing or out-of-range field, since lanternd would treat it as a protocol error, and blanks an
  icon whose base64 is longer than `MAX_ICON` allows. It keeps the labels it relayed (by
  `pkg` and `user`; `app_removed` forgets one) for `opened.label`.
- `opened` carries `label` when the helper has a catalogue label for that `pkg` and `user`, and
  `work:true` when `open.user` is a managed profile.

`activation_token` is an XDG activation token used only between `lanternd` and window clients.
It is omitted from the JSON when absent (so `{"t":"focus"}` is the plain focus), the daemon never
sends it to the helper, and the helper ignores unknown keys.

## Window IPC (lanternd ↔ `lantern open`)

The same frames, over the Unix socket `$XDG_RUNTIME_DIR/lantern/daemon.sock`. Its directory is
created mode `0700` (umask `077`) and must be owned by the user; a stale socket file is replaced
on start, a live one means another daemon is running. The daemon listens even without a phone.

- One client per window. Its first frame must be `open` on stream 0, carrying the client's
  `XDG_ACTIVATION_TOKEN` as `activation_token` if it has one. Anything else first gets
  `error{msg}` on stream 0 and the connection is closed.
- No phone session: `error{msg:"phone not connected"}` on stream 0, then close.
- New `pkg`/`user`: the daemon assigns window id `N` (from 1, never reused within a helper
  session) and forwards the `open` to the helper on stream `N` without `activation_token`. The
  helper's `opened`, `video`, `resized`, `error` and `closed` on `N` reach the client unchanged.
- Already open as window `M`: the existing window's client gets `focus{activation_token}` on
  stream `M` (it activates its window with that token); the new client gets `focus` with stream
  `M` and is closed. It should exit 0.
- A client that sends no frame within 5 s of connecting is closed.
- If the existing window's client cannot take the `focus` (it is dropped, as below), its window
  is closed and released and the new `open` is handled as a new window instead.
- After `opened`, the client sends `resize`, `focus`, `close` and input on stream `N`, forwarded
  unchanged, except that `focus` reaches the helper without `activation_token`. A frame on any
  other stream, video, another control type, or an input payload that does not decode is a
  protocol error: only that client is dropped (as a disconnect), and the bad frame never reaches
  the helper, where it would end the whole session.
- `closed` from the helper is forwarded, then the client is closed and `N` released.
- Client disconnect (or drop) without `closed`: the daemon sends `close` on `N` (close = quit)
  and releases `N`.
- A client that falls 240 frames behind is dropped, as a disconnect. The helper is never
  blocked by a slow window.
- Helper session end or restart, daemon shutdown or daemon death: every window client reads EOF
  and exits. Window ids start over with the next session.

### Status (`lantern status`)

Host-only; the helper never sees these, and the daemon never forwards them.

- A client whose first frame is `{"t":"status"}` on stream 0 gets one `status_report` on stream 0
  and is closed. It works with or without a phone session.
- `status_report`: `state` (`"idle"`: no session and none starting; `"starting"`: helper deploy
  and handshake in flight; `"ready"`: live session), `serial` (the device being started or served,
  else `null`), `helper` (the live helper jar's SHA-256 when `ready`, else `null`) and `windows`:
  `[{id: u32, pkg: string, user: u32}]`, the open windows sorted by id.
  Optional `companion` (`"ready"` or `"unavailable"`): the companion link's state, omitted when
  unknown (so a report without it is unchanged).
- `status` after a window's `open` (on any stream) is a protocol error for that client, like any
  other unexpected control type.

`lantern status` prints the report without `t`, e.g.
`{"state":"ready","serial":"20e9318","helper":"ae30…","windows":[{"id":1,"pkg":"com.android.settings","user":0}],"companion":"ready"}`.

### Exit statuses (`lantern`)

`0` ok; `1` runtime failure (daemon unreachable, adb gone, window error); `2` usage error only;
`3` (`lantern daemon` only) the helper broke the protocol handshake: a version mismatch (the
helper's `error{code:"version"}`, or a `hello` echo with another version) or a token echo:
`lanternd.service` has `RestartPreventExitStatus=3`, because a respawn meets the same helper.

## Golden vectors

`host/proto/golden/*.bin` are complete encoded frames (header + payload) used by both the Rust and
Java test suites. The values they encode are listed at the top of `host/proto/tests/golden.rs`.
Regenerate with `LANTERN_WRITE_GOLDEN=1 cargo test -p lantern-proto --test golden`.
