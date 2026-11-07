//! Decodes the Task 4 capture (`tests/fixtures/settings.h265`, 2 s of Settings at 1280x800).

use lantern::window::decode::{Backend, Colour, Decoder, Matrix, Nv12Frame};
use lantern_proto::VideoPacket;

/// Splits an H.265 Annex-B stream into helper-style packets: one config packet (VPS/SPS/PPS)
/// and then one packet per access unit.
fn packets(stream: &[u8]) -> Vec<VideoPacket> {
    // (start of the start code, first NAL byte) for every NAL.
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            let s = if i > 0 && stream[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            starts.push((s, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out: Vec<VideoPacket> = Vec::new();
    let mut config = Vec::new();
    let mut au: Vec<u8> = Vec::new();
    let mut au_key = false;
    let mut pts = 0u64;
    for (n, &(s, payload)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(stream.len(), |next| next.0);
        let nal = &stream[s..end];
        let ty = (stream[payload] >> 1) & 0x3f;
        match ty {
            32..=34 => config.extend_from_slice(nal),
            0..=31 => {
                let first_slice = stream[payload + 2] & 0x80 != 0;
                if first_slice && !au.is_empty() {
                    out.push(VideoPacket {
                        pts_us: pts,
                        config: false,
                        key: au_key,
                        data: std::mem::take(&mut au),
                    });
                    pts += 16_666;
                }
                if !config.is_empty() {
                    out.push(VideoPacket {
                        pts_us: 0,
                        config: true,
                        key: false,
                        data: std::mem::take(&mut config),
                    });
                }
                au_key = (16..=23).contains(&ty);
                au.extend_from_slice(nal);
            }
            _ => au.extend_from_slice(nal),
        }
    }
    if !au.is_empty() {
        out.push(VideoPacket {
            pts_us: pts,
            config: false,
            key: au_key,
            data: au,
        });
    }
    out
}

fn decode_all(d: &mut Decoder) -> Vec<Nv12Frame> {
    let stream = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/settings.h265"
    ))
    .unwrap();
    let pkts = packets(&stream);
    assert!(pkts[0].config, "fixture starts with VPS/SPS/PPS");
    assert!(pkts.len() > 100, "{} packets", pkts.len());
    let mut frames = Vec::new();
    for p in &pkts {
        frames.extend(d.push(p).unwrap());
    }
    frames
}

fn check(frames: &[Nv12Frame]) {
    assert!(!frames.is_empty());
    for f in frames {
        assert_eq!((f.w, f.h), (1280, 800));
        assert!(f.y_stride >= 1280 && f.uv_stride >= 1280);
        assert_eq!(f.y.len(), (f.y_stride * f.h) as usize);
        assert_eq!(f.uv.len(), (f.uv_stride * f.h / 2) as usize);
        // The phone signals BT.601 (smpte170m/bt470bg), TV range; the frame must carry it
        // (VA-API too: the colour comes from the surface frame, not the transfer copy).
        let bt601_tv = Colour {
            matrix: Matrix::Bt601,
            full_range: false,
        };
        assert_eq!(f.colour, bt601_tv);
    }
    // Real picture content, not a blank buffer.
    let y = &frames[frames.len() - 1].y;
    let (lo, hi) = (y.iter().min().unwrap(), y.iter().max().unwrap());
    assert!(hi - lo > 100, "luma range {lo}..{hi}");
}

#[test]
fn decode_fixture_software() {
    let mut d = Decoder::with_backend("h265", Backend::Software).unwrap();
    let frames = decode_all(&mut d);
    assert_eq!(d.backend(), Backend::Software);
    check(&frames);
    assert!(frames.len() >= 120, "{} frames", frames.len());
}

/// VA-API when the machine has it (this host's Intel GPU does); skipped otherwise.
#[test]
fn decode_fixture_vaapi_if_available() {
    let Ok(mut d) = Decoder::with_backend("h265", Backend::Vaapi) else {
        eprintln!("no VA-API; skipping");
        return;
    };
    check(&decode_all(&mut d));
    assert_eq!(
        d.backend(),
        Backend::Vaapi,
        "frames came from VA-API surfaces"
    );
}

#[test]
fn unknown_codec_is_an_error() {
    assert!(Decoder::new("vp9").is_err());
}
