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
| `open` | `pkg: string`, `user: u32`, `w: u32`, `h: u32`, `dpi: u32` |
| `opened` | `display: i32`, `task: i32`, `codec: string`, `w: u32`, `h: u32` |
| `resize` | `w: u32`, `h: u32`, `dpi: u32` |
| `resized` | `w: u32`, `h: u32` |
| `focus` | none |
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

## Golden vectors

`host/proto/golden/*.bin` are complete encoded frames (header + payload) used by both the Rust and
Java test suites. The values they encode are listed at the top of `host/proto/tests/golden.rs`.
Regenerate with `LANTERN_WRITE_GOLDEN=1 cargo test -p lantern-proto --test golden`.
