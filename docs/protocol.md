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
| `open` | `pkg: string`, `user: u32`, `w: u32`, `h: u32`, `dpi: u32`, optional `activation_token: string` |
| `opened` | `display: i32`, `task: i32`, `codec: string`, `w: u32`, `h: u32` |
| `resize` | `w: u32`, `h: u32`, `dpi: u32` |
| `resized` | `w: u32`, `h: u32` |
| `focus` | optional `activation_token: string` |
| `close` | none |
| `closed` | `reason`: one of `"closed"`, `"app_exited"`, `"error"` |
| `error` | `msg: string` |
| `bye` | none |

## Session rules (phone helper)

- The first frame must be `hello` on stream 0 with `version: 1` and the helper's token. The helper
  replies `hello{version:1, token:""}`; on a wrong version or token (or anything else first) it sends
  one `error{msg}` on stream 0 and closes the connection. Phone state (stay-awake, panel) only
  changes after a valid hello.
- `opened` is sent before any video for that window. `codec` is `"h265"` or `"h264"`.
- `resized{w,h}` answers `resize`, and is also sent unprompted when the app rotates its display.
- A per-window failure (open, resize or the video stream) is `error{msg}` followed by
  `closed{reason:"error"}` on that window's stream; the session continues.
- Frames for an unknown or already-closed window are ignored. A protocol error (bad JSON, unknown
  `t`, bad input payload, video from the host) ends the session with `error{msg}` on stream 0.
- `bye` (or EOF) ends the session: the helper closes every window without sending `closed`.

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

## Golden vectors

`host/proto/golden/*.bin` are complete encoded frames (header + payload) used by both the Rust and
Java test suites. The values they encode are listed at the top of `host/proto/tests/golden.rs`.
Regenerate with `LANTERN_WRITE_GOLDEN=1 cargo test -p lantern-proto --test golden`.
