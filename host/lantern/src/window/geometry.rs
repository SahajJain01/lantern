//! Pure window/video geometry: letterboxing, pointer mapping, display requests, rotation.

/// A rectangle in window (physical) pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Aspect-fits `video` into `window`, centered (letterbox while a resize is in flight).
pub fn video_rect(window: (u32, u32), video: (u32, u32)) -> Rect {
    let (ww, wh) = (window.0 as f32, window.1 as f32);
    let (vw, vh) = (video.0.max(1) as f32, video.1.max(1) as f32);
    let scale = (ww / vw).min(wh / vh);
    let (w, h) = (vw * scale, vh * scale);
    Rect {
        x: ((ww - w) / 2.0).round(),
        y: ((wh - h) / 2.0).round(),
        w,
        h,
    }
}

/// Maps a window pixel to 0..1 of the video, `None` outside it.
pub fn to_normalized(rect: Rect, px: f64, py: f64) -> Option<(f32, f32)> {
    let (x, y) = clamp_normalized(rect, px, py);
    let inside = |v: f64, lo: f32, len: f32| v >= lo as f64 && v < (lo + len) as f64;
    (inside(px, rect.x, rect.w) && inside(py, rect.y, rect.h)).then_some((x, y))
}

/// Like [`to_normalized`] but clamped to the video edge (for drags that leave the video).
pub fn clamp_normalized(rect: Rect, px: f64, py: f64) -> (f32, f32) {
    let n = |v: f64, lo: f32, len: f32| {
        (((v - lo as f64) / len.max(1.0) as f64) as f32).clamp(0.0, 1.0)
    };
    (n(px, rect.x, rect.w), n(py, rect.y, rect.h))
}

/// The display to ask the phone for: physical pixels and density for a logical window size.
pub fn display_request(logical: (u32, u32), scale: f64) -> (u32, u32, u32) {
    use crate::daemon::ipc::limits;
    let px = |l: u32| {
        ((l as f64 * scale).round() as u32).clamp(*limits::SIZE.start(), *limits::SIZE.end())
    };
    let dpi = ((160.0 * scale).round() as u32)
        .max(120)
        .clamp(*limits::DPI.start(), *limits::DPI.end());
    (px(logical.0), px(logical.1), dpi)
}

/// Whether two sizes' aspect ratios differ by more than 2%.
pub fn aspect_differs(a: (u32, u32), b: (u32, u32)) -> bool {
    let ra = a.0.max(1) as f64 / a.1.max(1) as f64;
    let rb = b.0.max(1) as f64 / b.1.max(1) as f64;
    (ra / rb - 1.0).abs() > 0.02
}

/// An unrequested `Resized` (app rotation): the logical window size to ask for, keeping the
/// window height. `None` when the aspects already match within 2%.
pub fn rotation_request(window: (u32, u32), resized: (u32, u32)) -> Option<(u32, u32)> {
    if !aspect_differs(window, resized) {
        return None;
    }
    let h = window.1.max(1);
    let w = (h as f64 * resized.0.max(1) as f64 / resized.1.max(1) as f64).round() as u32;
    Some((w.max(1), h))
}

/// How a phone `Resized` relates to the sizes this window asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizedKind {
    /// The echo of one of our outstanding requests (possibly late, possibly encoder-aligned).
    Requested,
    /// The app rotated: the phone's aspect is the inverse of a requested one.
    Rotation,
    /// Neither: keep the window and letterbox.
    Unrequested,
}

/// The display sizes recently asked of the phone (`open`/`resize`), oldest first.
#[derive(Debug, Default)]
pub struct ResizeRequests {
    recent: std::collections::VecDeque<(u32, u32)>,
}

impl ResizeRequests {
    pub const CAP: usize = 8;

    /// Records a size just asked for. Only the last [`Self::CAP`] are kept.
    pub fn push(&mut self, size: (u32, u32)) {
        if self.recent.len() == Self::CAP {
            self.recent.pop_front();
        }
        self.recent.push_back(size);
    }

    /// The size asked for most recently.
    pub fn latest(&self) -> Option<(u32, u32)> {
        self.recent.back().copied()
    }

    /// Classifies a phone `Resized`. An echo of an outstanding request (within 2% aspect)
    /// retires that request and every older one: the phone answers in order, so they are
    /// stale. Only a size whose aspect is the inverse of a request counts as a rotation.
    pub fn classify(&mut self, resized: (u32, u32)) -> ResizedKind {
        if let Some(i) = self
            .recent
            .iter()
            .rposition(|&r| !aspect_differs(r, resized))
        {
            self.recent.drain(..i);
            return ResizedKind::Requested;
        }
        let rotated = self
            .recent
            .iter()
            .any(|&(w, h)| !aspect_differs((h, w), resized));
        if rotated {
            ResizedKind::Rotation
        } else {
            ResizedKind::Unrequested
        }
    }

    /// The phone rotated to `size`: it is now the only size we stand by, so that rotating
    /// back is not mistaken for an echo of a pre-rotation request.
    pub fn rotated(&mut self, size: (u32, u32)) {
        self.recent.clear();
        self.recent.push_back(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_letterbox_wide_video_in_tall_window() {
        let r = video_rect((800, 1280), (1280, 800));
        assert_eq!(
            r,
            Rect {
                x: 0.0,
                y: 390.0,
                w: 800.0,
                h: 500.0
            }
        );
        // Exact fit and pillarbox.
        assert_eq!(
            video_rect((1280, 800), (1280, 800)),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1280.0,
                h: 800.0
            }
        );
        assert_eq!(
            video_rect((1600, 800), (800, 800)),
            Rect {
                x: 400.0,
                y: 0.0,
                w: 800.0,
                h: 800.0
            }
        );
    }

    #[test]
    fn geometry_outside_returns_none() {
        let r = video_rect((800, 1280), (1280, 800));
        assert_eq!(to_normalized(r, 10.0, 10.0), None);
        assert_eq!(to_normalized(r, 10.0, 1000.0), None);
        assert_eq!(to_normalized(r, -1.0, 500.0), None);
        assert_eq!(to_normalized(r, 0.0, 390.0), Some((0.0, 0.0)));
        assert_eq!(to_normalized(r, 400.0, 640.0), Some((0.5, 0.5)));
    }

    #[test]
    fn display_request_scale_1_25() {
        assert_eq!(display_request((1280, 800), 1.25), (1600, 1000, 200));
        assert_eq!(display_request((1280, 800), 1.0), (1280, 800, 160));
        // Density floor and helper limits.
        assert_eq!(display_request((100, 100), 0.5), (50, 50, 120));
        assert_eq!(display_request((0, 20000), 1.0), (1, 16384, 160));
    }

    #[test]
    fn rotation_swaps_window_keeping_long_edge() {
        assert_eq!(rotation_request((1280, 800), (800, 1280)), Some((500, 800)));
        assert_eq!(rotation_request((1280, 800), (1600, 1000)), None);
        // Within 2%: not a rotation.
        assert_eq!(rotation_request((1280, 800), (1296, 800)), None);
    }

    #[test]
    fn stale_resized_during_drag_is_not_a_rotation() {
        let mut r = ResizeRequests::default();
        r.push((1600, 1000)); // A, 1.6
        r.push((1000, 1000)); // B, 1.0: sent before the phone answered A
        assert_eq!(r.latest(), Some((1000, 1000)));
        // The late echo of A must not look like a rotation against B.
        assert_eq!(r.classify((1600, 1000)), ResizedKind::Requested);
        assert_eq!(r.classify((1000, 1000)), ResizedKind::Requested);
        // Once B is answered, A is no longer outstanding.
        assert_eq!(r.classify((1600, 1000)), ResizedKind::Unrequested);
        // Encoder alignment stays within 2%.
        r.push((1282, 802));
        assert_eq!(r.classify((1280, 800)), ResizedKind::Requested);
    }

    #[test]
    fn rotation_is_the_inverse_aspect_of_a_request() {
        let mut r = ResizeRequests::default();
        r.push((1280, 800));
        assert_eq!(r.classify((800, 1280)), ResizedKind::Rotation);
        // Some other aspect is neither ours nor a rotation.
        assert_eq!(r.classify((1000, 1000)), ResizedKind::Unrequested);
        // After acting on a rotation, rotating back is a rotation again (not a stale echo).
        r.rotated((800, 1280));
        assert_eq!(r.latest(), Some((800, 1280)));
        assert_eq!(r.classify((800, 1280)), ResizedKind::Requested);
        assert_eq!(r.classify((1280, 800)), ResizedKind::Rotation);
    }

    #[test]
    fn resize_requests_are_bounded() {
        let mut r = ResizeRequests::default();
        for i in 0..100 {
            r.push((1000 + i, 1000));
        }
        assert!(r.recent.len() <= ResizeRequests::CAP);
        assert_eq!(r.latest(), Some((1099, 1000)));
    }
}
