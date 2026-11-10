//! Lantern wire protocol v1. See docs/protocol.md.
//!
//! All integers are big-endian. A frame is `u32 len` (bytes after this field), `u8 kind`,
//! `u32 stream`, then `payload[len-5]`.

use std::io::{self, ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest payload accepted on either side (8 MiB).
pub const MAX_PAYLOAD: u32 = 8 << 20;

/// Largest `clip.text`, in UTF-8 bytes (1 MiB). Larger clipboard text is not sent.
pub const MAX_CLIP: usize = 1 << 20;

/// Largest `ime.text`, in UTF-8 bytes (64 KiB).
pub const MAX_IME_TEXT: usize = 1 << 16;

/// Largest icon PNG for `app.icon`, in bytes before base64 (1 MiB). A larger icon is sent as `""`.
pub const MAX_ICON: usize = 1 << 20;

/// Bytes covered by `len` that are not payload: `u8 kind` + `u32 stream`.
const HEADER_AFTER_LEN: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Control = 1,
    Video = 2,
    Input = 3,
}

impl TryFrom<u8> for Kind {
    type Error = io::Error;
    fn try_from(v: u8) -> io::Result<Self> {
        match v {
            1 => Ok(Kind::Control),
            2 => Ok(Kind::Video),
            3 => Ok(Kind::Input),
            _ => Err(invalid(format!("unknown frame kind {v}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub stream: u32,
    pub payload: Vec<u8>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, msg.into())
}

fn encode_header(f: &Frame) -> io::Result<[u8; 9]> {
    if f.payload.len() > MAX_PAYLOAD as usize {
        return Err(invalid(format!(
            "payload {} exceeds MAX_PAYLOAD",
            f.payload.len()
        )));
    }
    let len = f.payload.len() as u32 + HEADER_AFTER_LEN;
    let mut h = [0u8; 9];
    h[0..4].copy_from_slice(&len.to_be_bytes());
    h[4] = f.kind as u8;
    h[5..9].copy_from_slice(&f.stream.to_be_bytes());
    Ok(h)
}

/// Validates `len` and returns the payload length.
fn payload_len(len: u32) -> io::Result<usize> {
    if len < HEADER_AFTER_LEN {
        return Err(invalid(format!("frame len {len} shorter than header")));
    }
    let n = len - HEADER_AFTER_LEN;
    if n > MAX_PAYLOAD {
        return Err(invalid(format!("frame payload {n} exceeds MAX_PAYLOAD")));
    }
    Ok(n as usize)
}

fn parse_rest(rest: &[u8; 5]) -> io::Result<(Kind, u32)> {
    let kind = Kind::try_from(rest[0])?;
    let stream = u32::from_be_bytes(rest[1..5].try_into().unwrap());
    Ok((kind, stream))
}

pub fn write_frame(w: &mut impl Write, f: &Frame) -> io::Result<()> {
    let h = encode_header(f)?;
    w.write_all(&h)?;
    w.write_all(&f.payload)
}

/// Reads one frame. Returns `Ok(None)` on clean EOF at a frame boundary; EOF anywhere
/// inside a frame is `UnexpectedEof`.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let n = payload_len(u32::from_be_bytes(len))?;
    let mut rest = [0u8; 5];
    r.read_exact(&mut rest)?;
    let (kind, stream) = parse_rest(&rest)?;
    let mut payload = vec![0u8; n];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame {
        kind,
        stream,
        payload,
    }))
}

pub async fn write_frame_async<W: AsyncWrite + Unpin + ?Sized>(
    w: &mut W,
    f: &Frame,
) -> io::Result<()> {
    let h = encode_header(f)?;
    w.write_all(&h).await?;
    w.write_all(&f.payload).await
}

/// Async variant of [`read_frame`], same EOF semantics.
pub async fn read_frame_async<R: AsyncRead + Unpin + ?Sized>(
    r: &mut R,
) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]).await {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let n = payload_len(u32::from_be_bytes(len))?;
    let mut rest = [0u8; 5];
    r.read_exact(&mut rest).await?;
    let (kind, stream) = parse_rest(&rest)?;
    let mut payload = vec![0u8; n];
    r.read_exact(&mut payload).await?;
    Ok(Some(Frame {
        kind,
        stream,
        payload,
    }))
}

// ---------------------------------------------------------------------------------------------
// Control (UTF-8 JSON payload)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Closed,
    AppExited,
    Error,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    Hello {
        version: u32,
        token: String,
        /// Whether the session switches the phone to the Lantern keyboard. False gives the
        /// stay-awake-only session (no keyboard setting touched). Absent on the wire when true,
        /// so every v1 `hello` is byte-identical; a missing field decodes as true.
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        keyboard: bool,
    },
    Open {
        pkg: String,
        user: u32,
        w: u32,
        h: u32,
        dpi: u32,
        /// XDG activation token of the `lantern open` process (host-only; the daemon strips it
        /// before forwarding to the helper). Absent on the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation_token: Option<String>,
    },
    Opened {
        display: i32,
        task: i32,
        codec: String,
        w: u32,
        h: u32,
        /// The task existed before the launch (it was open on the phone or in its recents) and
        /// Android moved it onto the Lantern display, so closing the window quits it. Absent on the
        /// wire when false, so a v1 `opened` is byte-identical.
        #[serde(default, skip_serializing_if = "is_false")]
        moved: bool,
        /// The app's launcher label, for the window title. Absent when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// The app runs in a work (managed) profile. Absent when false.
        #[serde(default, skip_serializing_if = "is_false")]
        work: bool,
    },
    Resize {
        w: u32,
        h: u32,
        dpi: u32,
    },
    Resized {
        w: u32,
        h: u32,
    },
    /// `{"t":"focus"}` when `activation_token` is `None`, byte-identical to the v1 unit form.
    Focus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation_token: Option<String>,
    },
    Close,
    Closed {
        reason: CloseReason,
    },
    /// `code`/`version` are optional and absent on the wire when `None`, so `{"t":"error","msg":..}`
    /// is unchanged. The helper sets `code:"version"` plus its own `version` on a handshake
    /// version mismatch (docs/protocol.md).
    Error {
        msg: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<u32>,
    },
    Bye,
    /// Host-only (`lantern status` -> lanternd, stream 0, first frame). Never sent to the helper.
    Status,
    /// Host-only: lanternd's answer to [`Control::Status`].
    StatusReport(StatusReport),
    /// Host-only (`lantern connect|disconnect` -> lanternd, stream 0, first frame): queue `cmd`.
    /// lanternd answers with its current `status_report` and closes. Never sent to the helper.
    Daemon {
        cmd: DaemonCmd,
    },
    /// Helper -> daemon, stream 0: one launcher app of the catalogue. `icon` is a base64 PNG
    /// (at most [`MAX_ICON`] bytes before base64) or `""`.
    App {
        pkg: String,
        user: u32,
        label: String,
        /// Absent when false.
        #[serde(default, skip_serializing_if = "is_false")]
        work: bool,
        icon: String,
    },
    /// Helper -> daemon, stream 0: a full catalogue of `count` `app` messages was just sent.
    AppsEnd {
        count: u32,
    },
    /// Helper -> daemon, stream 0: the app is gone for that user.
    AppRemoved {
        pkg: String,
        user: u32,
    },
    /// Helper -> daemon, stream 0: whether the companion app is linked.
    Companion {
        state: CompanionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        msg: Option<String>,
    },
    /// Stream 0, both directions: clipboard text (at most [`MAX_CLIP`] bytes).
    Clip {
        text: String,
    },
    /// Window stream N, window -> daemon -> helper: text input (at most [`MAX_IME_TEXT`] bytes).
    /// `finish` carries `text: ""`.
    Ime {
        op: ImeOp,
        text: String,
    },
}

/// What `lantern connect|disconnect` asks lanternd to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonCmd {
    /// Serve the phone again after a disconnect (and retry a backed-off start now).
    Connect,
    /// End the session and leave the phone alone until it is replugged or connected again.
    Disconnect,
}

/// Whether the companion app is linked to the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionState {
    Ready,
    Unavailable,
}

/// `commit` inserts text; `compose` replaces the composing region (preedit); `finish` ends
/// composition, keeping the composing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImeOp {
    Commit,
    Compose,
    Finish,
}

/// The daemon's phone session, as `lantern status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// No session and none starting (no phone, or waiting to retry).
    Idle,
    /// Helper deploy/handshake in flight.
    Starting,
    /// A helper session is live; windows can open.
    Ready,
    /// Host-only: the user disconnected the phone in Lantern (or it is undoing its setup);
    /// nothing starts until it is replugged or connected again. `serial` is that phone.
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u32,
    pub pkg: String,
    pub user: u32,
}

/// `serial` is the session's device (`null` when idle); `helper` is the live helper jar's
/// SHA-256 (`null` unless ready). `windows` are the open windows, by id. `companion` is optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub state: SessionState,
    pub serial: Option<String>,
    pub helper: Option<String>,
    pub windows: Vec<WindowInfo>,
    /// The companion link's state while `ready`; absent otherwise (so a v1 report is unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion: Option<CompanionState>,
}

impl Control {
    /// A plain `error{msg}` (no `code`).
    pub fn error(msg: impl Into<String>) -> Self {
        Self::Error {
            msg: msg.into(),
            code: None,
            version: None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("Control serializes")
    }

    /// The error names the failure's category, position and the payload's size only: serde_json's
    /// own message quotes offending values, and a payload may hold typed or clipboard text.
    pub fn decode(b: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(b).map_err(|e| {
            invalid(format!(
                "bad control json ({:?} error at line {} column {}, {} bytes)",
                e.classify(),
                e.line(),
                e.column(),
                b.len()
            ))
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Video

const FLAG_CONFIG: u8 = 1 << 0;
const FLAG_KEY: u8 = 1 << 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket {
    pub pts_us: u64,
    pub config: bool,
    pub key: bool,
    pub data: Vec<u8>,
}

impl VideoPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(9 + self.data.len());
        b.extend_from_slice(&self.pts_us.to_be_bytes());
        let mut flags = 0;
        if self.config {
            flags |= FLAG_CONFIG;
        }
        if self.key {
            flags |= FLAG_KEY;
        }
        b.push(flags);
        b.extend_from_slice(&self.data);
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let mut c = Cur(b);
        let pts_us = c.u64()?;
        let flags = c.u8()?;
        Ok(VideoPacket {
            pts_us,
            config: flags & FLAG_CONFIG != 0,
            key: flags & FLAG_KEY != 0,
            data: c.0.to_vec(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Input

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerAction {
    Down = 0,
    Up = 1,
    Move = 2,
    HoverMove = 3,
    Cancel = 4,
}

impl TryFrom<u8> for PointerAction {
    type Error = io::Error;
    fn try_from(v: u8) -> io::Result<Self> {
        Ok(match v {
            0 => PointerAction::Down,
            1 => PointerAction::Up,
            2 => PointerAction::Move,
            3 => PointerAction::HoverMove,
            4 => PointerAction::Cancel,
            _ => return Err(invalid(format!("unknown pointer action {v}"))),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyAction {
    Down = 0,
    Up = 1,
}

impl TryFrom<u8> for KeyAction {
    type Error = io::Error;
    fn try_from(v: u8) -> io::Result<Self> {
        Ok(match v {
            0 => KeyAction::Down,
            1 => KeyAction::Up,
            _ => return Err(invalid(format!("unknown key action {v}"))),
        })
    }
}

const INPUT_POINTER: u8 = 1;
const INPUT_SCROLL: u8 = 2;
const INPUT_KEY: u8 = 3;

/// Input event. `x`/`y` are normalized to 0..1 of the display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    Pointer {
        action: PointerAction,
        id: u64,
        x: f32,
        y: f32,
        buttons: u32,
        pressure: f32,
    },
    Scroll {
        x: f32,
        y: f32,
        h: f32,
        v: f32,
    },
    Key {
        action: KeyAction,
        keycode: u32,
        meta: u32,
        repeat: u32,
    },
}

impl InputEvent {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(26);
        match *self {
            InputEvent::Pointer {
                action,
                id,
                x,
                y,
                buttons,
                pressure,
            } => {
                b.push(INPUT_POINTER);
                b.push(action as u8);
                b.extend_from_slice(&id.to_be_bytes());
                b.extend_from_slice(&x.to_be_bytes());
                b.extend_from_slice(&y.to_be_bytes());
                b.extend_from_slice(&buttons.to_be_bytes());
                b.extend_from_slice(&pressure.to_be_bytes());
            }
            InputEvent::Scroll { x, y, h, v } => {
                b.push(INPUT_SCROLL);
                for f in [x, y, h, v] {
                    b.extend_from_slice(&f.to_be_bytes());
                }
            }
            InputEvent::Key {
                action,
                keycode,
                meta,
                repeat,
            } => {
                b.push(INPUT_KEY);
                b.push(action as u8);
                b.extend_from_slice(&keycode.to_be_bytes());
                b.extend_from_slice(&meta.to_be_bytes());
                b.extend_from_slice(&repeat.to_be_bytes());
            }
        }
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let mut c = Cur(b);
        let ev = match c.u8()? {
            INPUT_POINTER => InputEvent::Pointer {
                action: PointerAction::try_from(c.u8()?)?,
                id: c.u64()?,
                x: c.f32()?,
                y: c.f32()?,
                buttons: c.u32()?,
                pressure: c.f32()?,
            },
            INPUT_SCROLL => InputEvent::Scroll {
                x: c.f32()?,
                y: c.f32()?,
                h: c.f32()?,
                v: c.f32()?,
            },
            INPUT_KEY => InputEvent::Key {
                action: KeyAction::try_from(c.u8()?)?,
                keycode: c.u32()?,
                meta: c.u32()?,
                repeat: c.u32()?,
            },
            t => return Err(invalid(format!("unknown input type {t}"))),
        };
        if !c.0.is_empty() {
            return Err(invalid(format!(
                "{} trailing bytes in input event",
                c.0.len()
            )));
        }
        Ok(ev)
    }
}

/// Big-endian cursor over a payload; running short is `InvalidData` (the frame was complete).
struct Cur<'a>(&'a [u8]);

impl Cur<'_> {
    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        if self.0.len() < N {
            return Err(invalid("payload too short"));
        }
        let (h, t) = self.0.split_at(N);
        self.0 = t;
        Ok(h.try_into().unwrap())
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take::<1>()?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        self.take().map(u32::from_be_bytes)
    }
    fn u64(&mut self) -> io::Result<u64> {
        self.take().map(u64::from_be_bytes)
    }
    fn f32(&mut self) -> io::Result<f32> {
        self.take().map(f32::from_be_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    fn frames() -> Vec<Frame> {
        vec![
            Frame {
                kind: Kind::Control,
                stream: 0,
                payload: br#"{"t":"bye"}"#.to_vec(),
            },
            Frame {
                kind: Kind::Video,
                stream: 7,
                payload: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            },
            Frame {
                kind: Kind::Input,
                stream: 0xdead_beef,
                payload: vec![],
            },
        ]
    }

    /// serde_json's messages quote offending values (an unknown variant, a string of the wrong
    /// type), and decode errors are logged, so they must never carry the payload: it may hold typed
    /// or clipboard text.
    #[test]
    fn control_decode_error_never_echoes_the_payload() {
        for bad in [
            &br#"{"t":"ime","op":"hunter2 secret","text":""}"#[..],
            br#"{"t":"open","pkg":"x","user":"hunter2 secret","w":1,"h":1,"dpi":1}"#,
            br#"{"t":"hunter2 secret"}"#,
            br#"{"t":"clip","text":"hunter2 secret""#,
        ] {
            let error = Control::decode(bad).unwrap_err().to_string();
            assert!(!error.contains("hunter2"), "{error}");
            assert!(
                error.contains(&format!("{} bytes", bad.len())),
                "names the size: {error}"
            );
        }
    }

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        for f in frames() {
            write_frame(&mut buf, &f).unwrap();
        }
        let mut r = Cursor::new(buf);
        for f in frames() {
            assert_eq!(read_frame(&mut r).unwrap(), Some(f));
        }
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn frame_header_layout() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Frame {
                kind: Kind::Video,
                stream: 0x0102_0304,
                payload: vec![0xaa, 0xbb],
            },
        )
        .unwrap();
        assert_eq!(buf, [0, 0, 0, 7, 2, 1, 2, 3, 4, 0xaa, 0xbb]);
    }

    #[test]
    fn truncated_frame_is_error() {
        let mut bytes = vec![0, 0, 0, 10];
        bytes.extend_from_slice(&[1, 0, 0]);
        let e = read_frame(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_header_is_error() {
        let e = read_frame(&mut Cursor::new(vec![0, 0])).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversize_rejected() {
        let mut bytes = (MAX_PAYLOAD + 6).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[1, 0, 0, 0, 0]);
        let e = read_frame(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn oversize_rejected_on_write() {
        let f = Frame {
            kind: Kind::Video,
            stream: 1,
            payload: vec![0; MAX_PAYLOAD as usize + 1],
        };
        let e = write_frame(&mut Vec::new(), &f).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn bad_kind_and_short_len_rejected() {
        let e = read_frame(&mut Cursor::new(vec![0, 0, 0, 5, 9, 0, 0, 0, 0])).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        let e = read_frame(&mut Cursor::new(vec![0, 0, 0, 4, 1, 0, 0, 0])).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn control_json_shape() {
        let c = Control::Open {
            pkg: "com.android.settings".into(),
            user: 0,
            w: 1280,
            h: 800,
            dpi: 160,
            activation_token: None,
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"t":"open","pkg":"com.android.settings","user":0,"w":1280,"h":800,"dpi":160}"#
        );
    }

    #[test]
    fn status_request_and_report_json_shape() {
        assert_eq!(Control::Status.encode(), br#"{"t":"status"}"#);
        let report = Control::StatusReport(StatusReport {
            state: SessionState::Ready,
            serial: Some("20e9318".into()),
            helper: Some("ab12".into()),
            windows: vec![WindowInfo {
                id: 1,
                pkg: "com.android.settings".into(),
                user: 0,
            }],
            companion: None,
        });
        let json = r#"{"t":"status_report","state":"ready","serial":"20e9318","helper":"ab12","windows":[{"id":1,"pkg":"com.android.settings","user":0}]}"#;
        assert_eq!(String::from_utf8(report.encode()).unwrap(), json);
        assert_eq!(Control::decode(json.as_bytes()).unwrap(), report);
        let idle = Control::StatusReport(StatusReport {
            state: SessionState::Idle,
            serial: None,
            helper: None,
            windows: vec![],
            companion: None,
        });
        assert_eq!(
            String::from_utf8(idle.encode()).unwrap(),
            r#"{"t":"status_report","state":"idle","serial":null,"helper":null,"windows":[]}"#
        );
        assert_eq!(
            serde_json::to_string(&SessionState::Starting).unwrap(),
            r#""starting""#
        );
    }

    #[test]
    fn activation_token_is_optional_on_open_and_focus() {
        let open = Control::Open {
            pkg: "p".into(),
            user: 0,
            w: 1,
            h: 2,
            dpi: 3,
            activation_token: Some("tok".into()),
        };
        let s = serde_json::to_string(&open).unwrap();
        assert_eq!(
            s,
            r#"{"t":"open","pkg":"p","user":0,"w":1,"h":2,"dpi":3,"activation_token":"tok"}"#
        );
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), open);
        let bare: Control =
            serde_json::from_str(r#"{"t":"open","pkg":"p","user":0,"w":1,"h":2,"dpi":3}"#).unwrap();
        assert!(matches!(
            bare,
            Control::Open {
                activation_token: None,
                ..
            }
        ));

        let focus = Control::Focus {
            activation_token: Some("tok".into()),
        };
        let s = serde_json::to_string(&focus).unwrap();
        assert_eq!(s, r#"{"t":"focus","activation_token":"tok"}"#);
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), focus);
        assert_eq!(
            serde_json::from_str::<Control>(r#"{"t":"focus"}"#).unwrap(),
            Control::Focus {
                activation_token: None
            }
        );
    }

    #[test]
    fn error_code_fields_are_optional_and_absent_by_default() {
        // v1 bytes unchanged when there is no code.
        assert_eq!(
            serde_json::to_string(&Control::error("boom")).unwrap(),
            r#"{"t":"error","msg":"boom"}"#
        );
        assert_eq!(
            serde_json::from_str::<Control>(r#"{"t":"error","msg":"boom"}"#).unwrap(),
            Control::error("boom")
        );
        let v = Control::Error {
            msg: "m".into(),
            code: Some("version".into()),
            version: Some(2),
        };
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"t":"error","msg":"m","code":"version","version":2}"#
        );
    }

    #[test]
    fn control_unit_and_reason_shapes() {
        assert_eq!(
            serde_json::to_string(&Control::Focus {
                activation_token: None
            })
            .unwrap(),
            r#"{"t":"focus"}"#
        );
        assert_eq!(
            serde_json::to_string(&Control::Bye).unwrap(),
            r#"{"t":"bye"}"#
        );
        // `moved` is optional: absent when false (v1 bytes), defaulted when missing.
        let v1 = r#"{"t":"opened","display":12,"task":7,"codec":"h264","w":1280,"h":800}"#;
        let opened = |moved| Control::Opened {
            display: 12,
            task: 7,
            codec: "h264".into(),
            w: 1280,
            h: 800,
            moved,
            label: None,
            work: false,
        };
        assert_eq!(serde_json::to_string(&opened(false)).unwrap(), v1);
        assert_eq!(serde_json::from_str::<Control>(v1).unwrap(), opened(false));
        assert_eq!(
            serde_json::to_string(&opened(true)).unwrap(),
            r#"{"t":"opened","display":12,"task":7,"codec":"h264","w":1280,"h":800,"moved":true}"#
        );
        let c = Control::Closed {
            reason: CloseReason::AppExited,
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#"{"t":"closed","reason":"app_exited"}"#);
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), c);
        let all = vec![
            Control::Hello {
                version: 1,
                token: "abc".into(),
                keyboard: true,
            },
            Control::Hello {
                version: 1,
                token: "abc".into(),
                keyboard: false,
            },
            Control::Opened {
                display: 12,
                task: -1,
                codec: "h264".into(),
                w: 1280,
                h: 800,
                moved: false,
                label: None,
                work: false,
            },
            Control::Opened {
                display: 12,
                task: 7,
                codec: "h265".into(),
                w: 1280,
                h: 800,
                moved: true,
                label: None,
                work: false,
            },
            Control::Resize { w: 1, h: 2, dpi: 3 },
            Control::Resized { w: 4, h: 5 },
            Control::Close,
            Control::Closed {
                reason: CloseReason::Error,
            },
            Control::error("boom"),
            Control::Error {
                msg: "old".into(),
                code: Some("version".into()),
                version: Some(2),
            },
        ];
        for c in all {
            let s = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), c);
        }
    }

    fn opened_v2(label: Option<&str>, work: bool) -> Control {
        Control::Opened {
            display: 12,
            task: 7,
            codec: "h264".into(),
            w: 1280,
            h: 800,
            moved: false,
            label: label.map(str::to_owned),
            work,
        }
    }

    fn hello(keyboard: bool) -> Control {
        Control::Hello {
            version: 1,
            token: "t".into(),
            keyboard,
        }
    }

    #[test]
    fn hello_without_keyboard_is_v1_bytes() {
        // keyboard defaults to true and is omitted then, so every existing hello is unchanged.
        let v1 = r#"{"t":"hello","version":1,"token":"t"}"#;
        assert_eq!(serde_json::to_string(&hello(true)).unwrap(), v1);
        assert_eq!(hello(true).encode(), v1.as_bytes());
    }

    #[test]
    fn hello_keyboard_false_json_shape() {
        let s = serde_json::to_string(&hello(false)).unwrap();
        assert_eq!(
            s,
            r#"{"t":"hello","version":1,"token":"t","keyboard":false}"#
        );
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), hello(false));
    }

    #[test]
    fn hello_missing_keyboard_decodes_true() {
        assert_eq!(
            serde_json::from_str::<Control>(r#"{"t":"hello","version":1,"token":"t"}"#).unwrap(),
            hello(true)
        );
        // The helper's echo (empty token) also decodes with the default.
        assert_eq!(
            Control::decode(br#"{"t":"hello","version":1,"token":""}"#).unwrap(),
            Control::Hello {
                version: 1,
                token: String::new(),
                keyboard: true,
            }
        );
    }

    #[test]
    fn opened_without_label_is_v1_bytes() {
        let v1 = r#"{"t":"opened","display":12,"task":7,"codec":"h264","w":1280,"h":800}"#;
        assert_eq!(serde_json::to_string(&opened_v2(None, false)).unwrap(), v1);
        assert_eq!(
            serde_json::from_str::<Control>(v1).unwrap(),
            opened_v2(None, false)
        );
        let full = opened_v2(Some("Settings"), true);
        let s = serde_json::to_string(&full).unwrap();
        assert_eq!(
            s,
            r#"{"t":"opened","display":12,"task":7,"codec":"h264","w":1280,"h":800,"label":"Settings","work":true}"#
        );
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), full);
    }

    #[test]
    fn ime_json_shape() {
        let c = Control::Ime {
            op: ImeOp::Compose,
            text: "việ".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#"{"t":"ime","op":"compose","text":"việ"}"#);
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), c);
        for (op, name) in [
            (ImeOp::Commit, "commit"),
            (ImeOp::Compose, "compose"),
            (ImeOp::Finish, "finish"),
        ] {
            assert_eq!(serde_json::to_string(&op).unwrap(), format!("\"{name}\""));
        }
        assert_eq!(
            Control::Ime {
                op: ImeOp::Finish,
                text: String::new()
            }
            .encode(),
            br#"{"t":"ime","op":"finish","text":""}"#
        );
        assert!(Control::decode(br#"{"t":"ime","op":"paste","text":""}"#).is_err());
    }

    #[test]
    fn clip_roundtrip_unicode() {
        let text = "Grüße, việt 日本語 🎉\n\t\"quoted\" \\ \u{0}";
        let c = Control::Clip { text: text.into() };
        let b = c.encode();
        assert_eq!(Control::decode(&b).unwrap(), c);
        assert!(String::from_utf8(b)
            .unwrap()
            .starts_with(r#"{"t":"clip","text":""#));
    }

    #[test]
    fn app_json_shape() {
        let app = |work| Control::App {
            pkg: "com.android.settings".into(),
            user: 0,
            label: "Settings".into(),
            work,
            icon: String::new(),
        };
        let s = serde_json::to_string(&app(false)).unwrap();
        assert_eq!(
            s,
            r#"{"t":"app","pkg":"com.android.settings","user":0,"label":"Settings","icon":""}"#
        );
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), app(false));
        let s = serde_json::to_string(&app(true)).unwrap();
        assert_eq!(
            s,
            r#"{"t":"app","pkg":"com.android.settings","user":0,"label":"Settings","work":true,"icon":""}"#
        );
        assert_eq!(serde_json::from_str::<Control>(&s).unwrap(), app(true));
    }

    #[test]
    fn catalogue_and_companion_shapes() {
        let cases: Vec<(Control, &str)> = vec![
            (
                Control::AppsEnd { count: 42 },
                r#"{"t":"apps_end","count":42}"#,
            ),
            (
                Control::AppRemoved {
                    pkg: "com.x".into(),
                    user: 10,
                },
                r#"{"t":"app_removed","pkg":"com.x","user":10}"#,
            ),
            (
                Control::Companion {
                    state: CompanionState::Ready,
                    msg: None,
                },
                r#"{"t":"companion","state":"ready"}"#,
            ),
            (
                Control::Companion {
                    state: CompanionState::Unavailable,
                    msg: Some("not installed".into()),
                },
                r#"{"t":"companion","state":"unavailable","msg":"not installed"}"#,
            ),
        ];
        for (c, json) in cases {
            assert_eq!(String::from_utf8(c.encode()).unwrap(), json);
            assert_eq!(Control::decode(json.as_bytes()).unwrap(), c);
        }
    }

    #[test]
    fn status_report_companion_is_optional() {
        let mut r = StatusReport {
            state: SessionState::Ready,
            serial: None,
            helper: None,
            windows: vec![],
            companion: None,
        };
        let v1 =
            r#"{"t":"status_report","state":"ready","serial":null,"helper":null,"windows":[]}"#;
        assert_eq!(
            String::from_utf8(Control::StatusReport(r.clone()).encode()).unwrap(),
            v1
        );
        assert_eq!(
            Control::decode(v1.as_bytes()).unwrap(),
            Control::StatusReport(r.clone())
        );
        r.companion = Some(CompanionState::Unavailable);
        let s = String::from_utf8(Control::StatusReport(r.clone()).encode()).unwrap();
        assert_eq!(
            s,
            r#"{"t":"status_report","state":"ready","serial":null,"helper":null,"windows":[],"companion":"unavailable"}"#
        );
        assert_eq!(
            Control::decode(s.as_bytes()).unwrap(),
            Control::StatusReport(r)
        );
    }

    #[test]
    fn session_state_paused_json() {
        assert_eq!(
            serde_json::to_string(&SessionState::Paused).unwrap(),
            r#""paused""#
        );
        let report = Control::StatusReport(StatusReport {
            state: SessionState::Paused,
            serial: Some("20e9318".into()),
            helper: None,
            windows: vec![],
            companion: None,
        });
        let json = r#"{"t":"status_report","state":"paused","serial":"20e9318","helper":null,"windows":[]}"#;
        assert_eq!(String::from_utf8(report.encode()).unwrap(), json);
        assert_eq!(Control::decode(json.as_bytes()).unwrap(), report);
    }

    #[test]
    fn daemon_cmd_json_shape() {
        for (cmd, json) in [
            (DaemonCmd::Connect, r#"{"t":"daemon","cmd":"connect"}"#),
            (
                DaemonCmd::Disconnect,
                r#"{"t":"daemon","cmd":"disconnect"}"#,
            ),
        ] {
            let c = Control::Daemon { cmd };
            assert_eq!(String::from_utf8(c.encode()).unwrap(), json);
            assert_eq!(Control::decode(json.as_bytes()).unwrap(), c);
        }
        assert!(Control::decode(br#"{"t":"daemon","cmd":"quit"}"#).is_err());
        assert!(Control::decode(br#"{"t":"daemon"}"#).is_err());
    }

    #[test]
    fn text_limits() {
        assert_eq!(MAX_CLIP, 1 << 20);
        assert_eq!(MAX_IME_TEXT, 1 << 16);
        assert_eq!(MAX_ICON, 1 << 20);
        // A maximal clip still fits one frame even if every byte becomes a 6-byte `\u00XX` escape.
        const { assert!(6 * MAX_CLIP + 64 <= MAX_PAYLOAD as usize) };
    }

    #[test]
    fn input_roundtrip() {
        let events = vec![
            InputEvent::Pointer {
                action: PointerAction::HoverMove,
                id: u64::MAX - 3,
                x: 0.25,
                y: 0.75,
                buttons: 1,
                pressure: 0.5,
            },
            InputEvent::Scroll {
                x: 0.5,
                y: 0.125,
                h: -1.0,
                v: 2.5,
            },
            InputEvent::Key {
                action: KeyAction::Up,
                keycode: 66,
                meta: 0x1000,
                repeat: 3,
            },
        ];
        for e in events {
            assert_eq!(InputEvent::decode(&e.encode()).unwrap(), e);
        }
    }

    #[test]
    fn input_layout_and_errors() {
        let k = InputEvent::Key {
            action: KeyAction::Down,
            keycode: 29,
            meta: 1,
            repeat: 0,
        };
        assert_eq!(k.encode(), [3, 0, 0, 0, 0, 29, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(
            InputEvent::Pointer {
                action: PointerAction::Down,
                id: 0,
                x: 0.0,
                y: 0.0,
                buttons: 0,
                pressure: 0.0
            }
            .encode()
            .len(),
            1 + 1 + 8 + 4 + 4 + 4 + 4
        );
        assert_eq!(
            InputEvent::decode(&[9]).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            InputEvent::decode(&[3, 0, 0]).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            InputEvent::decode(&[]).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            InputEvent::decode(&[
                1, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ])
            .unwrap_err()
            .kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn video_flags_roundtrip() {
        for (config, key) in [(false, false), (true, false), (false, true), (true, true)] {
            let p = VideoPacket {
                pts_us: 0x0102_0304_0506_0708,
                config,
                key,
                data: vec![0, 0, 0, 1, 0x65],
            };
            let b = p.encode();
            assert_eq!(b[8], (config as u8) | ((key as u8) << 1));
            assert_eq!(VideoPacket::decode(&b).unwrap(), p);
        }
        assert_eq!(
            VideoPacket::decode(&[0; 8]).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn async_frame_roundtrip() {
        let mut buf = Vec::new();
        for f in frames() {
            write_frame_async(&mut buf, &f).await.unwrap();
        }
        let mut sync_buf = Vec::new();
        for f in frames() {
            write_frame(&mut sync_buf, &f).unwrap();
        }
        assert_eq!(buf, sync_buf);
        let mut r = &buf[..];
        for f in frames() {
            assert_eq!(read_frame_async(&mut r).await.unwrap(), Some(f));
        }
        assert_eq!(read_frame_async(&mut r).await.unwrap(), None);
        let mut t: &[u8] = &[0, 0, 0, 10, 1, 0, 0];
        assert_eq!(
            read_frame_async(&mut t).await.unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }
}
