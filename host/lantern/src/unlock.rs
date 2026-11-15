//! Unlocking a PIN-locked phone from the PC: one "Unlock your <phone>" notification per locked
//! stretch, a KDE password dialog behind its action, and the PIN handed to the run loop, which
//! sends it to the helper as one `unlock{pin}` frame. After the unlock, apps whose open the lock
//! blocked are opened again.
//!
//! The PIN never reaches a log line, an error string, `Debug` output or a process argument: it
//! lives in a [`Pin`] (overwritten when dropped) from the dialog's stdout to the frame.

use std::ffi::OsString;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Result;
use futures_util::StreamExt;
use lantern_proto::{LockReason, SecretPin};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use zbus::Connection;

use crate::daemon::control::View;
use crate::notices::{escape_markup, Notices};

/// What [`Pin::parse`] says about anything but 4 to 16 ASCII digits.
pub const PIN_FORMAT: &str = "the PIN must be 4 to 16 digits";

/// A phone PIN (4-16 ASCII digits) on its way to one `unlock` frame. `Debug` prints
/// `Pin(<redacted>)`, there is no `Display`, and dropping it overwrites its buffer.
pub struct Pin(ManuallyDrop<Vec<u8>>);

impl Pin {
    /// Accepts 4 to 16 ASCII digits after trimming one trailing newline (kdialog and echo end
    /// their output with one). Anything else is refused with [`PIN_FORMAT`], and `raw` is wiped
    /// either way.
    pub fn parse(raw: String) -> Result<Pin, &'static str> {
        // Wrapped first, so every return path wipes `raw`.
        let mut pin = Pin(ManuallyDrop::new(raw.into_bytes()));
        if pin.0.last() == Some(&b'\n') {
            // The byte stays in the buffer's capacity, which Drop wipes too.
            pin.0.pop();
        }
        if (4..=16).contains(&pin.0.len()) && pin.0.iter().all(u8::is_ascii_digit) {
            Ok(pin)
        } else {
            Err(PIN_FORMAT)
        }
    }

    /// The digits. Never log them.
    pub(crate) fn digits(&self) -> &str {
        // Only ASCII digits pass `parse`.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    /// The wire form for the one `unlock` frame.
    pub(crate) fn secret(&self) -> SecretPin {
        SecretPin::new(self.digits().to_owned())
    }

    /// Overwrites the whole buffer, spare capacity included.
    fn wipe(&mut self) {
        let ptr = self.0.as_mut_ptr();
        for i in 0..self.0.capacity() {
            // SAFETY: `i` is within the buffer's allocation.
            unsafe { std::ptr::write_volatile(ptr.add(i), 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }

    /// The buffer's address and capacity, so a test can read it after the drop.
    #[cfg(test)]
    fn buffer(&self) -> (*const u8, usize) {
        (self.0.as_ptr(), self.0.capacity())
    }
}

#[cfg(test)]
thread_local! {
    /// Set by a test: a dropped Pin's (wiped) buffer is leaked so the test can read it.
    static LEAK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.wipe();
        #[cfg(test)]
        if LEAK.with(std::cell::Cell::get) {
            return;
        }
        // SAFETY: dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut self.0) };
    }
}

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pin(<redacted>)")
    }
}

impl PartialEq for Pin {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl Eq for Pin {}

/// What the daemon's [`crate::daemon::ipc::Hub`] tells [`run`] about a phone's lock. Each
/// event names the phone (`serial`) whose helper sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// The helper's `locked{reason}`: the lock blocked a copy, a paste or an open (or was there
    /// when the session came up).
    Locked { serial: String, reason: LockReason },
    /// The helper's `unlocked`, or the session ended: the locked stretch is over.
    Unlocked { serial: String },
    /// The helper's `unlock_result`.
    Result {
        serial: String,
        ok: bool,
        reason: Option<String>,
    },
    /// Sent right after the stretch ended in an unlock: the apps (pkg, user) on phone `serial`
    /// whose open the lock blocked in the last 2 minutes, each once.
    Reopen {
        serial: String,
        apps: Vec<(String, u32)>,
    },
}

/// A cheap, cloneable handle that asks the unlock prompt for its PIN dialog for one phone: the
/// tray's Unlock row. It takes the notification action's path (one dialog at a time, the
/// answer pinned to the phone), and opens nothing unless the phone it names is the one whose
/// locked stretch is open; the holder never runs the dialog and never sees the PIN.
#[derive(Debug, Clone)]
pub struct Request(mpsc::Sender<String>);

/// The prompt's end of [`Request`]s, for [`run`].
#[derive(Debug)]
pub struct Requests(mpsc::Receiver<String>);

impl Request {
    /// A handle and the receiving end [`run`] takes.
    pub fn channel() -> (Request, Requests) {
        // One pending ask is enough: more clicks before the prompt looks are the same ask.
        let (tx, rx) = mpsc::channel(1);
        (Request(tx), Requests(rx))
    }

    /// Asks for the PIN dialog for phone `serial` without waiting. A click while one is
    /// pending is dropped.
    pub fn ask(&self, serial: String) {
        match self.0.try_send(serial) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("unlock: the prompt is gone; no PIN dialog")
            }
        }
    }
}

/// The PIN dialog: a command whose stdout (exit 0) is the PIN. `{name}` in an argument becomes
/// the phone's name.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Prompt {
    /// `kdialog --title Lantern --password "Enter the PIN for {name}"`.
    pub fn kdialog() -> Self {
        Self {
            program: "kdialog".into(),
            args: [
                "--title",
                "Lantern",
                "--password",
                "Enter the PIN for {name}",
            ]
            .map(str::to_owned)
            .into(),
        }
    }
}

/// The notification action that opens the PIN dialog.
const ACTION: &str = "unlock";

/// "Unlock your {name}" and its body. Summaries are plain text in Plasma, so `name` is
/// escaped for markup in the body only.
pub fn locked_notice(name: &str) -> (String, String) {
    (
        format!("Unlock your {name}"),
        format!(
            "Lantern can't paste or open apps while {} is locked.",
            escape_markup(name)
        ),
    )
}

/// The notification for a failed unlock (`unlock_result{ok:false, reason}`); `name` and
/// `reason` are escaped in the body only, as [`locked_notice`].
pub fn failed_notice(name: &str, reason: Option<&str>) -> (String, String) {
    match reason {
        Some("wrong PIN") => (
            format!("That PIN didn't unlock {name}"),
            "wrong PIN".to_owned(),
        ),
        // Unconfirmed is not the user's error: never invite another guess toward a lockout.
        Some("unlock not confirmed") => (
            format!("Couldn't unlock {name}"),
            format!(
                "Lantern couldn't confirm {} unlocked. Check the phone before trying again.",
                escape_markup(name)
            ),
        ),
        other => (
            format!("Couldn't unlock {name}"),
            escape_markup(other.unwrap_or("unlock failed")),
        ),
    }
}

/// The `lantern open` arguments that reopen `pkg` for `user` on `phone` (`--` before the
/// package, so it is never read as an option).
pub fn reopen_args(pkg: &str, user: u32, phone: Option<&str>) -> Vec<String> {
    let mut args = vec!["open".to_owned()];
    if let Some(phone) = phone {
        args.extend(["--phone".to_owned(), phone.to_owned()]);
    }
    if user != 0 {
        args.extend(["--user".to_owned(), user.to_string()]);
    }
    args.extend(["--".to_owned(), pkg.to_owned()]);
    args
}

/// Phone `serial`'s name for notification texts.
fn phone_name(view: &watch::Receiver<View>, serial: &str) -> String {
    view.borrow()
        .phone(serial)
        .map(|p| p.display_name().to_owned())
        .unwrap_or_else(|| "phone".to_owned())
}

/// Shows (or replaces `replaces`) the notice with the `unlock` action labelled `label`.
async fn show(
    notices: &mut Notices,
    replaces: u32,
    (summary, body): (String, String),
    label: &str,
) -> u32 {
    notices
        .notify(replaces, &summary, &body, &[(ACTION, label)])
        .await
}

/// Runs the PIN dialog (blocking): its stdout parsed as a [`Pin`], `Ok(None)` if the user
/// cancelled.
fn ask(prompt: &Prompt, name: &str) -> Result<Option<Pin>, String> {
    let args: Vec<OsString> = prompt
        .args
        .iter()
        .map(|arg| arg.replace("{name}", name).into())
        .collect();
    let mut command = crate::desktop::command(&prompt.program, &args);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let output = command
        .output()
        .map_err(|error| format!("could not run {}: {error}", prompt.program.display()))?;
    let stdout = output.stdout;
    if !output.status.success() {
        // Cancelled: whatever it printed is wiped.
        drop(Pin(ManuallyDrop::new(stdout)));
        tracing::info!(status = %output.status, "PIN dialog cancelled");
        return Ok(None);
    }
    let raw = match String::from_utf8(stdout) {
        Ok(raw) => raw,
        Err(error) => {
            drop(Pin(ManuallyDrop::new(error.into_bytes())));
            return Err(PIN_FORMAT.to_owned());
        }
    };
    Pin::parse(raw).map(Some).map_err(str::to_owned)
}

/// Starts the PIN dialog for the phone whose stretch is open (its serial pinned to the
/// answer), unless one is already open. `wanted` is the phone a tray request named: the dialog
/// opens only if that is the stretch's phone. `None` if a dialog is open, no stretch is open or
/// another phone was named.
fn begin_asking(
    asking: &Option<Asking>,
    view: &watch::Receiver<View>,
    prompt: &Prompt,
    stretch: (Option<&str>, u64),
    from: &str,
    wanted: Option<&str>,
) -> Option<Asking> {
    if asking.is_some() {
        tracing::debug!(from, "PIN dialog already open");
        return None;
    }
    let (open, ended) = stretch;
    // The PIN is pinned to the stretch's phone now.
    let Some(serial) = open.map(str::to_owned) else {
        tracing::info!(from, "unlock clicked with no phone locked; no PIN dialog");
        return None;
    };
    if let Some(wanted) = wanted.filter(|wanted| *wanted != serial) {
        tracing::info!(
            from,
            wanted,
            locked = serial,
            "unlock asked for a phone whose stretch isn't the open one; no PIN dialog"
        );
        return None;
    }
    let name = phone_name(view, &serial);
    let prompt = prompt.clone();
    tracing::info!(serial, from, "asking for the PIN");
    Some(Asking {
        task: tokio::task::spawn_blocking(move || ask(&prompt, &name)),
        serial,
        stretch: ended,
    })
}

/// A PIN dialog in flight.
struct Asking {
    task: JoinHandle<Result<Option<Pin>, String>>,
    /// The serial of the phone whose notification was clicked: the PIN goes only to it.
    serial: String,
    /// [`run`]'s stretch count at the click: an answer after the stretch ended is wiped.
    stretch: u64,
}

/// Shows the unlock notification and the PIN dialog, hands valid PINs to `pins` with the serial
/// of the phone they were asked for (the run loop sends each as one `unlock` frame, only to that
/// phone) and reopens blocked apps after an unlock, until `events` closes. One stretch at a
/// time: it belongs to the phone whose `Locked` opened it, and other phones' lock events are
/// ignored until it ends. The dialog opens on the notification's action or on a [`Request`]
/// (the tray's Unlock row) naming that phone, one at a time, and its answer is pinned to that
/// phone. Without a notification server it logs one warning and carries on.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    conn: Connection,
    mut events: mpsc::Receiver<LockEvent>,
    requests: Requests,
    view: watch::Receiver<View>,
    pins: mpsc::Sender<(String, Pin)>,
    exe: PathBuf,
    prompt: Prompt,
) -> Result<()> {
    let (mut notices, mut actions) = Notices::new(conn).await?;
    // The locked stretch: the phone whose first `Locked` opened it, until it ends.
    let mut stretch: Option<String> = None;
    // Stretches ended so far: a dialog answer is only good for the stretch it was asked in.
    let mut ended: u64 = 0;
    // The notification on screen (0: none).
    let mut shown: u32 = 0;
    let mut asking: Option<Asking> = None;
    let mut requests = requests.0;
    // Whether `requests` can still deliver (every Request dropped: stop polling it).
    let mut requestable = true;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                // Another phone's event while a stretch is open waits until it ends.
                let other = |serial: &str| stretch.as_deref().is_some_and(|s| s != serial);
                match event {
                    LockEvent::Locked { serial, reason } => {
                        if stretch.is_some() {
                            if other(&serial) {
                                tracing::info!(serial, ?reason, "another phone is locked; its prompt waits");
                            }
                            continue;
                        }
                        tracing::info!(serial, ?reason, "phone locked: asking for its PIN");
                        let name = phone_name(&view, &serial);
                        stretch = Some(serial);
                        shown = show(&mut notices, 0, locked_notice(&name), "Enter PIN…").await;
                    }
                    LockEvent::Result { serial, .. } | LockEvent::Unlocked { serial }
                        if other(&serial) =>
                    {
                        tracing::debug!(serial, "lock event for another phone than the stretch's; ignored");
                    }
                    LockEvent::Result { serial, ok: false, reason } => {
                        tracing::info!(serial, reason = reason.as_deref().unwrap_or(""), "unlock failed");
                        let name = phone_name(&view, &serial);
                        let notice = failed_notice(&name, reason.as_deref());
                        shown = show(&mut notices, shown, notice, "Try again").await;
                    }
                    LockEvent::Result { serial, ok: true, .. } | LockEvent::Unlocked { serial } => {
                        if stretch.is_some() || shown != 0 {
                            tracing::info!(serial, "phone unlocked");
                        }
                        stretch = None;
                        ended += 1;
                        notices.close(std::mem::take(&mut shown)).await;
                    }
                    LockEvent::Reopen { serial, apps } => reopen(&exe, &serial, apps),
                }
            }
            message = actions.next() => {
                let Some(Ok(message)) = message else { continue };
                // Only the notification server's own signal counts, for a notification of ours.
                let Some((id, action)) = notices.action(&message) else { continue };
                if id == 0 || id != shown || action != ACTION {
                    continue;
                }
                if let Some(started) = begin_asking(&asking, &view, &prompt, (stretch.as_deref(), ended), "notification", None) {
                    asking = Some(started);
                }
            }
            request = requests.recv(), if requestable => {
                let Some(wanted) = request else {
                    requestable = false;
                    continue;
                };
                // The tray shows its row only while locked; a click that raced the unlock
                // opens nothing.
                if stretch.is_none() {
                    tracing::info!(wanted, "unlock requested while not locked; no PIN dialog");
                    continue;
                }
                if let Some(started) = begin_asking(&asking, &view, &prompt, (stretch.as_deref(), ended), "tray", Some(&wanted)) {
                    asking = Some(started);
                }
            }
            answer = async { (&mut asking.as_mut().expect("guarded").task).await }, if asking.is_some() => {
                let Asking { serial, stretch: asked_in, .. } = asking.take().expect("guarded");
                match answer {
                    Ok(Ok(Some(pin))) if asked_in != ended => {
                        drop(pin);
                        tracing::info!(serial, "the lock stretch ended during the PIN dialog; PIN dropped");
                    }
                    Ok(Ok(Some(pin))) => match pins.try_send((serial.clone(), pin)) {
                        Ok(()) => tracing::info!(serial, "PIN handed to the session"),
                        // The Pin inside the error is dropped (wiped) here.
                        Err(_) => tracing::warn!("PIN not sent: an unlock is already pending"),
                    },
                    Ok(Ok(None)) => {}
                    Ok(Err(message)) => {
                        tracing::warn!(serial, message, "PIN not sent");
                        let name = phone_name(&view, &serial);
                        let notice = (format!("Couldn't unlock {name}"), escape_markup(&message));
                        shown = show(&mut notices, shown, notice, "Try again").await;
                    }
                    Err(error) => tracing::warn!(%error, "PIN dialog task failed"),
                }
            }
        }
    }
    Ok(())
}

/// Runs `exe open --phone serial [--user N] -- pkg` for each app (not waiting for the windows).
fn reopen(exe: &std::path::Path, serial: &str, apps: Vec<(String, u32)>) {
    for (pkg, user) in apps {
        let exe = exe.to_owned();
        let serial = serial.to_owned();
        tokio::spawn(async move {
            let args = reopen_args(&pkg, user, Some(&serial));
            tracing::info!(pkg, user, serial, "reopening after the unlock");
            // desktop::spawn's systemctl lookup blocks: keep it off the async workers.
            let spawned =
                tokio::task::spawn_blocking(move || crate::desktop::spawn(&exe, &args)).await;
            match spawned {
                Ok(Ok(mut child)) => {
                    let _ = child.wait().await;
                }
                Ok(Err(error)) => tracing::warn!(%error, pkg, "could not reopen"),
                Err(error) => tracing::warn!(%error, pkg, "reopen task failed"),
            }
        });
    }
}

/// Starts [`run`] on its own session-bus connection (5 s to connect), with the [`Request`]
/// handle the tray holds. Without a bus there is no unlock prompt: one warning, and `None`.
pub async fn spawn(
    events: mpsc::Receiver<LockEvent>,
    view: watch::Receiver<View>,
    pins: mpsc::Sender<(String, Pin)>,
) -> Option<(JoinHandle<()>, Request)> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let conn = match tokio::time::timeout(TIMEOUT, Connection::session()).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(error)) => {
            tracing::warn!("no unlock prompt: {error}");
            return None;
        }
        Err(_) => {
            tracing::warn!("no unlock prompt: the session bus did not answer within {TIMEOUT:?}");
            return None;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!(%error, "no unlock prompt: unknown lantern executable");
            return None;
        }
    };
    let (request, requests) = Request::channel();
    let task = tokio::spawn(async move {
        let prompt = Prompt::kdialog();
        if let Err(error) = run(conn, events, requests, view, pins, exe, prompt).await {
            tracing::warn!(error = format!("{error:#}"), "unlock prompt stopped");
        }
    });
    Some((task, request))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_parse_accepts_4_to_16_digits() {
        assert_eq!(Pin::parse("1234\n".into()).unwrap().digits(), "1234");
        assert_eq!(Pin::parse("1234".into()).unwrap().digits(), "1234");
        let sixteen = "1234567890123456".to_owned();
        assert_eq!(Pin::parse(sixteen.clone()).unwrap().digits(), sixteen);
        for bad in [
            "123",
            "12a4",
            "12345678901234567",
            " 1234",
            "1234\n\n",
            "",
            "١٢٣٤",
        ] {
            assert_eq!(Pin::parse(bad.into()), Err(PIN_FORMAT), "{bad:?}");
        }
    }

    #[test]
    fn pin_debug_is_redacted() {
        let pin = Pin::parse("97531\n".into()).unwrap();
        assert_eq!(format!("{pin:?}"), "Pin(<redacted>)");
        assert_eq!(format!("{:?}", Some(&pin)), "Some(Pin(<redacted>))");
        assert_eq!(format!("{:?}", pin.secret()), "SecretPin(<redacted>)");
        assert_eq!(pin.secret().expose(), "97531");
    }

    #[test]
    fn pin_drop_zeroes() {
        LEAK.with(|leak| leak.set(true));
        let pin = Pin::parse("8642097\n".into()).unwrap();
        let (ptr, cap) = pin.buffer();
        assert!(cap >= 8);
        drop(pin);
        // SAFETY: LEAK kept the allocation alive; every byte of it was written.
        let bytes: Vec<u8> = (0..cap)
            .map(|i| unsafe { std::ptr::read_volatile(ptr.add(i)) })
            .collect();
        LEAK.with(|leak| leak.set(false));
        // SAFETY: the leaked buffer, freed once here.
        drop(unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, cap) });
        assert!(bytes.iter().all(|b| *b == 0), "{bytes:?}");
    }

    #[test]
    fn notice_texts() {
        assert_eq!(
            locked_notice("edge 50 pro"),
            (
                "Unlock your edge 50 pro".to_owned(),
                "Lantern can't paste or open apps while edge 50 pro is locked.".to_owned()
            )
        );
        assert_eq!(
            failed_notice("P", Some("wrong PIN")).0,
            "That PIN didn't unlock P"
        );
        let unconfirmed = failed_notice("P", Some("unlock not confirmed"));
        assert_eq!(unconfirmed.0, "Couldn't unlock P");
        assert_eq!(
            unconfirmed.1,
            "Lantern couldn't confirm P unlocked. Check the phone before trying again."
        );
        assert_eq!(
            failed_notice("P", Some("lock screen not reached")),
            (
                "Couldn't unlock P".to_owned(),
                "lock screen not reached".to_owned()
            )
        );
        assert_eq!(failed_notice("P", Some("something new")).1, "something new");
        assert_eq!(failed_notice("P", None).0, "Couldn't unlock P");
    }

    #[test]
    fn reopen_args_name_the_user_only_when_not_0() {
        assert_eq!(reopen_args("com.foo", 0, None), ["open", "--", "com.foo"]);
        assert_eq!(
            reopen_args("com.foo", 10, None),
            ["open", "--user", "10", "--", "com.foo"]
        );
        // `lantern open` parses them back.
        let args = reopen_args("com.foo", 10, None);
        let args: Vec<&str> = args[1..].iter().map(String::as_str).collect();
        let id = crate::window::identity::Identity::parse_args(&args).unwrap();
        assert_eq!((id.pkg.as_str(), id.user), ("com.foo", 10));
    }

    #[test]
    fn reopen_args_name_the_phone() {
        assert_eq!(
            reopen_args("com.foo", 0, Some("20e9318")),
            ["open", "--phone", "20e9318", "--", "com.foo"]
        );
        let args = reopen_args("com.foo", 10, Some("ZD222LMDFV"));
        assert_eq!(
            args,
            [
                "open",
                "--phone",
                "ZD222LMDFV",
                "--user",
                "10",
                "--",
                "com.foo"
            ]
        );
        let args: Vec<&str> = args[1..].iter().map(String::as_str).collect();
        let id = crate::window::identity::Identity::parse_args(&args).unwrap();
        assert_eq!(
            (id.pkg.as_str(), id.user, id.phone.as_deref()),
            ("com.foo", 10, Some("ZD222LMDFV"))
        );
    }

    #[test]
    fn kdialog_prompt_asks_for_a_password() {
        let p = Prompt::kdialog();
        assert_eq!(p.program, PathBuf::from("kdialog"));
        assert_eq!(
            p.args,
            [
                "--title",
                "Lantern",
                "--password",
                "Enter the PIN for {name}"
            ]
        );
    }
}
