# Placeholders — the stand-ins for anything that is not ours to publish

This repository is public. Addresses, ports, API keys, tokens, machine identifiers, usernames and
library or channel names from a real server — the owner's, or anyone's who granted access — are never
recorded in code, fixtures, doc comments, commit messages, PR bodies or release notes. Use the
stand-ins below. `.claude/hooks/outbound-guard.py` and `tools/scrub-gate.py` enforce the rule; this
table is what they tell you to use instead.

| real thing | stand-in |
|---|---|
| a person's handle or username | `friend` |
| a server / machine name | `nas-home` |
| a library or channel-group name | `Film Club` |
| a non-default port | `31234` |
| a LAN address | `10.9.9.7` (RFC1918, as a real one is) |
| any public address | `203.0.113.9` / `198.51.100.7` (TEST-NET-3 / TEST-NET-2) |
| a machine identifier | `aaaabbbb…` runs, never a real 40-hex id |
| an API key or token | `SESSIONKEY` (what `tests/mock_dispatcharr.py` and `tools/tv-session.sh` use) |

Several are deliberately the **same character length** as values they once replaced, because tests
assert text widths against them. The live values live only in gitignored files (`.tv-host`,
`src/config.local.h`, `tests/manifest.local.json`, the account file on the set), which is the whole
reason those files are gitignored.

**Why the table exists.** It was first written (in the since-retired `docs/shared-servers.md`) after
the repository had published a friend's handle, machine name, library name, real port and LAN address
across ~139 sites in committed code and four PR bodies (2026-08-14), while a paragraph saying not to
stood at the top of the same document. Stating the rule did not apply it; the stand-ins and the guard
do.
