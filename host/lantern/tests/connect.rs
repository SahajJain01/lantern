//! The connect prompt against a private `dbus-daemon` (never the real session bus) and a fake
//! `org.freedesktop.Notifications`: "<phone> is plugged in", "Connect your <phone>" and "Plug in
//! your <phone>", one per phone, and the Connect click as a `Command::Connect`.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{emit_action, wait_for, Bus, Call};
use lantern::connect::{self, Health};
use lantern::daemon::control::{Command, ConnectEvent};
use tokio::sync::mpsc;
use zbus::Connection;

struct Rig {
    bus: Bus,
    server: Option<Connection>,
    calls: Arc<Mutex<Vec<Call>>>,
    events: Option<mpsc::Sender<ConnectEvent>>,
    commands: mpsc::Receiver<Command>,
    health: Health,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Rig {
    async fn start(with_server: bool) -> Self {
        let bus = Bus::start();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server = if with_server {
            Some(bus.serve_notes(calls.clone()).await)
        } else {
            None
        };
        let (events, events_rx) = mpsc::channel(32);
        let (commands_tx, commands) = mpsc::channel(8);
        let health = Health::new();
        let conn = bus.connect().await;
        let task = tokio::spawn(connect::run(conn, events_rx, commands_tx, health.clone()));
        Self {
            bus,
            server,
            calls,
            events: Some(events),
            commands,
            health,
            task,
        }
    }

    async fn send(&self, event: ConnectEvent) {
        self.events.as_ref().unwrap().send(event).await.unwrap();
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn notifies(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Notify { .. }))
            .collect()
    }

    /// Waits for the `n`th `Notify` and returns it.
    async fn notify(&self, n: usize) -> Call {
        wait_for("Notify", Duration::from_secs(5), || {
            self.notifies().get(n - 1).cloned()
        })
        .await
    }

    async fn notify_id(&self, n: usize) -> u32 {
        match self.notify(n).await {
            Call::Notify { id, .. } => id,
            _ => unreachable!(),
        }
    }

    async fn click(&self, id: u32, action: &str) {
        emit_action(self.server.as_ref().unwrap(), id, action).await;
    }

    async fn next_command(&mut self) -> Command {
        tokio::time::timeout(Duration::from_secs(5), self.commands.recv())
            .await
            .expect("no command within 5 s")
            .unwrap()
    }

    async fn assert_no_command(&mut self) {
        if let Ok(cmd) =
            tokio::time::timeout(Duration::from_millis(500), self.commands.recv()).await
        {
            panic!("unexpected command: {cmd:?}");
        }
    }

    async fn wait_closed(&self, id: u32) {
        wait_for("CloseNotification", Duration::from_secs(5), || {
            self.calls().contains(&Call::Close(id)).then_some(())
        })
        .await;
    }
}

fn plugged(serial: &str, name: &str) -> ConnectEvent {
    ConnectEvent::PluggedIn {
        serial: serial.into(),
        name: name.into(),
    }
}

fn refused(serial: &str, name: &str) -> ConnectEvent {
    ConnectEvent::OpenRefused {
        serial: serial.into(),
        name: name.into(),
        app: "Xray".into(),
        pkg: "com.x".into(),
        user: 0,
    }
}

fn gone(serial: &str) -> ConnectEvent {
    ConnectEvent::Gone {
        serial: serial.into(),
    }
}

const PLUGGED_BODY: &str = "Connect it to use its apps on this PC.";

// ---------------------------------------------------------------------------------------------

#[test]
fn notice_texts() {
    assert_eq!(
        connect::plugged_notice("Pixel"),
        ("Pixel is plugged in".to_owned(), PLUGGED_BODY.to_owned())
    );
    assert_eq!(
        connect::offered_notice("Pixel", "Xray"),
        (
            "Connect your Pixel".to_owned(),
            "Xray is on your Pixel. Connect it to open Xray here.".to_owned()
        )
    );
    assert_eq!(
        connect::plug_in_notice("Pixel", Some("Xray")),
        (
            "Plug in your Pixel".to_owned(),
            "Xray opens if you plug it in within 2 minutes.".to_owned()
        )
    );
    assert_eq!(
        connect::plug_in_notice("Pixel", None),
        (
            "Plug in your Pixel".to_owned(),
            "It connects if you plug it in within 2 minutes.".to_owned()
        )
    );
}

#[tokio::test]
async fn plugged_in_notice_and_connect_click_sends_command() {
    let mut rig = Rig::start(true).await;
    rig.send(plugged("dev", "Pixel")).await;
    assert_eq!(
        rig.notify(1).await,
        Call::Notify {
            id: 1,
            replaces: 0,
            icon: "smartphone".into(),
            summary: "Pixel is plugged in".into(),
            body: PLUGGED_BODY.into(),
            actions: vec!["connect".into(), "Connect".into()],
            urgency: Some(1),
        }
    );
    // Another action or another id does nothing.
    rig.click(1, "default").await;
    rig.click(8, "connect").await;
    rig.assert_no_command().await;
    rig.click(1, "connect").await;
    assert_eq!(
        rig.next_command().await,
        Command::Connect {
            serial: "dev".into(),
            open: None
        }
    );
    rig.wait_closed(1).await;
    // The notice is done: a second click sends nothing.
    rig.click(1, "connect").await;
    rig.assert_no_command().await;
}

#[tokio::test]
async fn plugged_in_replaces_previous_notice() {
    let rig = Rig::start(true).await;
    rig.send(plugged("dev", "Pixel")).await;
    let first = rig.notify_id(1).await;
    rig.send(plugged("dev", "Pixel")).await;
    match rig.notify(2).await {
        Call::Notify { id, replaces, .. } => {
            assert_eq!(replaces, first);
            assert_eq!(id, first);
        }
        _ => unreachable!(),
    }
    // Another phone gets its own notice.
    rig.send(plugged("b", "Moto")).await;
    match rig.notify(3).await {
        Call::Notify {
            replaces, summary, ..
        } => {
            assert_eq!(replaces, 0);
            assert_eq!(summary, "Moto is plugged in");
        }
        _ => unreachable!(),
    }
    // A refused open for dev replaces dev's notice too: one per phone.
    rig.send(refused("dev", "Pixel")).await;
    match rig.notify(4).await {
        Call::Notify {
            replaces, summary, ..
        } => {
            assert_eq!(replaces, first);
            assert_eq!(summary, "Connect your Pixel");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn refused_open_connect_carries_the_app() {
    let mut rig = Rig::start(true).await;
    rig.send(refused("dev", "Pixel")).await;
    let id = match rig.notify(1).await {
        Call::Notify {
            id,
            summary,
            body,
            actions,
            ..
        } => {
            assert_eq!(summary, "Connect your Pixel");
            assert_eq!(body, "Xray is on your Pixel. Connect it to open Xray here.");
            assert_eq!(actions, ["connect", "Connect"]);
            id
        }
        _ => unreachable!(),
    };
    rig.click(id, "connect").await;
    assert_eq!(
        rig.next_command().await,
        Command::Connect {
            serial: "dev".into(),
            open: Some(("com.x".into(), 0))
        }
    );
    rig.wait_closed(id).await;
}

#[tokio::test]
async fn plug_in_has_no_action() {
    let mut rig = Rig::start(true).await;
    rig.send(ConnectEvent::PlugIn {
        serial: "dev".into(),
        name: "Pixel".into(),
        app: Some("Xray".into()),
    })
    .await;
    let id = match rig.notify(1).await {
        Call::Notify {
            id,
            summary,
            body,
            actions,
            ..
        } => {
            assert_eq!(summary, "Plug in your Pixel");
            assert_eq!(body, "Xray opens if you plug it in within 2 minutes.");
            assert!(actions.is_empty(), "{actions:?}");
            id
        }
        _ => unreachable!(),
    };
    // Even a forged "connect" on it sends nothing.
    rig.click(id, "connect").await;
    rig.assert_no_command().await;
    rig.send(ConnectEvent::PlugIn {
        serial: "b".into(),
        name: "Moto".into(),
        app: None,
    })
    .await;
    match rig.notify(2).await {
        Call::Notify { body, actions, .. } => {
            assert_eq!(body, "It connects if you plug it in within 2 minutes.");
            assert!(actions.is_empty());
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn gone_closes_the_notice() {
    let mut rig = Rig::start(true).await;
    rig.send(plugged("dev", "Pixel")).await;
    let id = rig.notify_id(1).await;
    // Gone for a phone without a notice closes nothing.
    rig.send(gone("b")).await;
    rig.send(gone("dev")).await;
    rig.wait_closed(id).await;
    let closes = rig
        .calls()
        .into_iter()
        .filter(|c| matches!(c, Call::Close(_)))
        .count();
    assert_eq!(closes, 1, "{:?}", rig.calls());
    // A click on the closed notice does nothing.
    rig.click(id, "connect").await;
    rig.assert_no_command().await;
    // The next plug-in is a new notice.
    rig.send(plugged("dev", "Pixel")).await;
    match rig.notify(2).await {
        Call::Notify { replaces, .. } => assert_eq!(replaces, 0),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn action_from_another_sender_is_ignored() {
    let mut rig = Rig::start(true).await;
    rig.send(plugged("dev", "Pixel")).await;
    let id = rig.notify_id(1).await;
    let other = rig.bus.connect().await;
    emit_action(&other, id, "connect").await;
    rig.assert_no_command().await;
    assert!(!rig.calls().contains(&Call::Close(id)));
    rig.click(id, "connect").await;
    assert_eq!(
        rig.next_command().await,
        Command::Connect {
            serial: "dev".into(),
            open: None
        }
    );
}

#[tokio::test]
async fn no_notification_server_is_harmless() {
    let mut rig = Rig::start(false).await;
    for _ in 0..3 {
        rig.send(plugged("dev", "Pixel")).await;
        rig.send(refused("dev", "Pixel")).await;
        rig.send(gone("dev")).await;
    }
    // Every event is taken: the channel (32) never fills and sends never block.
    for _ in 0..40 {
        tokio::time::timeout(Duration::from_secs(2), rig.send(plugged("b", "Moto")))
            .await
            .expect("the prompt stopped taking events");
    }
    assert!(!rig.task.is_finished(), "run must keep going");
    assert!(!rig.health.healthy());
    rig.assert_no_command().await;
    // It ends cleanly when the daemon drops its sender.
    rig.events.take();
    let task = std::mem::replace(&mut rig.task, tokio::spawn(async { Ok(()) }));
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run did not end")
        .unwrap()
        .unwrap();
}

/// Plasma shows a summary as plain text and a body as markup: names are escaped in bodies only.
#[tokio::test]
async fn names_are_escaped() {
    let rig = Rig::start(true).await;
    rig.send(plugged("dev", "A&B <1>")).await;
    match rig.notify(1).await {
        Call::Notify { summary, .. } => assert_eq!(summary, "A&B <1> is plugged in"),
        _ => unreachable!(),
    }
    rig.send(ConnectEvent::OpenRefused {
        serial: "b".into(),
        name: "A&B <1>".into(),
        app: "<X&Y>".into(),
        pkg: "com.x".into(),
        user: 0,
    })
    .await;
    match rig.notify(2).await {
        Call::Notify { summary, body, .. } => {
            assert_eq!(summary, "Connect your A&B <1>");
            assert_eq!(
                body,
                "&lt;X&amp;Y&gt; is on your A&amp;B &lt;1&gt;. Connect it to open &lt;X&amp;Y&gt; here."
            );
        }
        _ => unreachable!(),
    }
    rig.send(ConnectEvent::PlugIn {
        serial: "c".into(),
        name: "A&B <1>".into(),
        app: Some("<X&Y>".into()),
    })
    .await;
    match rig.notify(3).await {
        Call::Notify { summary, body, .. } => {
            assert_eq!(summary, "Plug in your A&B <1>");
            assert_eq!(
                body,
                "&lt;X&amp;Y&gt; opens if you plug it in within 2 minutes."
            );
        }
        _ => unreachable!(),
    }
}

/// The ledger's Task 5 note: lanternd offers Connect (and the window stays silent) only while
/// the prompt's notifications get through. A server that answers makes it healthy; a failed
/// `Notify` makes it unhealthy until the server answers again.
#[tokio::test]
async fn health_follows_the_notification_server() {
    let mut rig = Rig::start(true).await;
    wait_for("healthy", Duration::from_secs(5), || {
        rig.health.healthy().then_some(())
    })
    .await;
    // The server goes away: the next offer can't be shown, and the prompt says so.
    drop(rig.server.take());
    tokio::time::sleep(Duration::from_millis(200)).await;
    rig.send(refused("dev", "Pixel")).await;
    wait_for("unhealthy", Duration::from_secs(7), || {
        (!rig.health.healthy()).then_some(())
    })
    .await;
    // It comes back: healthy again without any event.
    rig.server = Some(rig.bus.serve_notes(rig.calls.clone()).await);
    wait_for("healthy again", Duration::from_secs(10), || {
        rig.health.healthy().then_some(())
    })
    .await;
    rig.assert_no_command().await;
}

#[tokio::test]
async fn without_a_server_it_never_reports_healthy() {
    let rig = Rig::start(false).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!rig.health.healthy());
}
