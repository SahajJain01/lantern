//! Text clipboard sync between the PC (Wayland `ext_data_control_v1`, [`wayland`]) and the
//! phone (`clip{text}` on stream 0; docs/protocol.md, "Text, clipboard, catalogue rules").
//!
//! [`ClipSync`] is the pure policy: what a PC change sends to the phone and what a phone change
//! puts on the PC. Clipboard text is never logged anywhere, only its length.

pub mod wayland;

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{mpsc as std_mpsc, Arc};

use lantern_proto::MAX_CLIP;

/// The text mime types Lantern reads and offers, most preferred first.
pub const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

/// KDE's password-manager hint (KeePassXC, Plasma's own password fields): an offer carrying it
/// whose content is `secret` must not leave the PC.
pub const SECRET_HINT_MIME: &str = "x-kde-passwordManagerHint";

/// One PC clipboard selection, as read by the Wayland thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcOffer {
    /// Every mime type the selection offers.
    pub mimes: Vec<String>,
    /// The text in the [`pick_mime`] type: `None` if there is none or it could not be read
    /// (timeout, invalid UTF-8). Text over [`MAX_CLIP`] bytes is kept over the limit (capped at
    /// `MAX_CLIP + 1` bytes read) so [`ClipSync::pc_changed`] rejects it.
    pub text: Option<String>,
    /// The offer carried [`SECRET_HINT_MIME`] = `secret` (or a hint that could not be read).
    pub secret: bool,
}

/// The text mime to read from an offer: the first of [`TEXT_MIMES`] it has.
pub fn pick_mime(mimes: &[String]) -> Option<&'static str> {
    TEXT_MIMES
        .iter()
        .copied()
        .find(|want| mimes.iter().any(|m| m == want))
}

/// Whether the [`SECRET_HINT_MIME`] content marks the selection secret.
pub fn is_secret_hint(content: &[u8]) -> bool {
    content.trim_ascii() == b"secret"
}

/// Selection bytes as text. Over [`MAX_CLIP`] the text only has to stay over the limit, so a cut
/// UTF-8 sequence is replaced (never shortening it); within it, invalid UTF-8 is not text.
pub fn decode_text(bytes: Vec<u8>) -> Option<String> {
    if bytes.len() > MAX_CLIP {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        String::from_utf8(bytes).ok()
    }
}

/// The PC side of clipboard sync: suppresses echoes of the phone's own text, secrets, empty and
/// oversize text. Only changes sync; nothing is pushed when a session starts.
#[derive(Debug, Default)]
pub struct ClipSync {
    /// The text last synced either way (the phone's last `clip`, or the last PC text sent).
    /// Putting the phone's text on the PC comes back as a selection, and Klipper may re-own the
    /// selection with the same text: neither goes back. Copying an older text again after the
    /// phone changed is still a change.
    last_from_phone: Option<String>,
}

impl ClipSync {
    /// The PC selection changed: the text to send to the phone, if any.
    pub fn pc_changed(&mut self, offer: &PcOffer) -> Option<String> {
        if offer.secret {
            return None;
        }
        let text = offer.text.as_ref()?;
        if text.is_empty() || text.len() > MAX_CLIP {
            return None;
        }
        if self.last_from_phone.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last_from_phone = Some(text.clone());
        Some(text.clone())
    }

    /// The phone sent `text`: remembers it and returns what to put on the PC clipboard.
    pub fn from_phone(&mut self, text: String) -> String {
        self.last_from_phone = Some(text.clone());
        text
    }
}

/// The daemon's PC clipboard: where to put phone text, and the PC's selections.
pub struct PcClipboard {
    pub handle: ClipHandle,
    pub offers: tokio::sync::mpsc::Receiver<PcOffer>,
}

/// Puts text on the PC clipboard (owned by the [`wayland`] thread). Never blocks.
#[derive(Clone)]
pub struct ClipHandle {
    tx: std_mpsc::Sender<String>,
    /// Write end of the thread's wake pipe (non-blocking), if any.
    wake: Option<Arc<OwnedFd>>,
}

impl ClipHandle {
    /// Sets the PC clipboard selection to `text` (the latest call wins if several queue up).
    pub fn set(&self, text: String) {
        if self.tx.send(text).is_err() {
            return;
        }
        if let Some(wake) = &self.wake {
            // SAFETY: a 1-byte write from a valid buffer to an fd we own; a full pipe
            // (EAGAIN) already guarantees a wakeup.
            unsafe { libc::write(wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        }
    }

    /// A handle whose texts arrive on the returned receiver (tests, no Wayland).
    pub fn test_pair() -> (Self, std_mpsc::Receiver<String>) {
        let (tx, rx) = std_mpsc::channel();
        (Self { tx, wake: None }, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lantern_proto::MAX_CLIP;

    fn offer(text: Option<&str>, secret: bool) -> PcOffer {
        PcOffer {
            mimes: vec!["text/plain;charset=utf-8".into()],
            text: text.map(str::to_owned),
            secret,
        }
    }

    #[test]
    fn clip_sync_sends_pc_copies() {
        let mut sync = ClipSync::default();
        assert_eq!(sync.pc_changed(&offer(Some("a"), false)), Some("a".into()));
        assert_eq!(sync.pc_changed(&offer(None, false)), None);
        assert_eq!(sync.pc_changed(&offer(Some(""), false)), None);
    }

    #[test]
    fn clip_sync_no_echo() {
        let mut sync = ClipSync::default();
        assert_eq!(sync.from_phone("from-phone".into()), "from-phone");
        // Setting the PC clipboard comes back as a selection of our own text: not sent back.
        assert_eq!(sync.pc_changed(&offer(Some("from-phone"), false)), None);
        assert_eq!(sync.pc_changed(&offer(Some("from-phone"), false)), None);
        assert_eq!(
            sync.pc_changed(&offer(Some("pc-copy"), false)),
            Some("pc-copy".into())
        );
        // After a PC copy, copying the phone's older text again is a real change.
        assert_eq!(
            sync.pc_changed(&offer(Some("from-phone"), false)),
            Some("from-phone".into())
        );
    }

    /// Klipper re-owning the selection repeats the same text: sent once.
    #[test]
    fn clip_sync_repeated_pc_selection_sent_once() {
        let mut sync = ClipSync::default();
        assert_eq!(sync.pc_changed(&offer(Some("a"), false)), Some("a".into()));
        assert_eq!(sync.pc_changed(&offer(Some("a"), false)), None);
        sync.from_phone("b".into());
        assert_eq!(sync.pc_changed(&offer(Some("a"), false)), Some("a".into()));
    }

    #[test]
    fn clip_sync_skips_kde_secret() {
        let mut sync = ClipSync::default();
        assert_eq!(sync.pc_changed(&offer(Some("hunter2"), true)), None);
        assert_eq!(sync.pc_changed(&offer(None, true)), None);
    }

    #[test]
    fn clip_sync_skips_oversize() {
        let mut sync = ClipSync::default();
        let max = "a".repeat(MAX_CLIP);
        assert_eq!(
            sync.pc_changed(&offer(Some(&max), false)),
            Some(max.clone())
        );
        let over = format!("é{}", "a".repeat(MAX_CLIP - 1));
        assert_eq!(over.len(), MAX_CLIP + 1);
        assert_eq!(sync.pc_changed(&offer(Some(&over), false)), None);
    }

    #[test]
    fn clip_sync_prefers_utf8_mime() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            pick_mime(&m(&[
                "text/plain",
                "UTF8_STRING",
                "text/plain;charset=utf-8"
            ])),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(
            pick_mime(&m(&["text/html", "text/plain", "UTF8_STRING"])),
            Some("UTF8_STRING")
        );
        assert_eq!(
            pick_mime(&m(&["text/html", "text/plain"])),
            Some("text/plain")
        );
        assert_eq!(pick_mime(&m(&["image/png", "text/html"])), None);
        assert_eq!(pick_mime(&[]), None);
    }

    #[test]
    fn secret_hint_parses() {
        assert!(is_secret_hint(b"secret"));
        assert!(is_secret_hint(b"secret\n"));
        assert!(!is_secret_hint(b"public"));
        assert!(!is_secret_hint(b""));
    }

    #[test]
    fn decode_caps_text() {
        assert_eq!(decode_text(b"hi".to_vec()), Some("hi".into()));
        // Over the cap: still longer than MAX_CLIP, so pc_changed rejects it.
        let over = vec![b'a'; MAX_CLIP + 1];
        assert!(decode_text(over).unwrap().len() > MAX_CLIP);
        // Invalid UTF-8 within the cap is not text.
        assert_eq!(decode_text(vec![0xff, 0xfe]), None);
    }

    #[test]
    fn handle_set_reaches_the_receiver() {
        let (handle, rx) = ClipHandle::test_pair();
        handle.set("x".into());
        assert_eq!(rx.recv().unwrap(), "x");
    }
}
