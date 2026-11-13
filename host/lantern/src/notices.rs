//! lanternd's own notifications with actions, straight on `org.freedesktop.Notifications`
//! (`notify-send` can't wait for a click): the unlock prompt's and the connect prompt's. Every
//! call has a 5 s timeout, so a hung server can't block a prompt for longer; without a server
//! each call fails quietly after one warning.

use std::collections::HashMap;

use anyhow::Result;
use zbus::names::UniqueName;
use zbus::zvariant::Value;
use zbus::{Connection, MatchRule, MessageStream};

const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// How long a notification server call may take (it blocks the caller's loop meanwhile).
pub const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `text` with `&`, `<` and `>` escaped, so Plasma's notification markup shows it as typed
/// (a phone or app name like `A&B <1>`).
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// The notification server as a prompt uses it.
pub struct Notices {
    conn: Connection,
    /// The server's unique name, from its last reply: only its `ActionInvoked` counts.
    server: Option<UniqueName<'static>>,
    /// Whether the missing-server warning was logged (reset by the next success).
    warned: bool,
}

impl Notices {
    /// Notices on `conn`, and the stream of every `ActionInvoked` signal on the bus (filter it
    /// with [`Self::action`]).
    pub async fn new(conn: Connection) -> Result<(Notices, MessageStream)> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(BUS)?
            .path(PATH)?
            .member("ActionInvoked")?
            .build();
        let actions = MessageStream::for_match_rule(rule, &conn, None).await?;
        Ok((
            Notices {
                conn,
                server: None,
                warned: false,
            },
            actions,
        ))
    }

    /// Shows (or, with `replaces` != 0, replaces) a notification with `actions` as
    /// (key, label) pairs. Its id, or 0 if the server didn't answer within 5 s (one warning
    /// until the next success).
    pub async fn notify(
        &mut self,
        replaces: u32,
        summary: &str,
        body: &str,
        actions: &[(&str, &str)],
    ) -> u32 {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(1));
        let actions: Vec<&str> = actions
            .iter()
            .flat_map(|(key, label)| [*key, *label])
            .collect();
        let message = (
            "Lantern",
            replaces,
            "smartphone",
            summary,
            body,
            actions,
            hints,
            -1i32,
        );
        let reply = tokio::time::timeout(
            CALL_TIMEOUT,
            self.conn
                .call_method(Some(BUS), PATH, Some(BUS), "Notify", &message),
        )
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("no reply in time".into())));
        match reply.and_then(|reply| {
            let sender = reply.header().sender().map(|s| s.to_owned());
            reply.body().deserialize::<u32>().map(|id| (id, sender))
        }) {
            Ok((id, sender)) => {
                tracing::info!(id, summary, "notification shown");
                if let Some(sender) = sender {
                    self.server = Some(sender);
                }
                self.warned = false;
                id
            }
            Err(error) => {
                if !self.warned {
                    tracing::warn!(%error, summary, "notification not shown (no notification server?)");
                    self.warned = true;
                }
                0
            }
        }
    }

    /// Closes notification `id` (0: nothing), within 5 s; a failure is only logged.
    pub async fn close(&mut self, id: u32) {
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
            Ok(Err(error)) => tracing::debug!(%error, id, "could not close the notification"),
            Err(_) => tracing::debug!(id, "closing the notification timed out"),
        }
    }

    /// `(id, action)` of an `ActionInvoked` from the server that answered our last `Notify`;
    /// `None` for anyone else's signal or a malformed one.
    pub fn action(&self, message: &zbus::Message) -> Option<(u32, String)> {
        let header = message.header();
        let sender = header.sender()?;
        if Some(sender) != self.server.as_ref() {
            return None;
        }
        message.body().deserialize::<(u32, String)>().ok()
    }

    /// The bus connection (for [`server_answers`] from another task).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Whether a notification server answers `GetServerInformation` within 5 s.
pub async fn server_answers(conn: &Connection) -> bool {
    let reply = tokio::time::timeout(
        CALL_TIMEOUT,
        conn.call_method(Some(BUS), PATH, Some(BUS), "GetServerInformation", &()),
    )
    .await;
    matches!(reply, Ok(Ok(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_markup_escapes_amp_lt_gt() {
        assert_eq!(escape_markup("A&B <1>"), "A&amp;B &lt;1&gt;");
        assert_eq!(escape_markup("Xiaomi 11T Pro"), "Xiaomi 11T Pro");
        assert_eq!(escape_markup("&amp;"), "&amp;amp;");
        assert_eq!(escape_markup(""), "");
    }
}
