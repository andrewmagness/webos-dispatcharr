# Dispatcharr TV Privacy Policy

Applies to Dispatcharr TV 0.1.0. Last updated 19 October 2026.

## The short version

Dispatcharr TV sends nothing to its developer or to any analytics or crash-reporting service. It
talks only to the Dispatcharr server you enter in Setup, and everything it keeps stays on your
television.

## Who is responsible for Dispatcharr TV data

The maintainer of this repository is responsible only for the data Dispatcharr TV stores on your
television. Contact: the repository's issue tracker,
<https://github.com/andrewmagness/webos-dispatcharr/issues> (keep personal data out of a public issue).

## Your Dispatcharr server

To sign in, list channels, read the programme guide, show channel logos and play live TV,
Dispatcharr TV communicates directly with the Dispatcharr server you enter in Setup, and fetches
the logo images your server's channels point at. Those requests are handled by that server and its
operator (or the logo's host). Dispatcharr TV's developer does not receive them. Your password is
sent to the server once, to sign in, and is not stored.

## Data stored on this television

Dispatcharr TV stores:

- **Your account**: the server's address, the username you signed in with and the API key the
  server issued, in one file readable only by the app (mode `0600`; the app restores that mode if
  something on the television widens it).
- **Your choices for that server**: your favourite channels, the channel you last watched, and
  whether the app starts on it. Channels are named by the server's identifiers for them, never by
  what was on.
- **Channel logos**, cached as bounded files for reuse across restarts.
- **Local technical logs**: an event log and an error-output log, each emptied every time the app
  launches, a crash log that is kept until the television restarts, and a bounded storage status
  snapshot. They exist so a problem can be diagnosed by someone with access to your television; the
  app never sends them anywhere. The event log redacts your username.

Dispatcharr TV has a developer-only input/frame recorder used to reproduce and test bugs. It is not
part of the app you installed: a release build has the developer-trigger inputs, including the
recorder, compiled out.

Signing out (Setup) removes the account file. Your channel choices are not a credential and are
kept, so signing back in to the same server finds them; signing in to a different server starts
with none. **webOS gives an application no way to run code as it is removed**, so these files can
survive an uninstall.

## No reporting

This version contains no crash reporting, error reporting or usage analytics, and no endpoint for
any such service. A release audit (`docs/release-audits/`) records that the packaged binary carries
none. Earlier development builds had opt-in reporting that was never switched on;
`docs/telemetry-removed.md` records what it was and why it was removed.

## Contact and non-affiliation

Privacy questions may be asked on the repository's issue tracker. Security vulnerabilities may be reported
privately through GitHub Security Advisories for `andrewmagness/webos-dispatcharr`.

Dispatcharr TV is an independent, unofficial application. It is not produced by, endorsed by, or
affiliated with the Dispatcharr project or LG Electronics Inc.
