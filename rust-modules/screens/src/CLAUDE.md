# screens/ — the application's owned screens (read before adding or changing one)

Every `Screen` impl the dispatcher mounts, steps, focuses and draws lives here: the Dispatcharr
Setup page (with its Server page), the Guide, and the `player/` family. This is the APPLICATION
half of the restructure; the library half is `../ui/`, whose [`CLAUDE.md`](../../ui/src/CLAUDE.md)
carries the token rules, the architecture and the rendering policy. Read that one first — its four
rules bind here too.

**`lib.rs` and `registry.rs` document themselves.** Both carry substantial `//!` docs and they are
the authority on the module list, the page alphabet and the application bundle. This guide holds
only what they do not: the rules that span screens.

## The layer rule is the thing that will bite you

A screen may name `ui/`, `appkit/` (the player's widgets — `player_hud`'s read-out and subtitles,
`live_banner`), the data crates and this
directory's `registry` — **never `app/`, and never a sibling screen module**. `ci/check-deps.sh`'s
`layer` and `sibling` gates hold this, so a violation fails CI rather than review. This directory is
the crate `plx_screens`, which cannot name the application at all (cargo has no edge from it to the
application crate, and the application depends on it); the `layer` gate still greps for the
spelling `crate::app::`, and the `sibling` gate reads a sibling as `crate::<screen>` because the
crate root is the module `screens`.

Two consequences worth stating, because both look like the obvious thing to do:

- **Reaching another screen is a message, not a call.** A screen reports where it wants to go (a
  `LoopReq` such as `DispatcharrGuide`, or a `ContentReq::Play` for a channel) and the application
  loop performs the navigation.
- **A screen keeps no focus of its own.** The `FocusEngine` in `Input` holds THE current focus and
  every group's remembered cursor (spec §7.3). There is no per-screen `move_focus`, `top_focus` or
  `key(sym)` ladder, and adding one back is how the old twelve-ladder tangle started.

No `static mut` here either: a screen's state is a field of the instance the container owns (§6.1).

## Adding a screen

A new screen touches its own `screens/<name>.rs`, `screens/lib.rs` and `screens/registry.rs`: the
`AppArg` variant, the screen id, the `mount` arm and the recorded shape (`SCREEN_SHAPES`, whose
pin `the_screen_shape_inventory_is_pinned` recomputes) all live in `registry.rs` precisely so they
do not spread. If a change reaches much further than that, stop and ask why.

A screen mounted as a modal surface is the same trait with a different `Style`.

## A landing must not move ground somebody is looking at

The Guide mixes async content (the lineup, the programme grid, logos) with a live cursor, and so
will any page like it:

- **Stage, never self-commit.** A landing that changes what is under the cursor is staged and
  committed at a point the reader cannot be mid-press; the first paint is not exempt, because two
  presses reach a page inside the seconds a first fetch takes.
- **A reloading control is not a missing control.** While the content a control acts on is
  reloading, the control is still there; an async focus clamp that cannot tell the two apart
  strands focus.
- **A seat nobody chose follows the head.** A page that seats its own focus on its first block must
  re-seat when a landing changes what that block is, until the user moves. A restore is only what
  the reader chose.
- **Derive re-entry position, don't store it.** A stored row index can name a different row once the
  server's answer changes; derive the seat from what is on screen.

A press cancels on hover in the engine, not in a screen: `Input` cancels an armed pointer press
whose `hit` leaves its arm (`input_tests.rs`, `a_pointer_press_is_cancelled_when_the_hit_leaves_its_arm`),
so a screen reports nothing about its focus stop moving.

A screen that builds a `TableView` ships a `fit_report` test over its REAL builder (extract a pure
builder that takes its inputs as arguments) across `i18n::SHIPPED`, asserting
`TableView::app_fit_failures` is empty. Text that comes from a server or a user (a channel or
programme name) is marked with the `server_*` builders so it is exempt; never mark the app's own
fallback strings. See `rust-modules/ui/src/CLAUDE.md`, "Localization and shared reading layout",
for what to do when a string does not fit.

A page on a `ui::form::FormTable` focuses by IDENTITY: the element the engine holds, the
`Fx::Remember` seat and the page's canon are the row's `RowKey`, never its table index, so
reordering the form moves no focus key. A menu activates with `FormTable::activate` and rebuilds
with `set_or_open`, so a vanished focused row reopens on the safe opening row instead of sliding
onto a destructive neighbour.

## Verifying a screen change

Captures are the check — see the `ui-sim` and `which-tier` skills, and `../../ui/src/CLAUDE.md`'s
"When you're done". A screen's `name()` is the heartbeat word (`route=setup|guide|player`) that the
tools and logs select on; renaming it silently changes what they find.
