//! Unlocking a PIN-locked phone from the PC: one "Unlock your <phone>" notification per locked
//! stretch, a KDE password dialog behind its action, and the PIN handed to the run loop, which
//! sends it to the helper as one `unlock{pin}` frame. After the unlock, apps whose open the lock
//! blocked are opened again.
//!
//! The PIN never reaches a log line, an error string, `Debug` output or a process argument: it
//! lives in a [`Pin`] (overwritten when dropped) from the dialog's stdout to the frame.

use std::collections::HashMap;
use std::ffi::OsString;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Result;
use futures_util::StreamExt;
use lantern_proto::{LockReason, SecretPin};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use zbus::names::UniqueName;
use zbus::zvariant::Value;
use zbus::{Connection, MatchRule, MessageStream};

use crate::daemon::control::View;

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

/// What the daemon's [`crate::daemon::ipc::Hub`] tells [`run`] about the phone's lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// The helper's `locked{reason}`: the lock blocked a copy, a paste or an open.
    Locked(LockReason),
    /// The helper's `unlocked`, or the session ended: the locked stretch is over.
    Unlocked,
    /// The helper's `unlock_result`.
    Result { ok: bool, reason: Option<String> },
    /// Sent right after the stretch ended in an unlock: the apps (pkg, user) whose open the
    /// lock blocked in the last 2 minutes, each once.
    Reopen(Vec<(String, u32)>),
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

const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// The notification action that opens the PIN dialog.
const ACTION: &str = "unlock";
/// How long a notification server call may take (it blocks the prompt's loop meanwhile).
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// "Unlock your {name}" and its body.
pub fn locked_notice(name: &str) -> (String, String) {
    (
        format!("Unlock your {name}"),
        format!("Lantern can't paste or open apps while {name} is locked."),
    )
}

/// The notification for a failed unlock (`unlock_result{ok:false, reason}`).
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
                "Lantern couldn't confirm {name} unlocked. Check the phone before trying again."
            ),
        ),
        other => (
            format!("Couldn't unlock {name}"),
            other.unwrap_or("unlock failed").to_owned(),
        ),
    }
}

/// The `lantern open` arguments that reopen `pkg` for `user`.
pub fn reopen_args(pkg: &str, user: u32) -> Vec<String> {
    let mut args = vec!["open".to_owned()];
    if user != 0 {
        args.extend(["--user".to_owned(), user.to_string()]);
    }
    args.push(pkg.to_owned());
    args
}

/// The phone's name for notification texts.
fn phone_name(view: &watch::Receiver<View>) -> String {
    let view = view.borrow();
    view.name
        .as_deref()
        .or_else(|| view.phone.serial())
        .unwrap_or("phone")
        .to_owned()
}

/// The notification server as [`run`] uses it.
struct Notifier {
    conn: Connection,
    /// The server's unique name, from its last reply: only its `ActionInvoked` counts.
    server: Option<UniqueName<'static>>,
    /// Whether the missing-server warning was logged.
    warned: bool,
}

impl Notifier {
    /// Shows (or replaces `replaces`) a notification with the `unlock` action labelled `label`.
    async fn notify(
        &mut self,
        replaces: u32,
        (summary, body): (String, String),
        label: &str,
    ) -> u32 {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(1));
        let body = (
            "Lantern",
            replaces,
            "smartphone",
            summary.as_str(),
            body.as_str(),
            vec![ACTION, label],
            hints,
            -1i32,
        );
        let reply = tokio::time::timeout(
            CALL_TIMEOUT,
            self.conn
                .call_method(Some(BUS), PATH, Some(BUS), "Notify", &body),
        )
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("no reply in time".into())));
        match reply.and_then(|reply| {
            let sender = reply.header().sender().map(|s| s.to_owned());
            reply.body().deserialize::<u32>().map(|id| (id, sender))
        }) {
            Ok((id, sender)) => {
                tracing::info!(id, summary, "unlock notification shown");
                if let Some(sender) = sender {
                    self.server = Some(sender);
                }
                self.warned = false;
                id
            }
            Err(error) => {
                if !self.warned {
                    tracing::warn!(%error, "no unlock notification (no notification server?)");
                    self.warned = true;
                }
                0
            }
        }
    }

    async fn close(&mut self, id: u32) {
        if id == 0 {
            return;
        }
        let reply = tokio::time::timeout(
            CALL_TIMEOUT,
            self.conn
                .call_method(Some(BUS), PATH, Some(BUS), "CloseNotification", &id),
        )
        .await;
        match reply {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(%error, id, "could not close the unlock notification")
            }
            Err(_) => tracing::debug!(id, "closing the unlock notification timed out"),
        }
    }
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

/// A PIN dialog in flight.
type Asking = JoinHandle<Result<Option<Pin>, String>>;

/// Shows the unlock notification and the PIN dialog, hands valid PINs to `pins` (the run loop
/// sends each as one `unlock` frame) and reopens blocked apps after an unlock, until `events`
/// closes. Without a notification server it logs one warning and carries on.
pub async fn run(
    conn: Connection,
    mut events: mpsc::Receiver<LockEvent>,
    view: watch::Receiver<View>,
    pins: mpsc::Sender<Pin>,
    exe: PathBuf,
    prompt: Prompt,
) -> Result<()> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(BUS)?
        .path(PATH)?
        .member("ActionInvoked")?
        .build();
    let mut actions = MessageStream::for_match_rule(rule, &conn, None).await?;
    let mut notifier = Notifier {
        conn,
        server: None,
        warned: false,
    };
    // The locked stretch: open from the first `Locked` until it ends.
    let mut stretch = false;
    // The notification on screen (0: none).
    let mut shown: u32 = 0;
    let mut asking: Option<Asking> = None;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    LockEvent::Locked(reason) => {
                        if stretch {
                            continue;
                        }
                        stretch = true;
                        tracing::info!(?reason, "phone locked: asking for its PIN");
                        let name = phone_name(&view);
                        shown = notifier.notify(0, locked_notice(&name), "Enter PIN…").await;
                    }
                    LockEvent::Result { ok: false, reason } => {
                        tracing::info!(reason = reason.as_deref().unwrap_or(""), "unlock failed");
                        let name = phone_name(&view);
                        let notice = failed_notice(&name, reason.as_deref());
                        shown = notifier.notify(shown, notice, "Try again").await;
                    }
                    LockEvent::Result { ok: true, .. } | LockEvent::Unlocked => {
                        if stretch || shown != 0 {
                            tracing::info!("phone unlocked");
                        }
                        stretch = false;
                        notifier.close(std::mem::take(&mut shown)).await;
                    }
                    LockEvent::Reopen(apps) => reopen(&exe, apps),
                }
            }
            message = actions.next() => {
                let Some(Ok(message)) = message else { continue };
                let header = message.header();
                // Only the notification server's own signal counts, for a notification of ours.
                if header.sender().is_none() || header.sender() != notifier.server.as_ref() {
                    continue;
                }
                let Ok((id, action)) = message.body().deserialize::<(u32, String)>() else {
                    continue;
                };
                if id == 0 || id != shown || action != ACTION {
                    continue;
                }
                if asking.is_some() {
                    tracing::debug!("PIN dialog already open");
                    continue;
                }
                let name = phone_name(&view);
                let prompt = prompt.clone();
                tracing::info!("asking for the PIN");
                asking = Some(tokio::task::spawn_blocking(move || ask(&prompt, &name)));
            }
            answer = async { asking.as_mut().expect("guarded").await }, if asking.is_some() => {
                asking = None;
                match answer {
                    Ok(Ok(Some(pin))) => match pins.try_send(pin) {
                        Ok(()) => tracing::info!("PIN handed to the session"),
                        // The Pin inside the error is dropped (wiped) here.
                        Err(_) => tracing::warn!("PIN not sent: an unlock is already pending"),
                    },
                    Ok(Ok(None)) => {}
                    Ok(Err(message)) => {
                        tracing::warn!(message, "PIN not sent");
                        let name = phone_name(&view);
                        let notice = (format!("Couldn't unlock {name}"), message);
                        shown = notifier.notify(shown, notice, "Try again").await;
                    }
                    Err(error) => tracing::warn!(%error, "PIN dialog task failed"),
                }
            }
        }
    }
    Ok(())
}

/// Runs `exe open [--user N] pkg` for each app (not waiting for the windows).
fn reopen(exe: &std::path::Path, apps: Vec<(String, u32)>) {
    for (pkg, user) in apps {
        let exe = exe.to_owned();
        tokio::spawn(async move {
            let args = reopen_args(&pkg, user);
            tracing::info!(pkg, user, "reopening after the unlock");
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

/// Starts [`run`] on its own session-bus connection (5 s to connect). Without a bus there is no
/// unlock prompt: one warning, and `None`.
pub async fn spawn(
    events: mpsc::Receiver<LockEvent>,
    view: watch::Receiver<View>,
    pins: mpsc::Sender<Pin>,
) -> Option<JoinHandle<()>> {
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
    Some(tokio::spawn(async move {
        if let Err(error) = run(conn, events, view, pins, exe, Prompt::kdialog()).await {
            tracing::warn!(error = format!("{error:#}"), "unlock prompt stopped");
        }
    }))
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
        assert_eq!(reopen_args("com.foo", 0), ["open", "com.foo"]);
        assert_eq!(
            reopen_args("com.foo", 10),
            ["open", "--user", "10", "com.foo"]
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
