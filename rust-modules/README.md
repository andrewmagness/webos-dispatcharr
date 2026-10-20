# rust-modules — the Rust app core

This workspace's ARM build is a `staticlib` linked into the C binary (`pkg/dispatcharrtv`). It IS
the app — UI, event loop, player engine, demux pipeline and the Dispatcharr data layer all live
here (`docs/agent-reference.md` has the architecture). The C side boots the process, records an
async-signal-safe fallback crash line and image marker, and wraps the Starfish C++ seam. Its Rust
entry points are `plx_run`, `plx_crash_write_image_marker` and the two Starfish callbacks.

## The crates

One crate per layer, each allowed to name only the layers below it (`ci/module-layers.ini` is the
authority, and `docs/module-layers.md` opens with the table of what each holds):

`plx_base` → `plx_machine` → `plx_platform` → `plx_gfx` / `plx_net` → `plx_dispatcharr` (the server
API, account and guide) / `plx_ui` (the UI framework) / `plx_data` → `plx_media` (demux, AU queues,
the player) → `plx_appkit` → `plx_screens` (Guide, Setup, the player screen) → `dispatcharrtv`
(`src/`: the loop, adapters, dev surfaces and the webOS port). `storage/` is the separate
`dispatcharrtv-storage` helper binary.

## Left as C by design

- **`src/starfish.c`** — LG's `StarfishMediaAPIs` (the mangled C++ methods called
  via `__asm__`, an `sret` `std::string` return, a 64 KB in-place object buffer)
  plus the ACB video-plane bind. The engine that *drives* it is Rust (`media/src/player/`).
- **`src/main.c`** — the boot shim: event-log/stderr setup, process bring-up and the fallback
  image marker; it calls the Rust entry points named above and owns no application state.
- **`src/crashtrace.c`** — the async-signal-safe fallback fault recorder installed by the shim.
- **`src/svg.c`** — the vendored nanosvg rasterizer (header-only C).
- **`src/compat/getauxval.c`** — the bounded `getauxval` fallback that replaced the NDK's old
  polyfill library.
- **`src/ass.c`** — compiled into the bundled `libass-plx`, never into the app: it reads libass's
  structures through the exact headers that library was built with.
- **`src/gpdebug.c`** — a guard-page allocator for debug builds only, not part of the normal build.

Rust *can* do the Starfish FFI (`#[link_name = "<mangled>"]` mirrors the C
`__asm__`), but it would be **all `unsafe` with zero safety gain**, high effort,
and high risk to the hardest-won feature (video on the panel). C stays the right
tool for the C++ interop.

## Build (see the Makefile for the full toolchain)

Cross-compiled (the `.a` via `cargo +nightly rustc --lib --crate-type staticlib`, with
`-Z build-std=std,panic_unwind`) for `arm-unknown-linux-gnueabi`; the C side and the final link use the webOS NDK
(`make setup-env`). Critical TV flags: `-C target-cpu=cortex-a9` (the default
ARMv6 codegen emits a CP15 memory barrier that SIGILLs on this SoC; cortex-a9
emits the dedicated `dmb`) and `-C target-feature=-neon`, with `-Z build-std`
rebuilding std under those flags. The Makefile owns this invocation — nothing
else should duplicate it.
