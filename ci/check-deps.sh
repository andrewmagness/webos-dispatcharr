#!/usr/bin/env bash
# Structure gates for the UI restructure (spec §15.2), as greps over rust-modules/src. Each rule
# is either ZERO outside a named set of files or ALLOWLISTED by file in ci/allow/<rule>.txt, whose
# first line is `# count: N` — the number of entries — and whose entries are repo-relative paths
# with a reason after a tab. `tests/test_harness.py` runs this script and asserts every
# allowlist's count equals its entries, so an allowlist grows only by a deliberate edit of both.
#
# Phase 2 rules (the rest of §15.2 land with the phases that make them true):
#   libm     — the transcendental surface outside machine/src/motion.rs (spec §4.2): logical state must
#              integrate with motion.rs's own exp/sin_cos; a render or colour formula is allowlisted.
#   ticks    — `SDL_GetTicks(` only in app/clock.rs (the one door) and diag/heartbeat.rs.
#   fpflags  — no `fp-contract`, `fast-math` or `+fma` in the build configuration.
#   wall     — `Instant::now`/`SystemTime::now`/`.elapsed()` in ui/ and app/ only in instruments.
#   present  — the present gate's worker door: ONE atomic static in machine/src/present.rs and ONE
#              `wake_from_worker`.
#   effect   — `Effect::` spelled nowhere (the enum is `Fx::`, the app's `AppFx::`).
#   sink     — `tv::sink::installed` and `VideoSink` only under player/, tv/, tv.rs and port.rs
#              (step L15: the Starfish/ACB verbs are the player's alone).
#
#   blocking — every production `allow_blocking(` call site is a line of
#              ci/allow/blocking.txt, held as an exact multiset: a new site or a stale line fails.
#
# Phase 8 rule (§14, §6.2):
#   nav      — a screen under rust-modules/screens/src/ never calls `plx_ui::nav::` (`crate::ui::nav::` before the ui split; any
#              function) live; it reads `DrawFrame::{page_alpha,chrome_alpha,view_tab,
#              blur_amount,nav_page_alpha}`, populated once per frame by `app/bridge.rs`'s
#              `Rig::navigation_presentation`. `ui/`'s CONTAINERS (popover.rs, glassload.rs,
#              widgets.rs's chrome helpers, the loop's own app/nav.rs and app/run.rs) still call
#              `ui::nav` directly — that is phase 7/12's boundary, not this one's; this gate
#              scans only `screens/`, where the count is zero.
# Phase 10 rules:
#   layer    — a screen (`screens/`) never names `crate::app::`/`super::app::` (§2.1's table). The
#              other half of that table has been gated since phase 2 (`ui/` names no application
#              type); this half was prose until the argument and the mounter moved into
#              `screens/registry.rs`, which is the boundary §0 criterion 5 stands on. Zero, no
#              allowlist.
#   legacypage — `LegacyPage` is spelled NOWHERE under rust-modules/src (§15.2). The type was a
#              route WORD wearing the `Screen` trait, mounted by `AppArg::Legacy`'s fallback arm,
#              and by phase 9 nothing constructed one — every route mounted an owned screen. A
#              fallback nothing takes is not free: it is what stopped the mounter's match from
#              being exhaustive, so a `Route` added without a screen compiled and mounted a blank
#              page instead of failing to build. Zero, with no allowlist: there is no such thing as
#              a legitimate second one.
#   sibling  — a file under `screens/<a>/` or `screens/<a>.rs` never names `crate::<b>`
#              (in the `plx_screens` crate, `crate::screens::<b>` before Split 13) for a different `<b>` (§2.1, §0 criterion 2): a screen talks to the shared
#              vocabulary (`crate::registry`, which the gate allows by name) and to the
#              container, never to its neighbours — reaching into a sibling is what makes a screen
#              impossible to mount, test or delete on its own. Existing violations are
#              ci/allow/sibling-migration.txt, one per FILE, and that list is empty when the
#              criterion is met. The own-module name is not a violation, and `mod.rs`/a submodule
#              of `screens/<a>/` counts as `<a>`.
#   sessionwrite — a screen (screens/, ui/) never calls `plex::session::load(`. `load` is the
#              BOOT/auth door: it mints a `client_id` when there is none and re-persists a
#              plaintext session, so a read turns into `write_atomic` — a temp file, `sync_all`,
#              a rename and a second `sync_all` on the directory. The read-only door is `peek`.
#              `session.rs` has said "it is not [an acceptable trade] on a path a keypress can
#              reach" and "do not add a per-frame reader of this file" since the two doors were
#              split, and a screen still had it: `screens/settings.rs::signed_in` asked the write
#              door twice per Settings open, which on a television with no usable key manager is
#              two flash writes and four fsyncs on the frame the modal mounts — 150-180 ms of
#              `navcommit` when the flash was slow (`fps:modal-ramp`, device-measured 2026-09-09).
#              Nothing failed: both doors return a `Session` and the difference is invisible at the
#              call site, which is exactly what a grep gate is for. Count is zero.
set -uo pipefail
cd "$(dirname "$0")/.."
SRC=rust-modules/src
# The layer crates already split out of $SRC (docs/module-layers.md, ci/module-layers.ini). The
# rules below that read the WHOLE tree read these too: moving a file into one must not move it out
# of a gate. A rule scoped to a directory of $SRC (ui/, screens/, app/ ...) cannot reach a layer
# below them and stays as it is, UNLESS the files it policed there were moved out: `machine` came
# from ui/, so the rules that read `ui/` read $SRC_MACHINE too, and `ui` is $SRC_UI now (docs/module-layers.md, Split 6). `platform` is the layer that held
# `tv/` and `storage/`, and the rules that name a moved module by path (`crate::tv::window::`,
# `crate::storage::`) now spell it `plx_platform::`. `gfx` held `text.rs`, the one file the textmeasure
# rule exempts, and the rule's pattern now accepts `plx_gfx::text::`. `net` held the loopback fixtures
# the `threads` gate skips as test code; they are `cfg(any(test, feature = "test-support"))` now,
# which it accepts.
SRC_BASE=rust-modules/base/src
SRC_MACHINE=rust-modules/machine/src
SRC_PLATFORM=rust-modules/platform/src
SRC_GFX=rust-modules/gfx/src
SRC_NET=rust-modules/net/src
SRC_UI=rust-modules/ui/src
SRC_DATA=rust-modules/data/src
SRC_MEDIA=rust-modules/media/src
SRC_APPKIT=rust-modules/appkit/src
SRC_SCREENS=rust-modules/screens/src
fails=0
fail() { echo "::error::check-deps: $*"; fails=$((fails+1)); }
ok()   { echo "  ok — $*"; }

# grep_code <pattern> <paths...>: matching lines, minus comment-only lines, as `path:line:text`.
grep_code() {
  local pat="$1"; shift
  # `-H` keeps the promised `path:line:text` shape even when a caller scans one file: GNU grep
  # otherwise omits the path while BSD grep keeps it. `[[:space:]]` is POSIX ERE; do not use `\s`.
  grep -HrnE --include='*.rs' "$pat" "$@" 2>/dev/null \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true
}

# grep_code_owner <pattern>: grep_code "$SRC" for the six owner rules below, from ONE shared pass.
# Each carries a ~100-name alternation that this platform's grep evaluates line by line at ~0.5 s
# a rule over the whole tree. Every line any of them can match names one of the six modules'
# paths (`crate::<module>::` or `stores::<module>::`, the leading part of the pattern), so one
# cheap alternation of just those prefixes keeps the few hundred candidate lines, and each rule
# filters that list with its real pattern. The verdict and the output are unchanged, but the
# prefix alternation must stay a NECESSARY part of every owner pattern, never a loose sample.
OWNER_PREFIX='(crate|plx_data)::(browse|viewstate|person|search|pms|metadata)::|stores::(browse|viewstate|person|search|hubs|metadata)::'
OWNER_LINES="$(grep -HrnE --include='*.rs' "$OWNER_PREFIX" "$SRC" "$SRC_DATA" "$SRC_APPKIT" "$SRC_SCREENS" 2>/dev/null || true)"
grep_code_owner() {
  printf '%s\n' "$OWNER_LINES" | grep -E -- "$1" | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true
}

# strip_strings_and_comments <file>: prints the file, one line per input line (so line numbers of
# the output line up with `sed -n '<n>p'` on the original), with the CONTENT of every
# double-quoted string literal blanked to spaces (quotes kept, so `"foo"` becomes `"   "`, an
# escaped character inside a string counts as one blanked character pair) and everything from an
# unquoted `//` to end of line dropped. Used by gates (`frame`, `tmppath`) that must not fire on a
# call SHAPE that only appears as message text or as a self-test's own expected-string literal —
# a host test that compares against `app/run.rs`'s source as a string (the loop pins in `app/run.rs`
# and `ui/fixture.rs`) is exactly that shape.
# This is character-by-character rather than a same-line regex heuristic for the reason both those
# gates' own comments give: a `//` or a `"` that is itself inside a string must not end the scan
# early, and a multi-token call spelled across a `"..."` boundary must not be reassembled by luck.
# Deliberately not general Rust lexing (raw strings, byte strings, char literals) — every case
# these two gates exist for is an ordinary `"…"` literal or a plain line comment.
strip_strings_and_comments() {
  awk '
  {
    line = $0; out = ""; instr = 0; i = 1; n = length(line)
    while (i <= n) {
      c = substr(line, i, 1)
      if (instr) {
        if (c == "\\") { out = out "  "; i += 2; continue }
        if (c == "\"") { instr = 0; out = out c; i++; continue }
        out = out " "; i++; continue
      }
      if (c == "\"") { instr = 1; out = out c; i++; continue }
      if (c == "/" && substr(line, i+1, 1) == "/") { break }
      out = out c; i++
    }
    print out
  }' "$1"
}

# allowed <rule> <path>: is `path` an entry of ci/allow/<rule>.txt?
# The allowlists are read ONCE, by one `awk`, into a newline-delimited index of `<rule>|<path>`
# keys, and `allowed` is then a `case` — a shell BUILTIN, which forks nothing. The old spelling
# forked a `grep` per candidate LINE and the gates below feed it thousands of them; that, together
# with the per-FILE `awk`+`grep` loops several gates ran over all 420 source files, is what made a
# green run of this script cost 17 s — and `tests/test_harness.py`, which runs it 32 more times to
# prove each gate still catches a planted violation, cost 16 MINUTES of a 20-minute `make check`.
#
# An allowlist entry is `path<TAB>reason`, so the old `^${path}(<TAB>|$)` regex matched exactly the
# first tab-delimited field. That is what the index stores and what the `case` compares, so the
# answer is unchanged — and it is now an exact string comparison rather than a regex, which for a
# path containing `.` is strictly the stricter of the two. The skip pattern is the same one the
# allowlist count rule at the foot of this file uses, so the two cannot disagree about what an
# entry is.
#
# The index is built by `awk` rather than a `while read` + `case` loop because **bash 3.2 — what
# macOS ships, and what `#!/usr/bin/env bash` resolves to here — cannot parse a `case` inside a
# `$( … )` at all**: it scans for the closing paren without parsing, so the `)` ending a case
# pattern terminates the substitution and the `;;` after it is a syntax error. One process reads
# every list, which is what we wanted anyway.
ALLOW_INDEX="
$(awk -F'\t' '
  FNR==1 { rule=FILENAME; sub(/.*\//, "", rule); sub(/\.txt$/, "", rule) }
  /^[[:space:]]*(#|$)/ { next }
  { print rule "|" $1 }
' ci/allow/*.txt)
"
allowed() {
  case "$ALLOW_INDEX" in
    *"
$1|$2
"*) return 0 ;;
  esac
  return 1
}

# Resolve external test modules (including #[path], visibility and intervening attributes)
# and include! files from Rust tokens. Test ownership propagates through descendants; any
# production reference keeps a shared file in the scan. No filename suffix grants an exemption.
wholly_test_files() {
  python3 ci/rust_test_modules.py "$SRC"
  python3 ci/rust_test_modules.py "$SRC_MACHINE"
  python3 ci/rust_test_modules.py "$SRC_PLATFORM"
  python3 ci/rust_test_modules.py "$SRC_GFX"
  python3 ci/rust_test_modules.py "$SRC_NET"
  python3 ci/rust_test_modules.py "$SRC_UI"
  python3 ci/rust_test_modules.py "$SRC_DATA"
  python3 ci/rust_test_modules.py "$SRC_MEDIA"
  python3 ci/rust_test_modules.py "$SRC_APPKIT"
  python3 ci/rust_test_modules.py "$SRC_SCREENS"
}

# is_wholly_test <path>: the `wholly_test` list as a builtin lookup. Three gates below asked this
# question once per candidate file or line with `echo "$wholly_test" | grep -qxF`, which is two
# processes an answer.
WHOLLY_TEST_INDEX=""
wholly_test_index_init() {
  WHOLLY_TEST_INDEX="
$1
"
}
is_wholly_test() {
  case "$WHOLLY_TEST_INDEX" in
    *"
$1
"*) return 0;;
  esac
  return 1
}

# gate <rule> <pattern> <paths...>: every match must be in an allowlisted file.
gate() {
  local rule="$1" pat="$2"; shift 2
  local bad=0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    local p="${line%%:*}"
    if ! allowed "$rule" "$p"; then echo "    $line"; bad=$((bad+1)); fi
  done < <(grep_code "$pat" "$@")
  if [ "$bad" -eq 0 ]; then ok "$rule"; else fail "$rule: $bad line(s) outside ci/allow/$rule.txt"; fi
}

echo "== check-deps =="
wholly_test="$(wholly_test_files)" || { fail "Rust test-module classification failed"; exit 1; }
wholly_test_index_init "$wholly_test"

# libm: the method-call spelling, OUTSIDE machine/src/motion.rs (which owns the integrators and their
# table test); `.log(&…`/`.log("…` is a logger, not a logarithm.
# Wholly-test files (see `wholly_test_files`) are skipped like inline `#[cfg(test)]` blocks: a
# test's reference colour maths is not logical state.
libm_lines=$(grep_code '\.(exp|ln|log|powf|powi|cbrt|sin|cos|tan|atan2|hypot|mul_add|sin_cos)\(' "$SRC" "$SRC_BASE" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS" \
  | grep -vE '\.log\((&|")' | grep -v "^$SRC_MACHINE/motion.rs:")
libm_bad=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  p="${line%%:*}"
  if is_wholly_test "$p"; then continue; fi
  if ! allowed libm "$p"; then echo "    $line"; libm_bad=$((libm_bad+1)); fi
done <<< "$libm_lines"
if [ "$libm_bad" -eq 0 ]; then ok "libm"; else fail "libm: $libm_bad line(s) outside ci/allow/libm.txt"; fi

gate ticks 'SDL_GetTicks\(' "$SRC" "$SRC_BASE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS"
#   wall (widened phase 12, D4): the scope grows from ui/+app/+route/plan.rs to also cover
#              screens/ and stores/ (screens/player/ is a subdirectory of screens/ and so already
#              included) — every screen migrated out of ui/ carries the same "instrument only"
#              rule its old home had. Re-verified clean on 2026-09-10 with no new violation.
gate wall '(Instant::now|SystemTime::now|\.elapsed\(\))' "$SRC_UI" "$SRC_MACHINE" "$SRC_APPKIT" "$SRC/app" "$SRC_MEDIA/route/plan.rs" "$SRC_SCREENS" "$SRC_DATA/stores"

if grep -rnE 'fp-contract|fast-math|\+fma' rust-modules/Cargo.toml rust-modules/build.rs rust-modules/net/Cargo.toml rust-modules/data/Cargo.toml rust-modules/media/Cargo.toml rust-modules/appkit/Cargo.toml rust-modules/screens/Cargo.toml rust-modules/platform/Cargo.toml rust-modules/platform/build.rs rust-modules/gfx/Cargo.toml rust-modules/gfx/build.rs rust-modules/ui/Cargo.toml rust-modules/ui/build.rs rust-modules/appkit/build.rs rust-modules/screens/build.rs rust-modules/storage/Cargo.toml rust-modules/storage/build.rs rust-modules/.cargo Makefile 2>/dev/null | grep -v '^[[:space:]]*#'; then
  fail "fpflags: a floating-point contraction flag is set (spec §4.2 assumes none)"
else ok "fpflags"; fi

n=$(grep -cE '^static [A-Z_]+: Atomic' "$SRC_MACHINE/present.rs"); d=$(grep -c 'pub fn wake_from_worker' "$SRC_MACHINE/present.rs")
if [ "$n" -eq 1 ] && [ "$d" -eq 1 ]; then ok "present: one worker door"; else fail "present: $n atomic statics, $d doors (one of each)"; fi

if [ -n "$(grep_code '\bEffect::' "$SRC" "$SRC_BASE" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS")" ]; then fail "effect: \`Effect::\` is spelled (use Fx:: / AppFx::)"; else ok "effect"; fi

gate nav '(crate::ui|plx_ui)::nav::' "$SRC_SCREENS"

# cards: a screen's card shelves and grids come from `plx_ui::cards::{Shelf, Grid}` (the
# shared-card-sections plan, layer L1), which own the pop, press, stops, restore and paging a
# hand-assembled strip forgets. The L0 modules `card_row` and `poster_grid` are `pub(crate)`, so a
# screen cannot NAME them at all; what it can reach is what `plx_ui::cards` re-exports, and this
# gate is the backstop on that surface. `screens/` may not use, in non-test code: the tile
# painters `cards::{paint_visible, draw_tile, draw_focused}` (qualified through any alias, or
# imported by name from `cards::{..}`) should one be re-exported or added there, the L0 types
# `CardRow`, `GridPop` and `GridBands` (a screen can name one only through such a re-export), or the
# thread-local `press::scale()`; and `AvatarRow`, the circular-avatar row painter, is the profile
# picker's alone (`profiles.rs`) — the scoping is in the gate, not an allowlist row, because it is
# a permanent fact about one screen and not a migration. The geometry and label helpers
# `plx_ui::cards` also re-exports (`tile_rect`, `under_band`, `poster_label`, ...) are what a
# screen's own layout reads and are deliberately not gated. Whole test files are skipped; the
# allowlist (empty) is the screens that still assemble their own, one line per file, and only
# SHRINKS.
cards_pat='cards::(paint_visible|draw_tile|draw_focused)\b|use [^;]*cards::\{[^}]*\b(paint_visible|draw_tile|draw_focused)\b|\bCardRow\b|\bGridPop\b|\bGridBands\b|press::scale\(\)'
cards_bad=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  p="${line%%:*}"
  if is_wholly_test "$p"; then continue; fi
  if ! allowed cards "$p"; then echo "    $line"; cards_bad=$((cards_bad+1)); fi
done < <(grep_code "$cards_pat" "$SRC_SCREENS")
while IFS= read -r line; do
  [ -z "$line" ] && continue
  p="${line%%:*}"
  if is_wholly_test "$p" || [ "$p" = "$SRC_SCREENS/profiles.rs" ]; then continue; fi
  echo "    $line"; cards_bad=$((cards_bad+1))
done < <(grep_code '\bAvatarRow\b' "$SRC_SCREENS")
if [ "$cards_bad" -eq 0 ]; then ok "cards"; else fail "cards: $cards_bad line(s) outside ci/allow/cards.txt (or AvatarRow outside profiles.rs) — draw card sections with plx_ui::cards::Shelf / Grid"; fi

# layer: a SCREEN never names the application. §2.1's table says `screens/` may name `ui/`,
# `stores/`, `plex/` and `player/` and never `app/`, and until phase 10 that half of the rule was
# prose alone — which is exactly how `screens/registry.rs` came to record, in its own module doc,
# that it could not hold the concrete `ScreenArg` because the argument carried an `app`-private
# `Route`. The type moved and the rule is now a grep, because the criterion that depends on it (§0
# criterion 5: a new screen touches its own file, the registry, `dev/scenarios.rs` and the manifest
# and nothing else) is only worth as much as the boundary underneath it. Count is zero, with no
# allowlist: a screen that needs something of the loop's asks for it as an effect (`AppFx`,
# `LoopReq`) — that is what the bundle in `registry.rs` is.
if [ -n "$(grep_code '(crate|super)::app::' "$SRC_SCREENS")" ]; then
  grep_code '(crate|super)::app::' "$SRC_SCREENS" | sed 's/^/    /'
  fail "layer: a screen names the application (§2.1) — ask for it as an AppFx/LoopReq instead"
else ok "layer"; fi
gate sessionwrite 'session::load\(' "$SRC_SCREENS" "$SRC_UI" "$SRC_MACHINE" "$SRC_APPKIT"
# uistorage: the LIBRARY (`ui/`) never names the storage layer (§2.1: `ui/` may name only
# `crate::{gfx,text,paths,task}`). A ui-owned sweep that needs the app's removal rule takes it as an
# injected `fn` (`ui::rec::erase_owned_artifacts`). Zero, no allowlist. The wider table is not a
# grep yet: ui/ still names other application modules, mostly from tests and dev instruments.
gate uistorage '(crate|super|plx_platform)::storage::' "$SRC_UI" "$SRC_MACHINE"

# legacypage: the word itself, anywhere under src — a doc that still describes the type is as much
# a hit as a declaration, which is the point (nothing compiles the prose either).
legacy_hits=$(grep -rn --include='*.rs' 'LegacyPage' "$SRC" "$SRC_BASE" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS" 2>/dev/null || true)
if [ -z "$legacy_hits" ]; then ok "legacypage"; else
  echo "$legacy_hits" | sed 's/^/    /'
  fail "legacypage: $(echo "$legacy_hits" | wc -l | tr -d ' ') mention(s) — the type is retired (§15.2)"
fi

# sibling: one screen family per directory; `<a>` is the first path component under screens/.
#
# `screens/registry.rs` is EXEMPT, and by design rather than by allowlist: it holds the concrete
# `ScreenArg` and the one `mount` match (§2.1), so naming every screen is the whole of its job —
# that match is the single place the application says which argument mounts which screen, and a
# gate that forbade it would forbid the structure the spec asks for. It was an allowlist entry
# until phase 10 moved the mounter into it; an entry would have had to say "this file names all of
# them, permanently", which is a rule and not a migration.
sib_bad=0
while IFS= read -r f; do
  [ "$f" = "$SRC_SCREENS/registry.rs" ] && continue
  rel="${f#"$SRC_SCREENS"/}"
  own="${rel%%/*}"
  own="${own%.rs}"
  hits=$(grep_code 'crate::[a-z_]+' "$f" | grep -oE 'crate::[a-z_]+' | sort -u \
    | grep -vE "^crate::(registry|${own})$" || true)
  [ -z "$hits" ] && continue
  if ! allowed sibling-migration "$f"; then
    echo "    $f: $(echo "$hits" | tr '\n' ' ')"
    sib_bad=$((sib_bad+1))
  fi
# ...over the files that name a sibling screen at all; the rest ran four processes to find nothing.
done < <(grep -rlE --include='*.rs' 'crate::[a-z_]+' "$SRC_SCREENS" 2>/dev/null | sort)
if [ "$sib_bad" -eq 0 ]; then ok "sibling"
else fail "sibling: $sib_bad file(s) name a sibling screen (use crate::registry)"; fi

# ============================================================================================
# Phase 12 rules (spec §15.2, D4): the gates the earlier phases' own comments above never wrote,
# added here because their preconditions were supposed to have landed by the time this package
# ran. Three of them (route, ladder, hittest — plus the `#[cfg(test)] mod`/`run`-length checks
# below) did NOT find that precondition true when they were WRITTEN, and were landed red on
# purpose: a red result was the accurate signal that D1 (Route/Nav/Trail retirement) was not done,
# not a bug in the gate. **All of them are green now** — PX-OVERLAYS closed `ladder`/`hittest` and
# PX-D1 deleted the `Route` enum, `app/nav.rs` and `ui/trail.rs`, which is what `route`, `fnlen` and
# `testmod` were each waiting on. They stay BLOCKING, which is the whole point: the retirement is
# only finished for as long as nothing re-introduces the shape.
# ============================================================================================

# textmeasure (phase 12, D4 — ZERO now, was an allowlist): crate::text::(text_width|elide|cap_h) (`plx_gfx::text::` since the gfx split)
# outside the three files that own the raw primitives (`gfx/src/text.rs`, `ui/text_view.rs`,
# `ui/text_buffer.rs` — the TextView component built directly on them) and the BODY of an
# `impl … Measure for …` block. That second exemption is structural, not a path: `Measure`
# (`machine/src/machine.rs`) is the one seam (`TtfMeasure` on device/sim, `TableMeasure` under replay,
# `FixtureMeasure` in host tests), and every real call site now threads it down from its caller —
# but two leaves genuinely could not (each struct's own doc says which and why): `widgets.rs`'s
# `LegacyMeasure` (generic `View::draw` leaves with no capability parameter, and the legacy
# tab-row cache reached only through `app::bridge`, off-limits to this lane) and `login.rs`'s
# `RawTextMeasure` (a host-test-only stand-in for `TtfMeasure`, whose `width` carries a boot-order
# `debug_assert!` that a test with no `init_text` would trip). Both wrap the identical free
# functions `TtfMeasure` does, minus the assert, so detecting the block structurally — rather than
# allowlisting the two files outright — is what keeps this a real zero-tolerance gate: a THIRD
# `impl Measure for` added later still only exempts ITS OWN body, and any other raw call anywhere
# in the tree fails.
tm_seams="$SRC_UI/text_view.rs $SRC_UI/text_buffer.rs $SRC_GFX/text.rs"
tm_bad=0
while IFS= read -r f; do
  is_seam=0
  for s in $tm_seams; do [ "$f" = "$s" ] && is_seam=1; done
  [ "$is_seam" -eq 1 ] && continue
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    echo "    $f:$line"
    tm_bad=$((tm_bad+1))
  done < <(awk '
    skip>0 { n=gsub(/\{/,"{"); m=gsub(/\}/,"}"); depth+=n-m; if (depth<=0) skip=0; next }
    /impl[ \t].*Measure.*[ \t]for[ \t]/ { skip=1; depth=gsub(/\{/,"{")-gsub(/\}/,"}"); if (depth<=0) skip=0; next }
    { print NR":"$0 }' "$f" | grep -E '(crate|plx_gfx)::text::(text_width|elide|cap_h)\(' | grep -vE '^[0-9]+:[[:space:]]*//' || true)
# ...over the files that spell a raw measurement call at all: the `awk` below only DROPS the body of
# an `impl … Measure for …` block, so a file with no raw call has nothing for it to find.
done < <(grep -rlE --include='*.rs' '(crate|plx_gfx)::text::(text_width|elide|cap_h)\(' "$SRC" "$SRC_GFX" "$SRC_UI" "$SRC_APPKIT" "$SRC_SCREENS" 2>/dev/null | sort)
if [ "$tm_bad" -eq 0 ]; then ok "textmeasure"; else fail "textmeasure: $tm_bad line(s) outside the Measure seam"; fi

# dt (phase 12, D4 — ZERO now, was an allowlist): idle::dt() (deleted from machine/src/idle.rs entirely —
# card_row.rs's focused-title marquee, its last caller, now reads the absolute idle::now_ms()
# instead and wrapping_sub's two readings, motion::Phase's own drift-free idiom) and (+=|-=) *dt
# outside machine/src/motion.rs, which owns the integrators spec §4.2 requires for hashed logical state.
# Every clock-driven animator (spinners, ramps, hero auto-advance, the modal dip, the route
# cross-fade, the poster-preview settle) now advances through motion::Ramp/motion::Phase, which
# read Tick.ms directly and report Motion from inside advance() — the frozen-animator regression
# class this whole gate exists to catch. motion.rs currently has no hit either; the exclusion is
# the documented intent (spec §4.2: the ONLY file licensed to touch a raw per-frame delta), not a
# live carve-out.
dt_hits=$(grep_code 'idle::dt\(\)|(\+=|-=)\s*dt\b' "$SRC" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS" | grep -v "^$SRC_MACHINE/motion.rs:")
if [ -z "$dt_hits" ]; then ok "dt"; else
  echo "$dt_hits" | sed 's/^/    /'
  fail "dt: $(echo "$dt_hits" | wc -l | tr -d ' ') line(s) — see rule comment above"
fi

# ladder: the old per-screen focus/hit ladder shape, zero across ui/ + screens/ once every Screen
# answers FocusSource::Engine/HitSource::Engine (D2). No allowlist: every remaining hit is a
# screen or component this phase was supposed to finish converting.
gate_zero() {
  local rule="$1" pat="$2"; shift 2
  local hits; hits=$(grep_code "$pat" "$@")
  if [ -z "$hits" ]; then ok "$rule"; else
    echo "$hits" | sed 's/^/    /'
    fail "$rule: $(echo "$hits" | wc -l | tr -d ' ') line(s) — see rule comment above"
  fi
}
gate_zero ladder 'fn move_focus|fn pointer_focus|fn top_focus|fn zones\b|fn key\(sym|fn focus_is_card|fn focus_is_ctl' "$SRC_UI" "$SRC_MACHINE" "$SRC_APPKIT" "$SRC_SCREENS"

# hittest: the narrowed raw hit-tester call shape, zero in app/ once the player's HUD registers
# its stops through DrawFrame::stop (D2) instead of app/run.rs testing raw coordinates against
# player_hud's geometry by hand. Deliberately NOT a blanket `_at(` — that false-positives on
# resume_at/memory_at/open_settings_at/write_at/profile_chip_at, which are unrelated lookups.
gate_zero hittest 'pointer_focus\(|\b(failure_quality|icon|scrub)_hit\(' "$SRC/app"

# frame: the three privileged OS-primitive calls, ZERO-TOLERANCE outside `app/run.rs` (D4) — no
# allowlist file, because there is exactly one legitimate home once D1 lands: `app/run.rs` is the
# frame loop, and `app/bridge.rs`'s `Rig` impl delegates to `run::rig_opaque_route`/
# `rig_clear_opaque_region` (a one-line pass-through) rather than naming `plx_platform::tv::window::` itself,
# which is what keeps this gate's text out of bridge.rs without splitting the `impl Rig<AppHost>
# for Bridge` block (a trait's impl for a type is one syntactic unit; it carries two dozen other
# methods beside these three). A self-test that spells two of the three call shapes as STRING
# LITERALS — it reads app/run.rs's own source text at runtime and compares against a copy of the
# exact line it expects, which is data, not a call (`app/run.rs`'s own `video_plane_gate_tests`
# today, `machine/src/idle.rs`'s before the machine layer left `ui/`) — must not trip this gate in any file
# that is not exempt, so a hit inside a `"…"` literal is stripped before matching (the same
# double-quote-depth tracking `tmppath` below uses), rather than exempting files by name: an
# actual call typed outside a string still fails this gate. `// `-prefixed comment lines (`app/run.rs` keeps one, describing where a call
# used to live) are stripped the same way `grep_code` above does for every other rule.
frame_pat='(crate|plx_platform)::tv::window::(pump_bus|opaque_route|clear_opaque_region)\('
frame_bad=0
while IFS= read -r f; do
  [ "$f" = "$SRC/app/run.rs" ] && continue
  hits=$(strip_strings_and_comments "$f" | grep -nE "$frame_pat" || true)
  [ -z "$hits" ] && continue
  while IFS= read -r h; do
    ln="${h%%:*}"
    orig=$(sed -n "${ln}p" "$f")
    echo "    $f:$ln:$orig"
    frame_bad=$((frame_bad+1))
  done <<< "$hits"
# ...over the files that name one of the three calls at all. `strip_strings_and_comments` only ever
# REMOVES matches, so a file with no raw hit cannot fail this gate — and running its `awk` plus a
# `grep` over all 420 files, to reach the two that mention the shape, was the single most expensive
# rule in this script (4.8 s of its 17 s).
done < <(grep -rlE --include='*.rs' "$frame_pat" "$SRC" "$SRC_BASE" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS" 2>/dev/null | sort)
if [ "$frame_bad" -eq 0 ]; then ok "frame"
else fail "frame: $frame_bad line(s) of a privileged OS-primitive call outside app/run.rs"; fi

# sink: the Starfish/ACB verbs (`tv::sink::VideoSink`) are the player's alone (step L15). Only
# `player/` (including `player/ffi*.rs`, which implement the trait), `port.rs` (which installs one)
# and `tv.rs` with `tv/` (which hold it) name `tv::sink::installed` or `VideoSink`; every other module reaches
# the television through the narrower `tv` interfaces. Wholly-test files are skipped like inline
# `#[cfg(test)]` blocks. Zero, no allowlist.
sink_bad=0
while IFS= read -r line; do
  [ -z "$line" ] && continue
  p="${line%%:*}"
  case "$p" in "$SRC_MEDIA"/player/*|"$SRC_PLATFORM"/tv/*|"$SRC_PLATFORM"/tv.rs|"$SRC"/port.rs) continue ;; esac
  if is_wholly_test "$p"; then continue; fi
  echo "    $line"; sink_bad=$((sink_bad+1))
done < <(grep_code 'tv::sink::installed|\bVideoSink\b' "$SRC" "$SRC_PLATFORM" "$SRC_UI" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS")
if [ "$sink_bad" -eq 0 ]; then ok "sink"
else fail "sink: $sink_bad line(s) naming the video sink outside player/, port.rs, tv.rs and tv/"; fi

# route: `Route::` in app/ = 0, and `enum Route` gone from the whole tree (D1/D4). No allowlist:
# the type is meant to be retired, not narrowed.
route_app=$(grep_code 'Route::' "$SRC/app")
if [ -z "$route_app" ]; then ok "route: Route:: in app/"
else fail "route: $(echo "$route_app" | wc -l | tr -d ' ') \`Route::\` use(s) in app/ — the page alphabet is \`AppArg\` (D1)"; fi
route_enum=$(grep -rn --include='*.rs' 'enum Route\b' "$SRC" "$SRC_UI" 2>/dev/null || true)
if [ -z "$route_enum" ]; then ok "route: enum Route absent"
else
  echo "$route_enum" | sed 's/^/    /'
  fail "route: enum Route re-declared — the page alphabet is \`AppArg\` (D1)"
fi

# fnlen: app/run.rs::run <= 200 lines, run_application (app/mod.rs, the D4 skeleton) <= 10 and
# plx_run (port.rs: install, then hand over) <= 10 — counted by brace
# depth from the `fn` line to its matching close, not by grep pattern.
fn_body_lines() {
  # fn_body_lines <file> <fn-name-pattern> — prints the line count of the first matching fn's body
  local file="$1" namepat="$2"
  awk -v pat="$namepat" '
    started==0 && $0 ~ ("fn " pat "\\(") { started=1; start=NR }
    started==1 {
      n=gsub(/\{/,"{"); m=gsub(/\}/,"}"); depth+=n-m
      if (depth<=0 && NR>start) { print NR-start+1; exit }
    }
  ' "$file"
}
run_len=$(fn_body_lines "$SRC/app/run.rs" 'run')
if [ -n "$run_len" ] && [ "$run_len" -le 200 ]; then ok "fnlen: app/run.rs::run ($run_len lines)"
else fail "fnlen: app/run.rs::run is ${run_len:-unknown} lines, budget 200 — a phase of the frame belongs in its own function (see run.rs's own doc)"; fi
runapp_len=$(fn_body_lines "$SRC/app/mod.rs" 'run_application')
if [ -n "$runapp_len" ] && [ "$runapp_len" -le 10 ]; then ok "fnlen: run_application (app/mod.rs) ($runapp_len lines)"
else fail "fnlen: run_application (app/mod.rs) is ${runapp_len:-unknown} lines, budget 10"; fi
plxrun_len=$(fn_body_lines "$SRC/port.rs" 'plx_run')
if [ -n "$plxrun_len" ] && [ "$plxrun_len" -le 10 ]; then ok "fnlen: plx_run (port.rs) ($plxrun_len lines)"
else fail "fnlen: plx_run (port.rs) is ${plxrun_len:-unknown} lines, budget 10"; fi

# testmod: `#[cfg(test)] mod` count in app/mod.rs = 0 (D4/D8) — every test module named in D8's
# table is meant to have moved to its subject's own file by the time this gate is added.
testmod_n=$(grep -c '^#\[cfg(test)\]$' "$SRC/app/mod.rs" 2>/dev/null || echo 0)
# only count ones immediately followed by `mod `, matching the mutators gate's own convention
testmod_n=$(awk '/^#\[cfg\(test\)\]$/{p=1;next} p && /^mod /{c++} {p=0} END{print c+0}' "$SRC/app/mod.rs")
if [ "$testmod_n" -eq 0 ]; then ok "testmod: app/mod.rs"
else fail "testmod: $testmod_n \`#[cfg(test)] mod\` block(s) in app/mod.rs — a test lives beside its subject (D8)"; fi

# threads: `thread::spawn(` outside task.rs, PRODUCTION lines only, EVERY spelling — a
# `#[cfg(test)] mod` block is skipped by brace depth, the same convention the `mutators` gate
# above uses, because every remaining call site in the tree (43, re-verified 2026-09-11 after
# widening the match below) is a test's own mock TCP/HTTP server standing in for a peer, never a
# real worker (see ci/allow/threads.txt's own header). Three spellings, not one:
#   - `std::thread::spawn(`, the fully-qualified form the old grep matched;
#   - a bare `thread::spawn(` after `use std::thread;` (or `use std::thread as thread;`) — the
#     shorter spelling is a SUBSTRING of the longer one, so one `\bthread::spawn\(` pattern
#     catches both without a second pass;
#   - a bare `spawn(` after a `use std::thread::spawn;` (or a `use std::thread::{.., spawn, ..};`)
#     import, which is a different call SHAPE (`spawn(` alone) and can only be told apart from
#     every other `spawn(` in the tree — `task::spawn(`, `Builder::spawn(`, a test helper's own
#     `fn spawn(..)` — by first checking whether the file imported `spawn` from `std::thread` that
#     way. No file does today (verified 2026-09-11: zero `use std::thread::spawn` /
#     `use std::thread::{..spawn..}` lines outside task.rs itself), so this spelling adds nothing
#     to the count yet — it exists so a FUTURE import cannot go unmatched the way the bare
#     `thread::spawn(` spelling used to.
threads_bad=0
while IFS= read -r f; do
  # a wholly-test file is test code exactly like an inline `#[cfg(test)] mod` block, which the
  # awk below skips — without this, splitting a test module into its own file fails the gate
  if is_wholly_test "$f"; then continue; fi
  pat='\bthread::spawn\('
  if grep -qE '^\s*use\s+std::thread::(spawn\s*;|\{[^}]*\bspawn\b[^}]*\}\s*;)' "$f"; then
    pat='\bthread::spawn\(|\bspawn\('
  fi
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    if ! allowed threads "$f"; then echo "    $f:$line"; threads_bad=$((threads_bad+1)); fi
  done < <(awk '
    skip>0 { n=gsub(/\{/,"{"); m=gsub(/\}/,"}"); depth+=n-m; if (depth<=0) skip=0; prev=$0; next }
    prev ~ /^[[:space:]]*#\[cfg\((test|any\(test, feature = "test-support"\))\)\][[:space:]]*$/ && /^[[:space:]]*mod / { skip=1; depth=gsub(/\{/,"{")-gsub(/\}/,"}"); if (depth<=0) skip=0; prev=$0; next }
    { print NR":"$0; prev=$0 }' "$f" | grep -E "$pat" | grep -vE '^[0-9]+:\s*//' || true)
# ...over the files that spell `spawn(` at all — a superset of both matched spellings, and the `awk`
# below only drops `#[cfg(test)] mod` blocks, so the count is unchanged.
done < <(grep -rlE --include='*.rs' '\bspawn\(' "$SRC" "$SRC_BASE" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS" 2>/dev/null | grep -v "^$SRC_BASE/task.rs\$" | sort)
threads_declared=$(sed -n 's/^# count: *//p' ci/allow/threads.txt | head -1)
if [ "$threads_bad" -eq "${threads_declared:-0}" ]; then ok "threads"
else fail "threads: $threads_bad line(s) outside ci/allow/threads.txt (declared count is exactly ${threads_declared:-0}, not a ceiling)"; fi

# blocking: the frame thread's blocking exceptions, as a ledger that only shrinks. Every
# production `allow_blocking(` CALL SITE is listed in
# ci/allow/blocking.txt as `<path>::<enclosing fn>` (one line per site, so a second site inside an
# already-listed fn is a new line, not free), and the gate compares the two as multisets:
#   - a site NOT in the list fails — a new frame-thread exception must move its work onto a worker
#     (`task::spawn_off_frame`, whose token a PMS half takes) instead of being excused;
#   - an entry whose site no longer exists fails — so the list can only shrink, and a migration
#     lands by deleting its line. The declared `# count:` must also equal the entry count.
# Test code is skipped exactly as in `threads` (a wholly-test file, a `#[cfg(test)] mod` block).
# The definition itself (`task/blocking.rs`), `fn` declarations and comment-only lines are not
# call sites. Any OTHER non-test mention of the bare identifier — `use ...::allow_blocking as x`,
# `let f = allow_blocking;` — is an alias the call-site grep cannot see, so it fails outright
# (`blocking_refs`); the one tolerated mention is the exact re-export line in `base/src/task.rs`.
# The enclosing fn is the innermost `fn <name>` whose body holds the call, tracked by
# brace depth; a call in a closure belongs to the fn around the closure.
blocking_sites=$(
  while IFS= read -r f; do
    if is_wholly_test "$f"; then continue; fi
    awk -v file="$f" -v reexport_file="$SRC_BASE/task.rs" '
      function pop_to(d) { while (top > 0 && sdepth[top] > d) top-- }
      skip>0 { n=gsub(/\{/,"{"); m=gsub(/\}/,"}"); depth+=n-m; if (depth<=0) skip=0; prev=$0; next }
      prev ~ /^[[:space:]]*#\[cfg\((test|any\(test, feature = "test-support"\))\)\][[:space:]]*$/ && /^[[:space:]]*mod / { skip=1; depth=gsub(/\{/,"{")-gsub(/\}/,"}"); if (depth<=0) skip=0; prev=$0; next }
      {
        line=$0; prev=$0
        code=line; sub(/\/\/.*/, "", code)
        # a mention that is neither a call, the `fn` definition nor the listed re-export: an alias
        if (code ~ /(^|[^A-Za-z0-9_])allow_blocking([^A-Za-z0-9_(]|$)/ && code !~ /fn[[:space:]]+allow_blocking\(/ \
            && !(file == reexport_file && code ~ /^[[:space:]]*pub use blocking::\{allow_blocking, AllowBlocking\};[[:space:]]*$/))
          print "REF\t" file ":" NR
        if (match(code, /(^|[^A-Za-z0-9_])fn[[:space:]]+[A-Za-z0-9_]+/)) {
          name=substr(code, RSTART, RLENGTH); sub(/.*fn[[:space:]]+/, "", name); pending=name
        }
        if (code ~ /(^|[^A-Za-z0-9_])allow_blocking\(/ && code !~ /fn[[:space:]]+allow_blocking\(/) {
          hit=(top>0 ? stack[top] : (pending != "" ? pending : "?"))
          # a call on the same line as the `fn` it sits in, before that fn pushes its body
          if (pending != "" && code ~ /fn[[:space:]]/ && code ~ /\{/) hit=pending
          print file "::" hit
        }
        n=gsub(/\{/,"{",code); m=gsub(/\}/,"}",code)
        for (i=0; i<n; i++) { depth++; if (pending != "") { top++; stack[top]=pending; sdepth[top]=depth; pending="" } }
        for (i=0; i<m; i++) { depth--; pop_to(depth) }
        if (pending != "" && code ~ /;[[:space:]]*$/) pending=""
      }' "$f"
  done < <(grep -rlE --include='*.rs' 'allow_blocking' "$SRC" "$SRC_BASE" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS" 2>/dev/null | grep -v "^$SRC_BASE/task/blocking.rs\$" | sort) | sort
)
blocking_refs=$(printf '%s\n' "$blocking_sites" | awk -F'\t' '$1 == "REF" {print $2}')
blocking_sites=$(printf '%s\n' "$blocking_sites" | awk -F'\t' '$1 != "REF"')
blocking_declared=$(awk -F'\t' '/^[[:space:]]*(#|$)/ {next} {print $1}' ci/allow/blocking.txt | sort)
blocking_new=$(comm -23 <(printf '%s\n' "$blocking_sites" | sed '/^$/d') <(printf '%s\n' "$blocking_declared" | sed '/^$/d'))
blocking_stale=$(comm -13 <(printf '%s\n' "$blocking_sites" | sed '/^$/d') <(printf '%s\n' "$blocking_declared" | sed '/^$/d'))
blocking_count=$(printf '%s\n' "$blocking_declared" | sed '/^$/d' | wc -l | tr -d ' ')
blocking_count_declared=$(sed -n 's/^# count: *//p' ci/allow/blocking.txt | head -1)
if [ -z "$blocking_new" ] && [ -z "$blocking_stale" ] && [ -z "$blocking_refs" ] && [ "$blocking_count" -eq "${blocking_count_declared:-0}" ]; then ok "blocking ($blocking_count sites)"
else
  [ -n "$blocking_new" ] && printf '%s\n' "$blocking_new" | sed 's/^/    new frame-thread blocking exception: /'
  [ -n "$blocking_refs" ] && printf '%s\n' "$blocking_refs" | sed 's/^/    allow_blocking named without a call (an alias the ledger cannot count): /'
  [ -n "$blocking_stale" ] && printf '%s\n' "$blocking_stale" | sed 's/^/    stale ci\/allow\/blocking.txt entry (site is gone — delete the line): /'
  fail "blocking: the frame-thread exception ledger is not exact (new: $(printf '%s\n' "$blocking_new" | sed '/^$/d' | wc -l | tr -d ' '), stale: $(printf '%s\n' "$blocking_stale" | sed '/^$/d' | wc -l | tr -d ' '), aliased: $(printf '%s\n' "$blocking_refs" | sed '/^$/d' | wc -l | tr -d ' '), declared count ${blocking_count_declared:-0} vs $blocking_count entries) — a new allow_blocking site moves its work to a worker instead; the list only shrinks"
fi

# tmppath: a literal `/tmp/dispatcharrtv-` string outside dev.rs and the log sinks, = 0 (D4). The
# earlier version matched only a literal and a filesystem-open VERB co-occurring on the SAME
# LINE, which passed a two-line split (`let p = format!("/tmp/dispatcharrtv-x"); File::open(p)`)
# straight through. Rewritten as the D4 wording actually reads: strip comments, then find the
# literal, then walk the CALL that encloses it — the same paren-depth-outside-literals shape
# `tmppath`'s own header used to point at `ci/check-scrub` for; that script does not exist
# anywhere in this tree (checked 2026-09-11 — the closest real analogue is `diag/scrub.rs`'s
# redaction pass, which is Rust, not a shell gate), so the walk below is a from-scratch small
# tokenizer rather than a shared helper. It: (1) strips `//`/`///` comments to end of line, never
# treating a `//` inside a string as one; (2) tracks whether each character is inside a `"…"`
# string literal, honouring `\"` so an escaped quote does not end it early; (3) as it goes,
# maintains a stack of the CALL NAME behind every currently-open, not-yet-closed `(` (the token
# immediately before it) — which is what lets a match inside `crate::eventlog::log(&format!("…"))`
# see BOTH enclosing calls, `format!` innermost and the log call beneath it, across as many lines as the
# call spans. A hit is a `/tmp/dispatcharrtv-` match that is NOT inside a string, or is inside one but
# no enclosing call on that stack is `log`/`crate::eventlog::log`/`log!` — i.e. exactly the two exemptions
# D4 names, comment and log-message text, and nothing else (a bare `let s = "/tmp/dispatcharrtv-x";`
# with no log() around it is a hit, deliberately, even though it opens nothing — the spec's own
# wording is "any literal…unless", not "any literal that is also an open"). `dev.rs` is the one
# structural exemption; a second category ("the log sinks") is named in the spec but resolves to
# NOTHING in this tree today — `eventlog::events_log`/`app/boot.rs`'s crash-log open both build the
# path through `paths::in_runtime_dir("dispatcharrtv-…")`, a bare filename with no `/tmp/` prefix, so
# neither one is a `/tmp/dispatcharrtv-` literal in the first place and there is no second file to
# name here (re-verify this if a log sink is ever given a hardcoded `/tmp/` path).
tmp_hits=$(python3 - "$SRC" "$SRC_BASE" "$SRC_MACHINE" "$SRC_PLATFORM" "$SRC_GFX" "$SRC_NET" "$SRC_UI" "$SRC_DATA" "$SRC_MEDIA" "$SRC_APPKIT" "$SRC_SCREENS" <<'PY'
import os, sys

src = sys.argv[1]
roots = sys.argv[1:]
exempt_files = {os.path.join(src, "dev.rs")}
needle = "/tmp/dispatcharrtv-"
log_names = {"log", "crate::eventlog::log", "plx_base::eventlog::log", "log!"}

def scan(path, text):
    hits = []
    stack = []          # call name behind each currently-open '('
    in_str = False
    i = 0
    n = len(text)
    line = 1
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
            continue
        if in_str:
            if text.startswith(needle, i):
                exempt = any(name in log_names for name in stack)
                if not exempt:
                    hits.append(line)
            if c == "\\":
                i += 2
                continue
            if c == '"':
                in_str = False
            i += 1
            continue
        # not in a string
        if c == '"':
            in_str = True
            i += 1
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            nl = text.find("\n", i)
            i = n if nl == -1 else nl
            continue
        if c == "(":
            j = i - 1
            while j >= 0 and text[j] in " \t\n":
                j -= 1
            k = j
            while k >= 0 and (text[k].isalnum() or text[k] in "_:!"):
                k -= 1
            stack.append(text[k + 1:j + 1])
            i += 1
            continue
        if c == ")":
            if stack:
                stack.pop()
            i += 1
            continue
        if text.startswith(needle, i):
            # a literal outside any string at all — not valid Rust for a real path, but still
            # not comment or log-message text, so it counts.
            hits.append(line)
        i += 1
    return hits

bad = 0
for tree in roots:
  for root, _dirs, files in os.walk(tree):
    for fn in sorted(files):
        if not fn.endswith(".rs"):
            continue
        path = os.path.join(root, fn)
        if path in exempt_files:
            continue
        with open(path, encoding="utf-8") as f:
            text = f.read()
        if needle not in text:
            continue
        for ln in scan(path, text):
            src_line = text.splitlines()[ln - 1]
            print(f"{path}:{ln}:{src_line}")
            bad += 1
sys.exit(1 if bad else 0)
PY
)
tmp_status=$?
if [ "$tmp_status" -eq 0 ] && [ -z "$tmp_hits" ]; then ok "tmppath"
else
  echo "$tmp_hits" | sed 's/^/    /'
  fail "tmppath: $(echo "$tmp_hits" | grep -c . ) line(s) of a literal /tmp/dispatcharrtv- path outside dev.rs, not comment or log-message text"
fi


# every allowlist's declared count equals its entries
for f in ci/allow/*.txt; do
  declared=$(sed -n 's/^# count: *//p' "$f" | head -1)
  entries=$(grep -cvE '^[[:space:]]*(#|$)' "$f")
  [ "$declared" = "$entries" ] || fail "$f declares count $declared but has $entries entries"
done

[ "$fails" -eq 0 ] && { echo "check-deps: all gates green"; exit 0; }
echo "check-deps: $fails gate(s) failed"; exit 1
