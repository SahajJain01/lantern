//! Desktop notifications through `notify-send`.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// `notify-send`'s arguments: `-a Lantern [-i icon] -- summary body`.
pub fn args(summary: &str, body: &str, icon: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-a".into(), "Lantern".into()];
    if let Some(icon) = icon {
        args.push("-i".into());
        args.push(icon.into());
    }
    args.extend(["--".into(), summary.into(), body.into()]);
    args
}

/// Shows a desktop notification. Does not wait for it; a failure is only logged.
pub fn send(summary: &str, body: &str, icon: Option<&Path>) {
    let spawned = Command::new("notify-send")
        .args(args(summary, body, icon))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            // Reap it off-thread so it never lingers as a zombie.
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => {
                    tracing::warn!(%status, "notify-send failed")
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "notify-send wait failed"),
            });
        }
        Err(error) => tracing::warn!(%error, "could not run notify-send"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_end_options_before_the_texts() {
        assert_eq!(
            args("-s", "body", None),
            ["-a", "Lantern", "--", "-s", "body"].map(OsString::from)
        );
        assert_eq!(
            args("s", "b", Some(Path::new("/i.png"))),
            ["-a", "Lantern", "-i", "/i.png", "--", "s", "b"].map(OsString::from)
        );
    }
}
