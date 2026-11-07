# Lantern wire protocol v1

Spoken between the host (`lanternd`, Rust: `host/proto`, crate `lantern-proto`) and the phone-side
server (Java) over a single byte stream.

## Wire format

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

## Details

- Frame kinds: `1` Control, `2` Video, `3` Input. Any other kind is a protocol error.
- Max payload is `8 << 20` bytes (8 MiB), so `len` must be in `5 ..= MAX_PAYLOAD + 5`; anything else
  is rejected as invalid data before the payload is read.
- EOF exactly at a frame boundary is a clean end of stream. EOF inside a frame (including inside the
  4-byte `len`) is an error.
- Pointer and scroll `x`/`y` are normalized to `0..1` of the display. `f32` is IEEE-754 binary32.
- Input payloads must be exactly the size of their type; trailing bytes are an error.

## Control messages

JSON objects tagged by `"t"` (snake_case). Example:
`{"t":"open","pkg":"com.android.settings","user":0,"w":1280,"h":800,"dpi":160}`

| `t` | fields |
|---|---|
| `hello` | `version: u32`, `token: string` |
| `open` | `pkg: string`, `user: u32` (Android user/profile id, `0..=i32::MAX`), `w: u32`, `h: u32`, `dpi: u32`, optional `activation_token: string` |
| `opened` | `display: i32`, `task: i32`, `codec: string`, `w: u32`, `h: u32` |
| `resize` | `w: u32`, `h: u32`, `dpi: u32` |
| `resized` | `w: u32`, `h: u32` |
| `focus` | optional `activation_token: string` |
| `close` | none |
| `closed` | `reason`: one of `"closed"`, `"app_exited"`, `"error"` |
| `error` | `msg: string`, optional `code: string`, optional `version: u32` |
| `bye` | none |

## Session rules (phone helper)

- The first frame must be `hello` on stream 0 with `version: 1` and the helper's token. The helper
  replies `hello{version:1, token:""}`; on a wrong token (or anything else first) it sends one
  `error{msg}` on stream 0 and closes the connection. On a wrong version it sends
  `error{msg, code:"version", version:<the helper's own version>}` instead (the helper never
  answers `hello` with another version). Phone state (stay-awake, panel) only changes after a
  valid hello.
- `lanternd` treats a handshake `error` with `code:"version"` (or, from a helper predating
  `code`, a `msg` starting `protocol version mismatch`) as a version mismatch and exits 3 (see
  Exit statuses); any other handshake `error` is retried with backoff.
- `opened` is sent before any video for that window. `codec` is `"h265"` or `"h264"`.
- `open.user`: the helper resolves `pkg`'s launcher activity in that user's package manager and
  starts it as that user (`am start --user N` semantics). Before creating a display it refuses,
  as a per-window failure, a user that does not exist, is not running, or has the
  `no_debugging_features` restriction (Android bars the shell uid from such users), and a `pkg`
  that is not installed for that user or has no launcher activity. The `error.msg` is meant for
  the person at the PC (e.g. `open com.x failed: com.x is not installed for Android user 10`).
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
- `status` after a window's `open` (on any stream) is a protocol error for that client, like any
  other unexpected control type.

`lantern status` prints the report without `t`, e.g.
`{"state":"ready","serial":"20e9318","helper":"ae30…","windows":[{"id":1,"pkg":"com.android.settings","user":0}]}`.

### Exit statuses (`lantern`)

`0` ok; `1` runtime failure (daemon unreachable, adb gone, window error); `2` usage error only;
`3` (`lantern daemon` only) the helper broke the protocol handshake: a version mismatch (the
helper's `error{code:"version"}`, or a `hello` echo with another version) or a token echo:
`lanternd.service` has `RestartPreventExitStatus=3`, because a respawn meets the same helper.

## Golden vectors

`host/proto/golden/*.bin` are complete encoded frames (header + payload) used by both the Rust and
Java test suites. The values they encode are listed at the top of `host/proto/tests/golden.rs`.
Regenerate with `LANTERN_WRITE_GOLDEN=1 cargo test -p lantern-proto --test golden`.
