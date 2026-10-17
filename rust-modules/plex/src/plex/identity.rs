//! Who this client says it is — ONE set of constants for both Plex services.
//!
//! There are two independent transports here and they used to each hardcode their own identity:
//! [`account`](super::account) talks to plex.tv over libcurl and sends `X-Plex-*` as HEADERS, while
//! [`client`](super::client) talks to the PMS and sends them as QUERY PARAMETERS. Nothing kept the
//! two in step, and they had drifted on every field that mattered:
//!
//! | field       | plex.tv said        | the PMS said     |
//! |-------------|---------------------|------------------|
//! | Product     | `Plex for webOS`    | `Dispatcharr TV`      |
//! | Version     | `1.0`               | `0.1.0`          |
//! | Device      | `LG TV`             | `webOS`          |
//! | Device-Name | `Plex (LG webOS)`   | `Living Room TV` |
//! | Model       | `LG webOS TV`       | `49SM9000PLA`    |
//!
//! Two of those were release blockers rather than untidiness.
//!
//! **`Plex for webOS` is impersonation.** `Plex for <platform>` is Plex's own documented
//! first-party naming pattern (their sample value is `Plex for Roku`), and it appeared verbatim in
//! the authorized-devices list of the account this app signs into — on a platform where an official
//! Plex app exists. A server owner had no way to tell an unofficial client from Plex's own. This is
//! an unofficial client and must say so; that is also what makes the referential use of the Plex
//! name elsewhere defensible.
//!
//! **`49SM9000PLA` is the author's television.** It was accurate for exactly one device on earth
//! and reported as fact by every install.
//!
//! `Living Room TV` was the third: not an impersonation, but a claim about a room this app cannot
//! see, and identical across every install, so two users of the same shared server appeared under
//! one name.
//!
//! The client IDENTIFIER is deliberately not here — it is per-install, not per-build. It is minted
//! once from `/dev/urandom` and persisted by [`session`](super::session).

// The app-wide half of this identity (product, display name, version, device class, User-Agent)
// lives in `plx_base::identity`; the X-Plex-* headers below read it from there.
pub use plx_base::identity::{set_version, user_agent, version, DEVICE, DISPLAY_NAME, PRODUCT};

pub const PLATFORM: &str = "webOS";

/// The OS version — the REAL one, read off the set at boot ([`plx_platform::tv::device`]), because the
/// app runs on webOS 4 through 11 now and a literal is wrong on every set but one. This was
/// `const … = "4.5"` while the app was packaged `>=4.0, <5.0`; the webosbrew reviewer flagged it
/// reporting 4.5 from a 6.5.2 television (issue #22). PMS augments our named Generic profile
/// from the X-Plex-Client-Profile-Extra we send, so the version is informational today — but it
/// is also how a server-side profile could ever distinguish firmware generations, and a false
/// one poisons that forever. The fallback when `os_info.json` is unreadable keeps the literal
/// this replaces — the exact claim every release so far has made — rather than inventing an
/// empty-string case no server has ever been shown. Safe by boot order: `tv::probe_device()` runs
/// before any PMS request, so none can precede the read.
pub fn platform_version() -> &'static str {
    let r = &plx_platform::tv::device::info().release;
    if r.is_empty() {
        "4.5"
    } else {
        r
    }
}

/// The resolved UI language for this launch, also requested from Plex metadata endpoints.
pub fn language() -> Option<&'static str> {
    Some(plx_platform::i18n::current().language().tag())
}

pub const MODEL: &str = "LG webOS TV";

/// Who MADE the hardware. The one field in this file that is a fact about the panel rather than a
/// claim about this app, and it is safe to state because it is not a choice: the binary is
/// cross-compiled for LG's webOS, links LG's `libplayerAPIs`, and starts on nothing else.
///
/// It rides the **plex.tv** headers only. That surface is the account's authorized-device list,
/// where a user picks their television out of a column of them and revokes it — the place the
/// vendor is worth reading. PMS is told what it acts on instead (`Client::playback_identity`), and
/// it acts on the codec profile, not on who built the set. See that method's doc for the split.
pub const VENDOR: &str = "LG";

/// The FRIENDLY name, which is what a user actually reads in plex.tv's device list and in the
/// server's Now Playing. Names the app rather than a room, so it is true on every install and
/// distinguishable from an official client sharing the same TV.
///
/// **A flavoured install says so.** Two builds can sit on one television now
/// ([`plx_base::paths::app_id`]) and they hold separate session files, so each mints its own
/// `X-Plex-Client-Identifier` and each appears as its own authorized device. Without the suffix
/// the account grows two entries spelled identically, and revoking "the one on the TV" is a
/// coin flip. The shipped app's name is unchanged, which matters because it is already in every
/// existing user's device list — a rename there would read as a new, unknown device.
pub fn device_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| match plx_base::paths::flavour() {
        None => "Dispatcharr TV (LG TV)".to_string(),
        Some(f) => format!("Dispatcharr TV {f} (LG TV)"),
    })
}

/// What this client offers the network. It plays; it is not a controller and not a server.
pub const PROVIDES: &str = "player";

#[cfg(test)]
mod tests {
    /// The point of the module: an unofficial client must not present as a first-party one.
    /// `Plex for <platform>` is Plex's documented pattern for its OWN apps.
    #[test]
    fn identity_never_claims_to_be_plex() {
        for s in [
            super::PRODUCT,
            super::DEVICE,
            super::device_name(),
            super::MODEL,
            &super::user_agent(),
        ] {
            let low = s.to_ascii_lowercase();
            assert!(
                !low.starts_with("plex for"),
                "{s:?} uses Plex's own first-party naming pattern"
            );
            assert!(
                !low.starts_with("plexfor"),
                "{s:?} uses Plex's own first-party naming pattern"
            );
        }
    }

    /// The developer's own panel must not be reported as every user's hardware.
    #[test]
    fn no_specific_model_is_asserted() {
        for s in [
            super::DEVICE,
            super::MODEL,
            super::device_name(),
            super::VENDOR,
        ] {
            assert!(
                !s.contains("49SM9000"),
                "{s:?} names the author's television"
            );
        }
    }

    /// The vendor is the panel's, and it is the ONE identity field that is not this app's to
    /// choose — the binary starts on LG's webOS and on nothing else. Pinned so that "make the
    /// identity honest" can never be read as a reason to blank it: an empty header value is a
    /// claim too, and a wrong one.
    #[test]
    fn the_vendor_names_the_hardware_this_binary_runs_on() {
        assert_eq!(super::VENDOR, "LG");
        assert!(!super::VENDOR.to_ascii_lowercase().starts_with("plex"));
    }

    /// The shipped app's device name must not move — it is already in every existing user's
    /// authorized-device list, and a rename there reads as a new, unknown device. A flavoured
    /// install must move, or the two separate sign-ins are indistinguishable in that list.
    #[test]
    fn only_a_flavoured_install_renames_the_device() {
        let name = super::device_name();
        match plx_base::paths::flavour() {
            None => assert_eq!(name, "Dispatcharr TV (LG TV)"),
            Some(f) => assert!(name.contains(f), "{name:?} does not name the {f} install"),
        }
        assert!(name.starts_with("Dispatcharr TV"));
    }

    /// On the host there is no `/var/run/nyx/os_info.json`, so this exercises exactly the
    /// unreadable-file path a television would hit: the fallback is the literal every release
    /// so far reported, not an empty string no server has ever been shown. (The real-version
    /// path can only be seen on a set — issue #22's reviewer saw "4.5" from webOS 6.5.2, which
    /// is the bug this function fixes.)
    #[test]
    fn unknown_firmware_falls_back_to_the_old_literal() {
        assert_eq!(super::platform_version(), "4.5");
    }

    #[test]
    fn a_process_locale_becomes_a_safe_plex_language_tag() {
        assert_eq!(
            plx_platform::i18n::normalize("en_US.UTF-8"),
            Some("en-US".into())
        );
        assert_eq!(
            plx_platform::i18n::normalize("mn_Cyrl_MN.UTF-8"),
            Some("mn-Cyrl-MN".into())
        );
        assert_eq!(plx_platform::i18n::normalize("pt-BR"), Some("pt-BR".into()));
        assert_eq!(plx_platform::i18n::normalize("C.UTF-8"), None);
        assert_eq!(plx_platform::i18n::normalize("POSIX"), None);
        assert_eq!(
            plx_platform::i18n::normalize("en_US\r\nX-Plex-Token: stolen"),
            None
        );
    }
}
