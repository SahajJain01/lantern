//! Per-phone names: the Wayland app_id (= `.desktop` stem = `StartupWMClass`), the
//! `windows.json` size key, the menu category and the folder file of each phone's apps.
//!
//! One *primary* phone keeps the names from before Lantern served several phones
//! (`lantern.<pkg>[-u<N>]`, `<pkg>[:<N>]`), so its pinned launchers and remembered sizes keep
//! working. Every other phone gets a `-p<tag>` suffix and an `@<serial>` key. The primary is
//! recorded in `<state>/menu.json`. Shared by the window and lanternd: only [`Naming::load`] and
//! [`Naming::save`] touch files.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::window::identity;

/// The file under the state dir that records the primary phone.
const MENU_FILE: &str = "menu.json";

/// The `menu.json` version written by [`Naming::save`]; 2 marks the per-phone migration done.
const VERSION: u64 = 2;

/// The longest tag, in chars.
const MAX_TAG: usize = 64;

/// `serial` with every character outside `[A-Za-z0-9_]` turned into `_`, at most 64 chars. Safe
/// in an app_id, a menu category and a file name.
pub fn tag(serial: &str) -> String {
    serial
        .chars()
        .take(MAX_TAG)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The menu category of `serial`'s entries: `X-Lantern-<tag>`.
pub fn category(serial: &str) -> String {
    format!("X-Lantern-{}", tag(serial))
}

/// The folder (`.directory`) file of `serial`'s submenu: `lantern-phone-<tag>.directory`.
pub fn directory_file(serial: &str) -> String {
    format!("lantern-phone-{}.directory", tag(serial))
}

/// Which phone keeps the legacy names. `None`: no phone does (every phone is suffixed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Naming {
    pub primary: Option<String>,
}

fn menu_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MENU_FILE)
}

fn read_menu(state_dir: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = std::fs::read_to_string(menu_path(state_dir)).ok()?;
    match serde_json::from_str(&text).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

impl Naming {
    /// `<state_dir>/menu.json`'s primary. A missing or unreadable file, or a primary that is not
    /// a non-empty string, gives no primary.
    pub fn load(state_dir: &Path) -> Naming {
        let primary = read_menu(state_dir)
            .and_then(|map| map.get("primary")?.as_str().map(str::to_owned))
            .filter(|p| !p.is_empty());
        Naming { primary }
    }

    /// Writes `{"version":2,"primary":…}` (`null` without a primary) to `<state_dir>/menu.json`
    /// (temp file, fsync, rename).
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        let body = serde_json::json!({"version": VERSION, "primary": self.primary});
        crate::daemon::companion::write_atomic(&menu_path(state_dir), body.to_string().as_bytes())
    }

    /// Whether `menu.json` records a finished migration (version 2 or later).
    pub fn migrated(state_dir: &Path) -> bool {
        read_menu(state_dir)
            .and_then(|map| map.get("version")?.as_u64())
            .is_some_and(|v| v >= VERSION)
    }

    fn is_primary(&self, serial: &str) -> bool {
        self.primary.as_deref() == Some(serial)
    }

    /// The app_id of `pkg` for Android user `user` on phone `serial`: `lantern.<pkg>[-u<N>]` for
    /// the primary, else with `-p<tag>` appended. `-` never occurs in a package name, so no
    /// app_id collides with another phone's or user's.
    pub fn app_id(&self, pkg: &str, user: u32, serial: &str) -> String {
        let legacy = identity::app_id(pkg, user);
        if self.is_primary(serial) {
            legacy
        } else {
            format!("{legacy}-p{}", tag(serial))
        }
    }

    /// The `windows.json` key: `<pkg>[:<N>]` for the primary, else `<pkg>[:<N>]@<serial>`.
    pub fn size_key(&self, pkg: &str, user: u32, serial: &str) -> String {
        let legacy = match user {
            0 => pkg.to_owned(),
            n => format!("{pkg}:{n}"),
        };
        if self.is_primary(serial) {
            legacy
        } else {
            format!("{legacy}@{serial}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lantern-naming-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn primary(serial: &str) -> Naming {
        Naming {
            primary: Some(serial.into()),
        }
    }

    #[test]
    fn tag_keeps_alnum_and_underscore() {
        assert_eq!(tag("192.168.1.5:5555"), "192_168_1_5_5555");
        assert_eq!(tag("ZD222LMDFV"), "ZD222LMDFV");
        assert_eq!(tag("a_b-c/../é"), "a_b_c_____");
        assert_eq!(tag(&"x".repeat(100)), "x".repeat(64));
        assert_eq!(category("192.168.1.5:5555"), "X-Lantern-192_168_1_5_5555");
        assert_eq!(directory_file("20e9318"), "lantern-phone-20e9318.directory");
    }

    #[test]
    fn app_id_primary_is_legacy() {
        let n = primary("P");
        assert_eq!(n.app_id("com.a", 10, "P"), "lantern.com.a-u10");
        assert_eq!(n.app_id("com.a", 0, "P"), "lantern.com.a");
        // The primary's names are exactly today's.
        assert_eq!(
            n.app_id("com.a", 10, "P"),
            crate::window::identity::app_id("com.a", 10)
        );
    }

    #[test]
    fn app_id_other_phone_suffix() {
        let n = primary("P");
        assert_eq!(
            n.app_id("com.a", 10, "ZD222LMDFV"),
            "lantern.com.a-u10-pZD222LMDFV"
        );
        assert_eq!(n.app_id("com.a", 0, "20e9318"), "lantern.com.a-p20e9318");
        assert_eq!(
            n.app_id("com.a", 0, "192.168.1.5:5555"),
            "lantern.com.a-p192_168_1_5_5555"
        );
    }

    #[test]
    fn size_key_primary_legacy_else_at_serial() {
        let n = primary("P");
        assert_eq!(n.size_key("com.a", 10, "P"), "com.a:10");
        assert_eq!(n.size_key("com.a", 0, "P"), "com.a");
        assert_eq!(n.size_key("com.a", 10, "20e9318"), "com.a:10@20e9318");
        assert_eq!(n.size_key("com.a", 0, "20e9318"), "com.a@20e9318");
    }

    #[test]
    fn no_primary_means_every_phone_suffixed() {
        let n = Naming::default();
        assert_eq!(n.app_id("com.a", 0, "P"), "lantern.com.a-pP");
        assert_eq!(
            n.app_id("com.a", 10, "20e9318"),
            "lantern.com.a-u10-p20e9318"
        );
        assert_eq!(n.size_key("com.a", 0, "P"), "com.a@P");
        assert_eq!(n.size_key("com.a", 10, "P"), "com.a:10@P");
    }

    #[test]
    fn naming_load_missing_or_bad_is_default() {
        let state = scratch("bad");
        assert_eq!(Naming::load(&state), Naming::default());
        assert!(!Naming::migrated(&state));
        std::fs::create_dir_all(&state).unwrap();
        for bad in [
            "",
            "not json",
            "[1]",
            r#"{"version":2,"primary":7}"#,
            r#"{"version":2,"primary":""}"#,
        ] {
            std::fs::write(state.join("menu.json"), bad).unwrap();
            assert_eq!(Naming::load(&state), Naming::default(), "{bad:?}");
        }
        std::fs::write(state.join("menu.json"), r#"{"version":1,"primary":"P"}"#).unwrap();
        assert!(!Naming::migrated(&state), "version 1 is not migrated");
        std::fs::write(state.join("menu.json"), r#"{"version":3}"#).unwrap();
        assert!(Naming::migrated(&state));
        assert_eq!(Naming::load(&state), Naming::default());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn save_load_roundtrip() {
        let state = scratch("roundtrip");
        let n = primary("ZD222LMDFV");
        n.save(&state).unwrap();
        assert_eq!(Naming::load(&state), n);
        assert!(Naming::migrated(&state));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.join("menu.json")).unwrap())
                .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"version": 2, "primary": "ZD222LMDFV"})
        );
        // No primary is saved as null and still counts as migrated.
        Naming::default().save(&state).unwrap();
        assert_eq!(Naming::load(&state), Naming::default());
        assert!(Naming::migrated(&state));
        // No temp file is left behind.
        let files: Vec<String> = std::fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, ["menu.json"]);
        let _ = std::fs::remove_dir_all(&state);
    }
}
