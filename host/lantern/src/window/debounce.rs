//! Resize debouncing: emit the latest size once it has been stable for `quiet`, and at most
//! every [`MAX_INTERVAL`] during a continuous drag.

use std::time::{Duration, Instant};

pub const QUIET: Duration = Duration::from_millis(120);
pub const MAX_INTERVAL: Duration = Duration::from_millis(500);

pub struct Debouncer {
    pub quiet: Duration,
    pending: Option<(u32, u32)>,
    /// When the pending size last changed.
    last_push: Option<Instant>,
    /// First push since the last emission (the drag interval is measured from here).
    burst_start: Option<Instant>,
}

impl Debouncer {
    pub fn new() -> Self {
        Self {
            quiet: QUIET,
            pending: None,
            last_push: None,
            burst_start: None,
        }
    }

    pub fn push(&mut self, now: Instant, size: (u32, u32)) {
        if self.pending.is_none() {
            self.burst_start = Some(now);
        }
        self.pending = Some(size);
        self.last_push = Some(now);
    }

    pub fn due(&mut self, now: Instant) -> Option<(u32, u32)> {
        if now < self.deadline()? {
            return None;
        }
        self.burst_start = None;
        self.last_push = None;
        self.pending.take()
    }

    /// When `due` will next return a size, if anything is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending?;
        let quiet = self.last_push? + self.quiet;
        let drag = self.burst_start? + MAX_INTERVAL;
        Some(quiet.min(drag))
    }
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn debounce_emits_final_size_after_quiet() {
        let t0 = Instant::now();
        let mut d = Debouncer::new();
        assert_eq!(d.quiet, ms(120));
        assert_eq!(d.due(t0), None);
        d.push(t0, (100, 100));
        d.push(t0 + ms(50), (110, 100));
        assert_eq!(d.due(t0 + ms(100)), None);
        assert_eq!(d.deadline(), Some(t0 + ms(170)));
        assert_eq!(d.due(t0 + ms(169)), None);
        assert_eq!(d.due(t0 + ms(170)), Some((110, 100)));
        assert_eq!(d.due(t0 + ms(1000)), None);
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn debounce_emits_during_long_drag_every_500ms() {
        let t0 = Instant::now();
        let mut d = Debouncer::new();
        let mut emitted = Vec::new();
        // A drag pushing a new size every 16 ms for 1.6 s, polled every 1 ms.
        for t in 0..=1600u64 {
            let now = t0 + ms(t);
            if t % 16 == 0 {
                d.push(now, (1000 + t as u32, 800));
            }
            if let Some(s) = d.due(now) {
                emitted.push((t, s));
            }
        }
        let times: Vec<u64> = emitted.iter().map(|e| e.0).collect();
        assert_eq!(times, vec![500, 1012, 1524], "{emitted:?}");
        for w in times.windows(2) {
            assert!(w[1] - w[0] >= 500);
        }
        // The final size still arrives after the drag stops.
        let mut last = None;
        for t in 1601..2000u64 {
            if let Some(s) = d.due(t0 + ms(t)) {
                last = Some((t, s));
            }
        }
        assert_eq!(last, Some((1600 + 120, (2600, 800))));
    }
}
