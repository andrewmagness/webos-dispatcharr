# Telemetry was removed (and how to bring it back)

**Decision, 2026-10-18 (owner):** remove telemetry for now. Dispatcharr TV 0.1.0 ships with no
crash reporting, no error reporting and no product analytics. Nothing leaves the television except
the requests the app makes to the user's own Dispatcharr server (and the public logo URLs that
server's channels point at). This note records what existed, why it went, and what reinstating it
would take, so the question can be revisited without archaeology.

## What was removed

| Piece | Where it lived | What it did |
|---|---|---|
| `plx_telemetry` crate | `rust-modules/telemetry/` | consent store and machine, the opt-in crash and usage channels, the durable spool and send worker, the typed usage-event schema (`diag::schema`), the handled playback-error report, the native-crash importer and the window breadcrumbs |
| Sentry Native | `libsentry.a` + `libunwind.a` linked into `pkg/dispatcharrtv`, the `sentry-crash` handler shipped beside it, `src/sentry_context.c`, the spool re-entry in `src/main.c` | async-signal-safe native crash capture and an out-of-process ARM stack walk, spooled for the app to send later |
| PostHog | `telemetry::posthog` | typed usage events (`app.launch`, `route.entered`, `feature.used`, the playback funnel) |
| The playback report | `media::player::report` | the usage funnel (`playback.requested/started/failed/…`) and the handled error report with its 32-step trace; the attempt origin (`fresh` / `user_retry` / `recovery` / `resume`) passed through `route::request_live` |
| Release and CI gates | `.github/workflows/build-package.yml` (and its `telemetry:` / `upload-debug-files:` inputs from `release.yml`, `rc.yml`, `nightly.yml`), `ci/check-debug-files.py`, `make sentry-symbols`, `make telemetry-local` | a production build refused to package without the `PLX_SENTRY_DSN` / `PLX_POSTHOG_KEY` repository variables; debug files were uploaded to Sentry |
| Privacy copy | `PRIVACY.md`'s reporting sections | the field-by-field description of every report, with the generated event table |

**Kept**, because they never left the television: the event log, the error-output log, the
append-only crash log written by `src/crashtrace.c` (now also stamped with the binary's build id by
`rust-modules/src/crash_image.rs`, so a crash can be symbolicated offline against
`pkg/dispatcharrtv.debug`), `make symbols` / `make symbols-libs`, and the `dispatcharrtv-stackwalk`
dev sampler (which still links the remote `libunwind` from the pinned Sentry Native build — the app
itself links none of it).

## Why

The release workflow would not build without Sentry and PostHog keys, and no accounts had been set
up. The app had no consent screen (the Plex flow that asked was removed), so even a build carrying
keys sent nothing. Keeping a dormant subsystem meant carrying its gates, its privacy copy and a
crash handler in every package for no benefit. The owner chose removal over setting up accounts or
leaving it dormant.

## Bringing it back

The last tree that had all of it is `main` at `0201741b` (before the removal PR). The removal is one
squash commit; `git revert` of it is the starting point, then:

1. **Accounts.** Sentry and PostHog projects in the **EU** region (the privacy copy promised EU, and
   `telemetry::sentry` refused a non-EU DSN). Set `PLX_SENTRY_DSN`, `PLX_POSTHOG_KEY` (and the
   `_DEV` pair for nightly) as repository **variables**, and `SENTRY_AUTH_TOKEN` as a **secret**.
2. **A consent screen.** Owner decision (2026-10-16): ask **once**, on the first launch after Setup
   completes, reusing the consent preview (`telemetry::playback::preview_event` and
   `preview_domains`), and keep the choice changeable later. Nothing is sent until it is answered.
3. **The payload.** It had already been trimmed to what live TV can say (owner decision "option A",
   2026-10-16, PR #36): no delivery, transcode, decision-refusal or Plex Pass fields.
4. **PRIVACY.md.** Restore the reporting sections from `0201741b` and regenerate the event table
   (`diag::schema` tests fail until the document matches).
5. **Firmware review.** Re-linking Sentry Native changes `DT_NEEDED`; run `tools/fwcompat.py` and the
   `fw-compat-reviewer` review before shipping.
