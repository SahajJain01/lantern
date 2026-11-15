//! Text clipboard sync between the PC (Wayland `ext_data_control_v1`, [`wayland`]) and the
//! phone (`clip{text}` on stream 0; docs/protocol.md, "Text, clipboard, catalogue rules").
//!
//! [`ClipSync`] is the pure policy: what a PC change sends to the phone and what a phone change
//! puts on the PC. Clipboard text is never logged anywhere, only its length.

pub mod wayland;

use std::collections::{BTreeMap, VecDeque};
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
    /// The phone's last `clip`. Putting it on the PC comes back as a selection (and Klipper may
    /// re-own it): not sent back. The PC's own sent text is not remembered, and any other PC
    /// change, synced or not (secret, oversize, no text), clears this, so the phone's text copied
    /// again later is sent.
    last_from_phone: Option<String>,
}

impl ClipSync {
    /// The PC selection changed: the text to send to the phone, if any.
    pub fn pc_changed(&mut self, offer: &PcOffer) -> Option<String> {
        let text = match &offer.text {
            Some(t) if !offer.secret && !t.is_empty() && t.len() <= MAX_CLIP => t,
            _ => {
                self.last_from_phone = None;
                return None;
            }
        };
        if self.last_from_phone.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last_from_phone = None;
        Some(text.clone())
    }

    /// The phone sent `text`: remembers it and returns what to put on the PC clipboard.
    pub fn from_phone(&mut self, text: String) -> String {
        self.last_from_phone = Some(text.clone());
        text
    }
}

/// Phone texts Lantern put on the PC whose selection event may still come back.
pub const IN_FLIGHT_MAX: usize = 8;

/// Clipboard sync for several phones at once. A phone copy goes onto the PC selection; the
/// other phones get it when that selection comes back ([`Self::pc_changed`]), never its source.
/// Each phone has its own [`ClipSync`] memory; the in-flight list keeps a stale echo (a burst:
/// both phones copied before the first copy's selection came back) from going anywhere.
#[derive(Debug, Default)]
pub struct ClipFan {
    /// One echo memory per phone with a live session (serial order).
    phones: BTreeMap<String, ClipSync>,
    /// Phone texts put on the PC, oldest first, at most [`IN_FLIGHT_MAX`].
    in_flight: VecDeque<String>,
}

impl ClipFan {
    /// `serial`'s session is up: a fresh memory.
    pub fn add(&mut self, serial: &str) {
        self.phones.insert(serial.to_owned(), ClipSync::default());
    }

    /// `serial`'s session is down.
    pub fn remove(&mut self, serial: &str) {
        self.phones.remove(serial);
    }

    /// Clipboard sync is back on: every memory and the in-flight list may be stale.
    pub fn reset(&mut self) {
        for sync in self.phones.values_mut() {
            *sync = ClipSync::default();
        }
        self.in_flight.clear();
    }

    /// Phone `serial` copied `text`: what to put on the PC clipboard.
    pub fn from_phone(&mut self, serial: &str, text: String) -> String {
        if let Some(sync) = self.phones.get_mut(serial) {
            sync.from_phone(text.clone());
        }
        self.in_flight.push_back(text.clone());
        while self.in_flight.len() > IN_FLIGHT_MAX {
            self.in_flight.pop_front();
        }
        text
    }

    /// The PC selection changed: (serial, text) for each phone it goes to, in serial order.
    /// The echo of an older phone text while a newer one is still in flight goes nowhere and
    /// changes no memory; an unsyncable offer clears every memory and sends nothing.
    pub fn pc_changed(&mut self, offer: &PcOffer) -> Vec<(String, String)> {
        let syncable = !offer.secret
            && offer
                .text
                .as_ref()
                .is_some_and(|t| !t.is_empty() && t.len() <= MAX_CLIP);
        if syncable {
            let text = offer.text.as_deref().unwrap_or_default();
            if let Some(i) = self.in_flight.iter().position(|t| t == text) {
                self.in_flight.drain(..=i);
                if !self.in_flight.is_empty() {
                    return Vec::new();
                }
            }
        }
        self.phones
            .iter_mut()
            .filter_map(|(serial, sync)| sync.pc_changed(offer).map(|text| (serial.clone(), text)))
            .collect()
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

    /// Only the phone's text is suppressed: the PC's own sent text is not remembered, so a
    /// re-copy of it is sent (the phone may hold something else by then).
    #[test]
    fn clip_sync_pc_recopy_is_sent() {
        let mut sync = ClipSync::default();
        assert_eq!(sync.pc_changed(&offer(Some("a"), false)), Some("a".into()));
        assert_eq!(sync.pc_changed(&offer(Some("a"), false)), Some("a".into()));
    }

    /// Phone X -> PC, then an unsyncable PC change (secret, oversize, no text), then the phone's
    /// X copied again on the PC: sent, since the phone may now hold something else.
    #[test]
    fn clip_sync_unsyncable_pc_change_resets_echo() {
        let over = "a".repeat(MAX_CLIP + 1);
        let unsyncable = [
            offer(Some("hunter2"), true),
            offer(Some(&over), false),
            offer(None, false),
            offer(Some(""), false),
        ];
        for (i, bad) in unsyncable.iter().enumerate() {
            let mut sync = ClipSync::default();
            sync.from_phone("x".into());
            assert_eq!(
                sync.pc_changed(&offer(Some("x"), false)),
                None,
                "echo, case {i}"
            );
            assert_eq!(sync.pc_changed(bad), None, "unsyncable, case {i}");
            assert_eq!(
                sync.pc_changed(&offer(Some("x"), false)),
                Some("x".into()),
                "re-copy after unsyncable, case {i}"
            );
        }
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

    fn fan(serials: &[&str]) -> ClipFan {
        let mut fan = ClipFan::default();
        for serial in serials {
            fan.add(serial);
        }
        fan
    }

    fn to(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(s, t)| (s.to_string(), t.to_string()))
            .collect()
    }

    #[test]
    fn fan_pc_copy_goes_to_every_phone() {
        let mut fan = fan(&["A", "B"]);
        assert_eq!(
            fan.pc_changed(&offer(Some("x"), false)),
            to(&[("A", "x"), ("B", "x")])
        );
    }

    #[test]
    fn fan_phone_copy_goes_to_the_others_not_its_source() {
        let mut fan = fan(&["A", "B"]);
        assert_eq!(fan.from_phone("A", "x".into()), "x");
        assert_eq!(fan.pc_changed(&offer(Some("x"), false)), to(&[("B", "x")]));
    }

    /// Both phones copy before the first copy's PC echo is back: the stale echo goes nowhere
    /// (it would overwrite B's newer copy), and no phone gets its own copy back.
    #[test]
    fn burst_never_echoes_to_source() {
        let mut fan = fan(&["A", "B"]);
        fan.from_phone("A", "x".into());
        fan.from_phone("B", "y".into());
        assert_eq!(fan.pc_changed(&offer(Some("x"), false)), to(&[]));
        assert_eq!(fan.pc_changed(&offer(Some("y"), false)), to(&[("A", "y")]));
    }

    #[test]
    fn fan_secret_reaches_nobody_and_clears_memories() {
        let mut fan = fan(&["A", "B"]);
        fan.from_phone("A", "x".into());
        assert_eq!(fan.pc_changed(&offer(Some("x"), true)), to(&[]));
        assert_eq!(fan.pc_changed(&offer(None, false)), to(&[]));
        // A's memory of x is gone: a later x is a real change for every phone.
        assert_eq!(
            fan.pc_changed(&offer(Some("x"), false)),
            to(&[("A", "x"), ("B", "x")])
        );
    }

    #[test]
    fn fan_removed_phone_gets_nothing() {
        let mut fan = fan(&["A", "B"]);
        fan.remove("B");
        assert_eq!(fan.pc_changed(&offer(Some("x"), false)), to(&[("A", "x")]));
        // A phone that comes back starts with no memory.
        fan.from_phone("A", "y".into());
        fan.add("B");
        assert_eq!(fan.pc_changed(&offer(Some("y"), false)), to(&[("B", "y")]));
    }

    #[test]
    fn fan_reset_forgets_echoes() {
        let mut fan = fan(&["A", "B"]);
        fan.from_phone("A", "x".into());
        fan.from_phone("B", "y".into());
        fan.reset();
        assert_eq!(
            fan.pc_changed(&offer(Some("x"), false)),
            to(&[("A", "x"), ("B", "x")])
        );
    }

    #[test]
    fn in_flight_is_capped() {
        let mut fan = fan(&["A", "B"]);
        for i in 0..=IN_FLIGHT_MAX {
            let serial = if i % 2 == 0 { "A" } else { "B" };
            fan.from_phone(serial, format!("t{i}"));
        }
        // t0 is no longer in flight: a real change for every phone.
        assert_eq!(
            fan.pc_changed(&offer(Some("t0"), false)),
            to(&[("A", "t0"), ("B", "t0")])
        );
    }

    #[test]
    fn handle_set_reaches_the_receiver() {
        let (handle, rx) = ClipHandle::test_pair();
        handle.set("x".into());
        assert_eq!(rx.recv().unwrap(), "x");
    }
}
