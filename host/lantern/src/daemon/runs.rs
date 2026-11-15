//! Every wanted phone's run: its session (or its start in flight), its retry backoff and its
//! unplug grace. One phone's run never touches another's; the run loop (`daemon::drive`)
//! polls them all through [`Runs::next`].

use std::collections::BTreeMap;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use lantern_proto::PhoneState;
use tokio::time::Instant;

use super::session::{Inbound, SessionConn};

/// One wanted phone: what runs for it now.
pub struct PhoneRun<F> {
    /// Its live session and when it became ready.
    pub active: Option<(SessionConn, Instant)>,
    /// Its session start in flight. Dropping it kills the helper and removes the forward.
    pub starting: Option<Pin<Box<F>>>,
    /// When its next start is due (while it has neither a session nor a start).
    pub retry_at: Option<Instant>,
    /// Consecutive failed starts or helper deaths (the backoff).
    pub failures: u32,
    /// Offline while wanted: the end of its unplug grace.
    pub grace: Option<Instant>,
}

impl<F> PhoneRun<F> {
    fn new() -> Self {
        Self {
            active: None,
            starting: None,
            retry_at: Some(Instant::now()),
            failures: 0,
            grace: None,
        }
    }

    /// Drops its start in flight, if any (`why` is logged).
    pub fn cancel_start(&mut self, serial: &str, why: &str) {
        if let Some(future) = self.starting.take() {
            // Dropping the start future drops its PendingSession, which kills the helper and
            // removes the forward.
            drop(future);
            tracing::info!(serial, why, "session start cancelled");
        }
    }

    /// Its state as the view shows it.
    pub fn state(&self) -> PhoneState {
        if self.active.is_some() {
            PhoneState::Ready
        } else if self.starting.is_some() {
            PhoneState::Starting
        } else if self.grace.is_some() {
            PhoneState::Reconnecting
        } else if self.failures > 0 {
            PhoneState::Retrying
        } else {
            // A start due now is as good as starting.
            PhoneState::Starting
        }
    }

    /// When it next needs the loop: its due start (only with nothing running) or its grace end.
    fn wake(&self) -> Option<Instant> {
        let retry = self
            .retry_at
            .filter(|_| self.active.is_none() && self.starting.is_none());
        match (retry, self.grace) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// What [`Runs::next`] yields. Short-lived (one per loop turn), so its size doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum RunEvent {
    /// A phone's session start finished.
    Started(String, Result<SessionConn>),
    /// A frame from a phone's helper, or why its session ended.
    Inbound(String, Inbound),
}

/// The wanted phones' runs, by serial.
pub struct Runs<F> {
    runs: BTreeMap<String, PhoneRun<F>>,
    /// Where the next poll starts, so a busy phone never starves the others.
    cursor: usize,
}

impl<F> Default for Runs<F> {
    fn default() -> Self {
        Self {
            runs: BTreeMap::new(),
            cursor: 0,
        }
    }
}

impl<F: Future<Output = Result<SessionConn>>> Runs<F> {
    pub fn new() -> Self {
        Self::default()
    }

    /// `serial`'s run; a new one has its first start due now.
    pub fn want(&mut self, serial: &str) -> &mut PhoneRun<F> {
        self.runs
            .entry(serial.to_owned())
            .or_insert_with(PhoneRun::new)
    }

    pub fn is_wanted(&self, serial: &str) -> bool {
        self.runs.contains_key(serial)
    }

    pub fn get(&self, serial: &str) -> Option<&PhoneRun<F>> {
        self.runs.get(serial)
    }

    pub fn get_mut(&mut self, serial: &str) -> Option<&mut PhoneRun<F>> {
        self.runs.get_mut(serial)
    }

    /// Forgets `serial`'s run and hands it back (its session and start are the caller's to
    /// end). Every other run is untouched.
    pub fn release(&mut self, serial: &str) -> Option<PhoneRun<F>> {
        self.runs.remove(serial)
    }

    /// The wanted phones, in serial order.
    pub fn serials(&self) -> Vec<String> {
        self.runs.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// `serial`'s state while it is wanted: Ready, Starting, Reconnecting or Retrying.
    pub fn state(&self, serial: &str) -> Option<PhoneState> {
        self.runs.get(serial).map(PhoneRun::state)
    }

    /// The earliest due start (of a run with nothing running) or grace end.
    pub fn next_wake(&self) -> Option<Instant> {
        self.runs.values().filter_map(PhoneRun::wake).min()
    }

    /// Polls every run from the cursor on; `pick` returns `Some` for the run that is ready.
    fn poll_each<T>(
        &mut self,
        cx: &mut Context<'_>,
        mut pick: impl FnMut(&str, &mut PhoneRun<F>, &mut Context<'_>) -> Option<T>,
    ) -> Poll<T> {
        let n = self.runs.len();
        for k in 0..n {
            let i = (self.cursor + k) % n;
            let (serial, run) = self.runs.iter_mut().nth(i).expect("i < len");
            if let Some(found) = pick(serial, run, cx) {
                self.cursor = i + 1;
                return Poll::Ready(found);
            }
        }
        Poll::Pending
    }

    fn poll_started(&mut self, cx: &mut Context<'_>) -> Poll<(String, Result<SessionConn>)> {
        self.poll_each(cx, |serial, run, cx| {
            let future = run.starting.as_mut()?;
            match future.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    run.starting = None;
                    Some((serial.to_owned(), result))
                }
                Poll::Pending => None,
            }
        })
    }

    fn poll_inbound(&mut self, cx: &mut Context<'_>) -> Poll<(String, Inbound)> {
        self.poll_each(cx, |serial, run, cx| {
            let (session, _) = run.active.as_mut()?;
            match session.poll_next(cx) {
                Poll::Ready(inbound) => Some((serial.to_owned(), inbound)),
                Poll::Pending => None,
            }
        })
    }

    /// The next finished start. Cancel-safe: the start futures stay in their runs. Pending
    /// while none finishes (or none runs).
    pub async fn next_started(&mut self) -> (String, Result<SessionConn>) {
        poll_fn(|cx| self.poll_started(cx)).await
    }

    /// The next frame from any live session, tagged with its phone. Cancel-safe. Pending while
    /// no session has one (or none is live).
    pub async fn next_inbound(&mut self) -> (String, Inbound) {
        poll_fn(|cx| self.poll_inbound(cx)).await
    }

    /// [`Self::next_started`] and [`Self::next_inbound`] in one future (the run loop's select
    /// can borrow `self` only once).
    pub async fn next(&mut self) -> RunEvent {
        poll_fn(|cx| {
            if let Poll::Ready((serial, result)) = self.poll_started(cx) {
                return Poll::Ready(RunEvent::Started(serial, result));
            }
            self.poll_inbound(cx)
                .map(|(serial, inbound)| RunEvent::Inbound(serial, inbound))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::Adb;
    use futures_util::future::BoxFuture;
    use lantern_proto::{write_frame_async, Frame, Kind};
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    type Start = BoxFuture<'static, Result<SessionConn>>;

    /// A session for `serial` over a local TCP pair, and the helper's end.
    async fn pair(serial: &str) -> (SessionConn, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ours = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (theirs, _) = listener.accept().await.unwrap();
        (SessionConn::attach(ours, Adb::new(serial), "sha"), theirs)
    }

    fn frame(byte: u8) -> Frame {
        Frame {
            kind: Kind::Video,
            stream: 1,
            payload: vec![byte; 64],
        }
    }

    fn live(runs: &mut Runs<Start>, serial: &str, session: SessionConn) {
        let run = runs.want(serial);
        run.retry_at = None;
        run.active = Some((session, Instant::now()));
    }

    #[tokio::test]
    async fn inbound_from_every_session() {
        let mut runs = Runs::<Start>::new();
        let (a, mut a_peer) = pair("a").await;
        let (b, mut b_peer) = pair("b").await;
        live(&mut runs, "a", a);
        live(&mut runs, "b", b);
        // a's helper is busy: it never stops writing.
        let flood = tokio::spawn(async move {
            loop {
                if write_frame_async(&mut a_peer, &frame(1)).await.is_err() {
                    return;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_frame_async(&mut b_peer, &frame(2)).await.unwrap();
        let (mut from_a, mut from_b) = (0, 0);
        timeout(Duration::from_secs(1), async {
            while from_a == 0 || from_b == 0 {
                match runs.next_inbound().await {
                    (serial, Inbound::Frame(f)) => {
                        let byte = f.payload[0];
                        match serial.as_str() {
                            "a" => {
                                assert_eq!(byte, 1);
                                from_a += 1
                            }
                            "b" => {
                                assert_eq!(byte, 2);
                                from_b += 1
                            }
                            other => panic!("frame tagged {other}"),
                        }
                    }
                    (serial, Inbound::Ended(why)) => panic!("{serial} ended: {why}"),
                }
            }
        })
        .await
        .expect("both phones' frames within 1 s");
        flood.abort();
        // The end of b's session comes tagged too.
        drop(b_peer);
        let ended = timeout(Duration::from_secs(1), async {
            loop {
                if let (serial, Inbound::Ended(_)) = runs.next_inbound().await {
                    return serial;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(ended, "b");
    }

    #[tokio::test]
    async fn next_started_reports_its_phone() {
        let mut runs = Runs::<Start>::new();
        let (b, _b_peer) = pair("b").await;
        runs.want("a").starting = Some(Box::pin(Box::pin(std::future::pending())));
        runs.want("b").starting = Some(Box::pin(Box::pin(async move { Ok(b) })));
        let (serial, result) = timeout(Duration::from_secs(1), runs.next_started())
            .await
            .unwrap();
        assert_eq!(serial, "b");
        assert_eq!(result.unwrap().serial(), "b");
        assert!(runs.get("a").unwrap().starting.is_some(), "a's start stays");
        assert!(runs.get("b").unwrap().starting.is_none());
        // Nothing else finishes; dropping the call keeps a's start in its run.
        assert!(timeout(Duration::from_millis(100), runs.next_started())
            .await
            .is_err());
        assert!(runs.get("a").unwrap().starting.is_some());
    }

    #[tokio::test]
    async fn release_leaves_the_other() {
        let mut runs = Runs::<Start>::new();
        let (a, mut a_peer) = pair("a").await;
        let (b, _b_peer) = pair("b").await;
        live(&mut runs, "a", a);
        live(&mut runs, "b", b);
        let released = runs.release("b").expect("b's run");
        assert!(released.active.is_some());
        assert_eq!(runs.serials(), ["a"]);
        assert!(!runs.is_wanted("b") && runs.is_wanted("a"));
        write_frame_async(&mut a_peer, &frame(3)).await.unwrap();
        let (serial, inbound) = timeout(Duration::from_secs(1), runs.next_inbound())
            .await
            .unwrap();
        assert_eq!(serial, "a");
        assert!(matches!(inbound, Inbound::Frame(f) if f.payload[0] == 3));
        assert!(runs.release("b").is_none());
    }

    #[tokio::test]
    async fn state_of_each_phone() {
        let mut runs = Runs::<Start>::new();
        let (ready, _peer) = pair("ready").await;
        live(&mut runs, "ready", ready);
        runs.want("due");
        runs.want("starting").starting = Some(Box::pin(Box::pin(std::future::pending())));
        let back = runs.want("back");
        back.retry_at = None;
        back.grace = Some(Instant::now() + Duration::from_secs(1));
        let retry = runs.want("retry");
        retry.failures = 2;
        retry.retry_at = Some(Instant::now() + Duration::from_secs(1));
        assert_eq!(runs.state("ready"), Some(PhoneState::Ready));
        assert_eq!(runs.state("due"), Some(PhoneState::Starting));
        assert_eq!(runs.state("starting"), Some(PhoneState::Starting));
        assert_eq!(runs.state("back"), Some(PhoneState::Reconnecting));
        assert_eq!(runs.state("retry"), Some(PhoneState::Retrying));
        assert_eq!(runs.state("other"), None);
    }

    #[tokio::test]
    async fn next_wake_is_the_earliest() {
        let mut runs = Runs::<Start>::new();
        assert_eq!(runs.next_wake(), None);
        let now = Instant::now();
        let a = runs.want("a");
        a.retry_at = Some(now + Duration::from_secs(5));
        let b = runs.want("b");
        b.retry_at = None;
        b.grace = Some(now + Duration::from_secs(3));
        assert_eq!(runs.next_wake(), Some(now + Duration::from_secs(3)));
        // A due start doesn't count while that phone has a session or a start.
        let c = runs.want("c");
        c.retry_at = Some(now + Duration::from_secs(1));
        c.starting = Some(Box::pin(Box::pin(std::future::pending())));
        assert_eq!(runs.next_wake(), Some(now + Duration::from_secs(3)));
        runs.get_mut("c").unwrap().starting = None;
        assert_eq!(runs.next_wake(), Some(now + Duration::from_secs(1)));
    }
}
