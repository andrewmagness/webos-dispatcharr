//! Who this app says it is: its product token, its name as a person reads it, its version and the
//! `User-Agent` every HTTP request carries (the Dispatcharr API over the socket transport and the
//! media plane over libcurl).
//!
//! It lived in the Plex client layer, which sent the same identity as `X-Plex-*` headers; it moved
//! here when that layer stopped being the only one that spoke HTTP. The two rules it was written
//! for still hold:
//!
//! - **An unofficial client says so.** The product is `DispatcharrTV`, never `Plex for webOS`
//!   (Plex's own first-party naming pattern) or anything else a server operator could mistake for
//!   another vendor's app.
//! - **No claim about one particular television.** The device is the class, `LG webOS TV`, never
//!   the model the app happened to be developed against.

/// The product token: what the `User-Agent` carries. One token (no space), because a `User-Agent`
/// product is one.
pub const PRODUCT: &str = "DispatcharrTV";

/// The app's name as a person reads it: the failure read-out's footer, the diagnostics panel.
pub const DISPLAY_NAME: &str = "Dispatcharr TV";

/// Device CLASS — what kind of thing this is. Generic on purpose: the app runs on any rooted webOS
/// panel, not on the model it was developed against.
pub const DEVICE: &str = "LG webOS TV";

/// The app version, handed in by the application: no layer crate can see `PLX_VERSION`, because a
/// `cargo:rustc-env` reaches one crate only and the rule that derives it (`rust-modules/build.rs`)
/// is written exactly once. The application passes `env!("PLX_VERSION")` to [`set_version`] before
/// its first request.
///
/// **A release reports the package version exactly; anything else reports the version it is
/// heading for with a `-dev` suffix.** `rust-modules/build.rs` is where that rule is written and
/// why, and the application crate's `release_line::tests` grade it. Every surface that reports a
/// version — this `User-Agent`, the telemetry release, the diagnostics header — reads the same
/// `PLX_VERSION`, so they cannot drift apart.
static VERSION: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Record the application's version. Idempotent for the same string; a second, different one is a
/// wiring bug and is refused loudly in a debug build rather than silently re-labelling the app.
pub fn set_version(version: &'static str) {
    let kept = VERSION.get_or_init(|| version);
    debug_assert_eq!(*kept, version, "identity::set_version called with two versions");
}

/// What this app reports as its version.
///
/// Before [`set_version`] a test build answers a fixed placeholder (a layer's own tests build their
/// requests without an application). A shipping build that gets here first is a wiring bug, and it
/// **panics in every profile** rather than answer: the television's build is `--release`, where a
/// `debug_assert!` is compiled out, so a placeholder there would be sent as this app's version with
/// nothing to say it had happened. The order that makes this unreachable (`set_version` is the
/// first statement of `app::enter_application`, the one entry point) is held by
/// `app::boot::seam_order_tests`.
pub fn version() -> &'static str {
    match VERSION.get() {
        Some(v) => v,
        None => {
            #[cfg(any(test, feature = "test-support"))]
            {
                "0.0.0-test"
            }
            #[cfg(not(any(test, feature = "test-support")))]
            {
                panic!("identity::version read before set_version: no request may report an unset version")
            }
        }
    }
}

/// The HTTP `User-Agent`: `DispatcharrTV/<version> (LG webOS TV)`.
pub fn user_agent() -> String {
    format!("{PRODUCT}/{} ({DEVICE})", version())
}

#[cfg(test)]
mod tests {
    /// An unofficial client must not present as a first-party one of another vendor.
    #[test]
    fn identity_never_claims_to_be_another_vendors_app() {
        for s in [super::PRODUCT, super::DISPLAY_NAME, super::DEVICE, &super::user_agent()] {
            assert!(!s.to_ascii_lowercase().contains("plex"), "{s:?}");
        }
    }

    /// The `User-Agent` names the product, the version and the device class, in that order.
    #[test]
    fn the_user_agent_is_product_version_and_device() {
        let ua = super::user_agent();
        assert!(ua.starts_with("DispatcharrTV/"), "{ua}");
        assert!(ua.contains(super::version()), "{ua}");
        assert!(ua.ends_with("(LG webOS TV)"), "{ua}");
    }
}
