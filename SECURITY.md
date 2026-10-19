# Security policy

## Reporting a vulnerability

Report privately, **not** as a public issue, through **GitHub Security Advisories**:
<https://github.com/andrewmagness/webos-dispatcharr/security/advisories/new>. It is private, it
threads, and it produces a CVE if one is warranted.

This is a one-person unpaid project, so the honest service level is: acknowledged within **7 days**,
an assessment within **30**. If you have not heard back in a week, open a public issue saying only
*"sent a security report on <date>, no reply"*, with no details.

Please allow a reasonable window for a fix before disclosing. There is no bounty; you will be
credited in the release note unless you ask not to be.

## What is in scope

The app, its packaging, and the host-side tools in `tools/` and `ci/`. The things worth looking at:

- **The developer trigger files.** The app's runtime directory under `/tmp` is mode 1777 in webOS's
  production jail, so any co-resident process can create files there. A developer build reads
  several dozen trigger files from it that change behaviour, and some of them replace the stream or
  the server the app talks to. **All of it is compiled out of a release build** by dropping the
  `devtriggers` cargo feature, and `ci/check-elf.sh` measures that on the shipped bytes rather than
  asserting it. A release binary that still carries any of it is a valid report, and a serious one.
- **The logs.** The event log, the crash log and the stderr log in the runtime directory are created
  0640 (owner read/write, the app's own group read, never world-readable), so a Developer Mode user
  can fetch them with webOS Dev Manager on a television that is not rooted. Every event-log line goes
  through `eventlog::scrub::scrub_local` before the write, and so does the panic text the Rust panic
  hook appends to the crash log. A line that reaches a log carrying a password, an API key, a token
  or a server address is a valid report. All three files are opened with `O_NOFOLLOW` as a regular
  file this app owns with exactly one link, so a symlink or a hard link planted at their names is
  refused and never chmod'ed or truncated. The residual exposure, stated plainly: another native app
  on the same television that runs in the app's group can read all three files, and the stderr log
  (whatever aborts and the television's own libraries print), the crash log's C-side records (a
  faulting address, registers, the crash's wall-clock second and the memory-map lines around the
  fault) and the C-side lines the boot shim and the Starfish seam write into the event log do not
  pass through that scrubber. A credential in any of them is a valid report too.
- **The Dispatcharr account.** Signing in keeps the server address and an API key, nothing else:
  the password and the short-lived login token are discarded once the key is obtained. The record
  is one JSON file, written whole-old-or-whole-new and created mode 0600. A way to read it from
  another process, or to make the app write it somewhere world-readable, is in scope.
- **Transport.** An `https://` server is verified against the television's trust store, with
  certificate and host-name checks on, through the firmware's libcurl. An `http://` server is
  plaintext, which is how most Dispatcharr servers on a home network are reached: the API key then
  crosses the network in the clear, as it does for any other Dispatcharr client using that address.
  Anything that disables or bypasses verification for an `https://` server is in scope, including a
  path where a failure to *set* a security option results in a request going out anyway.
- **The bundled FFmpeg.** Built from unmodified FFmpeg with demuxers, parsers and subtitle decoders
  only. It is fed untrusted bytes from the network, so parser bugs reachable through `ff.rs` are in
  scope. Report FFmpeg's own bugs upstream as well.

## What is not in scope

- Post-compromise access by an attacker who already has root on the television. Root is not an app
  prerequisite; a report whose only precondition is an already-rooted OS describes a platform
  compromise rather than an app sandbox escape.
- The webosbrew Homebrew Channel, webOS itself, LG's own libraries, or Dispatcharr. Report those to
  their maintainers.
- Missing hardening that costs nothing to an attacker who is already executing code in the app's
  jail, unless you can show a concrete consequence.

## What this app does not have

No account of its own, no server, no payment path, and no user-generated content. It signs in to
**your** Dispatcharr server and plays **your** channels.

The app has no crash reporting, error reporting or usage analytics, and sends nothing to its
developer ([PRIVACY.md](PRIVACY.md)).
