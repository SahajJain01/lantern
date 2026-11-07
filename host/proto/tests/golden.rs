//! Golden wire vectors shared with the Java side (Task 2).
//!
//! Each `golden/*.bin` file is one complete encoded FRAME (u32 len, u8 kind, u32 stream, payload).
//! Regenerate with `LANTERN_WRITE_GOLDEN=1 cargo test -p lantern-proto --test golden`; otherwise the
//! test asserts the current encoding is byte-identical to the committed file.
//!
//! Fixed values:
//! - control_open.bin  : kind=Control(1), stream=0,
//!                       Control::Open { pkg: "com.android.settings", user: 0, w: 1280, h: 800, dpi: 160 }
//!                       payload = {"t":"open","pkg":"com.android.settings","user":0,"w":1280,"h":800,"dpi":160}
//! - input_pointer.bin : kind=Input(3), stream=1,
//!                       InputEvent::Pointer { action: Move(2), id: 7, x: 0.25, y: 0.75, buttons: 1, pressure: 0.5 }
//! - input_key.bin     : kind=Input(3), stream=1,
//!                       InputEvent::Key { action: Down(0), keycode: 29 (KEYCODE_A), meta: 1 (META_SHIFT_ON), repeat: 0 }
//! - video_key.bin     : kind=Video(2), stream=1,
//!                       VideoPacket { pts_us: 1_000_000, config: false, key: true,
//!                                     data: [00 00 00 01 65 88 84 00] }  (flags byte = 0x02)

use std::path::PathBuf;

use lantern_proto::*;

fn frame_bytes(kind: Kind, stream: u32, payload: Vec<u8>) -> Vec<u8> {
    let mut buf = Vec::new();
    write_frame(&mut buf, &Frame { kind, stream, payload }).unwrap();
    buf
}

fn vectors() -> Vec<(&'static str, Vec<u8>)> {
    let open = Control::Open { pkg: "com.android.settings".into(), user: 0, w: 1280, h: 800, dpi: 160 };
    let pointer = InputEvent::Pointer { action: PointerAction::Move, id: 7, x: 0.25, y: 0.75, buttons: 1, pressure: 0.5 };
    let key = InputEvent::Key { action: KeyAction::Down, keycode: 29, meta: 1, repeat: 0 };
    let video = VideoPacket { pts_us: 1_000_000, config: false, key: true, data: vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00] };
    vec![
        ("control_open", frame_bytes(Kind::Control, 0, serde_json::to_vec(&open).unwrap())),
        ("input_pointer", frame_bytes(Kind::Input, 1, pointer.encode())),
        ("input_key", frame_bytes(Kind::Input, 1, key.encode())),
        ("video_key", frame_bytes(Kind::Video, 1, video.encode())),
    ]
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("golden").join(format!("{name}.bin"))
}

#[test]
fn golden_vectors() {
    let write = std::env::var("LANTERN_WRITE_GOLDEN").as_deref() == Ok("1");
    for (name, bytes) in vectors() {
        let path = golden_path(name);
        if write {
            std::fs::write(&path, &bytes).unwrap();
            continue;
        }
        let want = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(bytes, want, "{name} encoding differs from {}", path.display());
    }
}

#[test]
fn golden_files_decode() {
    if std::env::var("LANTERN_WRITE_GOLDEN").as_deref() == Ok("1") {
        return; // files may be mid-rewrite by golden_vectors
    }
    for (name, _) in vectors() {
        let bytes = std::fs::read(golden_path(name)).unwrap();
        let f = read_frame(&mut &bytes[..]).unwrap().expect("one frame");
        match name {
            "control_open" => assert_eq!(
                serde_json::from_slice::<Control>(&f.payload).unwrap(),
                Control::Open { pkg: "com.android.settings".into(), user: 0, w: 1280, h: 800, dpi: 160 }
            ),
            "input_pointer" | "input_key" => {
                InputEvent::decode(&f.payload).unwrap();
            }
            "video_key" => assert!(VideoPacket::decode(&f.payload).unwrap().key),
            _ => unreachable!(),
        }
    }
}
