//! Which phone app a window shows: package + Android user (profile), and the per-window names
//! derived from it (Wayland app_id, title, remembered-size key, CLI arguments).

/// `lantern open [--user N] <pkg>`. Android user ids are `0..=i32::MAX` (the phone's `int`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub pkg: String,
    pub user: u32,
}

pub const MAX_USER: u32 = i32::MAX as u32;

/// Wayland app_id (and `.desktop` file stem / `StartupWMClass`) of `pkg` in Android user `user`.
/// User 0 keeps `lantern.<pkg>`; another user gets `lantern.<pkg>-u<N>`. `-` never occurs in an
/// Android package name, so no package can produce another user's id.
pub fn app_id(pkg: &str, user: u32) -> String {
    match user {
        0 => format!("lantern.{pkg}"),
        n => format!("lantern.{pkg}-u{n}"),
    }
}

/// Longest label (in chars) Lantern shows in a title or menu entry.
pub const MAX_LABEL: usize = 200;

/// ` (work)` for a work-profile app, ` (user N)` for another non-owner user, else nothing.
pub fn profile_suffix(user: u32, work: bool) -> String {
    match (work, user) {
        (true, _) => " (work)".to_owned(),
        (false, 0) => String::new(),
        (false, n) => format!(" (user {n})"),
    }
}

impl Identity {
    /// Parses the arguments after `open`: `[--user N | --user=N] [--] <pkg>`. After `--` every
    /// argument is the package. `Err` is a usage error.
    pub fn parse_args(args: &[&str]) -> Result<Self, String> {
        let mut user: Option<u32> = None;
        let mut pkg: Option<&str> = None;
        let mut it = args.iter().copied();
        let mut options = true;
        while let Some(arg) = it.next() {
            if options && arg == "--" {
                options = false;
                continue;
            }
            let value = if !options {
                None
            } else if arg == "--user" {
                Some(it.next().ok_or("--user needs a number")?)
            } else {
                arg.strip_prefix("--user=")
            };
            if let Some(v) = value {
                if user.is_some() {
                    return Err("--user given twice".into());
                }
                // Plain decimal only: no sign, no leading zeros (u32::parse accepts "+10", "010").
                let canonical = !v.is_empty()
                    && v.bytes().all(|b| b.is_ascii_digit())
                    && (v == "0" || !v.starts_with('0'));
                let n: u32 = v
                    .parse()
                    .ok()
                    .filter(|n| canonical && *n <= MAX_USER)
                    .ok_or_else(|| {
                        format!("--user {v:?}: not an Android user id (0..={MAX_USER})")
                    })?;
                user = Some(n);
            } else if arg.starts_with('-') {
                return Err(format!("unknown option {arg:?}"));
            } else if arg.is_empty() {
                return Err("empty package name".into());
            } else if !arg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
            {
                // Android package names use only these; the app_id/size-key schemes rely on it.
                return Err(format!("{arg:?} is not an Android package name"));
            } else if pkg.replace(arg).is_some() {
                return Err("only one package".into());
            }
        }
        Ok(Self {
            pkg: pkg.ok_or("missing package")?.to_owned(),
            user: user.unwrap_or(0),
        })
    }

    /// Wayland app_id: see [`app_id`].
    pub fn app_id(&self) -> String {
        app_id(&self.pkg, self.user)
    }

    /// Window title: the app's launcher label (control characters dropped, capped so the whole
    /// title is at most [`MAX_LABEL`] chars), or the package when the label is unknown or empty,
    /// plus [`profile_suffix`].
    pub fn title(&self, label: Option<&str>, work: bool) -> String {
        let suffix = profile_suffix(self.user, work);
        let room = MAX_LABEL.saturating_sub(suffix.chars().count());
        let clean: String = label
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(room)
            .collect();
        let name = if clean.is_empty() { &self.pkg } else { &clean };
        format!("{name}{suffix}")
    }

    /// One stderr line when the phone moved an already-open task into this window (`opened.moved`).
    pub fn moved_notice(&self) -> String {
        format!(
            "lantern: {} (user {}) was already open on the phone; it moved into this window and closing the window will quit it",
            self.pkg, self.user
        )
    }

    /// Key in `windows.json`. User 0 keeps the bare package (files from before profiles).
    pub fn size_key(&self) -> String {
        match self.user {
            0 => self.pkg.clone(),
            n => format!("{}:{n}", self.pkg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(pkg: &str, user: u32) -> Identity {
        Identity {
            pkg: pkg.into(),
            user,
        }
    }

    #[test]
    fn title_uses_label_and_suffixes() {
        assert_eq!(id("com.a", 0).title(Some("Settings"), false), "Settings");
        assert_eq!(id("com.a", 10).title(Some("Files"), true), "Files (work)");
        assert_eq!(
            id("com.a", 11).title(Some("Files"), false),
            "Files (user 11)"
        );
        assert_eq!(id("com.a", 0).title(Some("Files"), true), "Files (work)");
        // Empty (or control-only) labels are unknown: the package stands in.
        assert_eq!(id("com.a", 0).title(Some(""), false), "com.a");
        assert_eq!(id("com.a", 10).title(Some(""), true), "com.a (work)");
        assert_eq!(id("com.a", 10).title(None, false), "com.a (user 10)");
        // Control characters are dropped and the label is capped.
        assert_eq!(id("com.a", 0).title(Some("A\nB\u{7}"), false), "AB");
        let long = "z".repeat(1000);
        assert!(id("com.a", 0).title(Some(&long), false).chars().count() <= 200);
    }

    #[test]
    fn app_id_is_shared() {
        assert_eq!(app_id("com.a", 0), id("com.a", 0).app_id());
        assert_eq!(app_id("com.a", 10), "lantern.com.a-u10");
        assert_eq!(
            crate::daemon::catalog::app_id("com.a", 10),
            "lantern.com.a-u10"
        );
    }

    #[test]
    fn moved_notice_names_pkg_and_user() {
        assert_eq!(
            id("com.google.android.apps.nbu.files", 10).moved_notice(),
            "lantern: com.google.android.apps.nbu.files (user 10) was already open on the phone; \
             it moved into this window and closing the window will quit it"
        );
    }

    #[test]
    fn parses_user_forms() {
        assert_eq!(Identity::parse_args(&["com.a"]), Ok(id("com.a", 0)));
        assert_eq!(
            Identity::parse_args(&["--user", "10", "com.a"]),
            Ok(id("com.a", 10))
        );
        assert_eq!(
            Identity::parse_args(&["--user=10", "com.a"]),
            Ok(id("com.a", 10))
        );
        assert_eq!(
            Identity::parse_args(&["com.a", "--user", "0"]),
            Ok(id("com.a", 0))
        );
        assert_eq!(
            Identity::parse_args(&["--user", "2147483647", "com.a"]),
            Ok(id("com.a", MAX_USER))
        );
    }

    #[test]
    fn double_dash_ends_the_options() {
        assert_eq!(Identity::parse_args(&["--", "com.a"]), Ok(id("com.a", 0)));
        assert_eq!(
            Identity::parse_args(&["--user", "10", "--", "com.a"]),
            Ok(id("com.a", 10))
        );
        for bad in [
            &["--"][..],
            &["--", "--user", "10"],
            &["--", "-x"],
            &["--", "com.a", "com.b"],
            &["--", "com.a", "--"],
        ] {
            assert!(
                Identity::parse_args(bad).is_err(),
                "{bad:?} must be a usage error"
            );
        }
    }

    #[test]
    fn rejects_bad_user_and_args() {
        for bad in [
            &["--user"][..],
            &["--user", "com.a"],
            &["com.a", "--user"],
            &["--user", "-1", "com.a"],
            &["--user", "x", "com.a"],
            &["--user", "2147483648", "com.a"],
            &["--user", "1", "--user", "2", "com.a"],
            &["--uesr", "1", "com.a"],
            &["com.a", "com.b"],
            &["--user", "+10", "com.a"],
            &["--user", "010", "com.a"],
            &["--user=00", "com.a"],
            &["--user", " 1", "com.a"],
            &["com.a-u10"],
            &["com a"],
            &["com/a"],
            &["com.á"],
            &[""],
            &[],
        ] {
            assert!(
                Identity::parse_args(bad).is_err(),
                "{bad:?} must be a usage error"
            );
        }
    }

    #[test]
    fn user_zero_names_are_unchanged() {
        let i = id("com.android.settings", 0);
        assert_eq!(i.app_id(), "lantern.com.android.settings");
        assert_eq!(i.title(None, false), "com.android.settings");
        assert_eq!(i.size_key(), "com.android.settings");
    }

    #[test]
    fn other_users_get_distinct_names() {
        let i = id("com.android.settings", 10);
        assert_eq!(i.app_id(), "lantern.com.android.settings-u10");
        assert_eq!(i.title(None, false), "com.android.settings (user 10)");
        assert_eq!(i.size_key(), "com.android.settings:10");
        // No package (letters, digits, `_`, `.`) can make user 0's app_id equal another user's.
        assert_ne!(id("com.a.u10", 0).app_id(), id("com.a", 10).app_id());
        assert_ne!(id("com.a", 0).size_key(), i.size_key());
    }
}
