# The television's on-screen keyboard

How the app raises the TV's own keyboard and reads what it types — the Setup screen's server address,
username and password fields use it. This is the record that exists so nobody re-derives it; it was
first written for the (since removed) Plex Search screen, and everything below is about the platform,
not that screen.

Text editing itself is `ui/text_buffer.rs`: an owned UTF-8 buffer and caret, with literal insertion,
character-wise movement and deletion, and the TV's **whole-commit prediction rule** — LG's keyboard
sends a whole predicted word without a matching deletion event, so a prediction replaces the word it
completes in one commit, and no intermediate half-deleted word is ever published.

## It is plain SDL, not a webOS call

Stock `SDL_StartTextInput()` / `SDL_StopTextInput()` raise and dismiss the TV's system keyboard.

`SDL_webOS.h` **misleads by omission**: it declares eight entry points (cursor visibility, panel
resolution, refresh rate, and the five exported-window calls) and not one of them is a keyboard. The
backend is not in the webOS extension API at all — it is inside **LG's Wayland video driver**:
`Wayland_CreateDevice` writes four real hooks into `SDL_VideoDevice`
(`WebOSHasScreenKeyboardSupport`, `Show`, `Hide`, `IsShown`), `SDL_StartTextInput` dispatches to the
second, and `WebOSShowScreenKeyboard` is a complete `text_model` IME client. Typed text comes back
as an ordinary **`SDL_TEXTINPUT`** event, so `app.rs`'s existing `SDL_PollEvent` loop already sees
it. moonlight-tv 1.5.8 shipped exactly this against the TV's own SDL.

**Linking is the plain `extern "C"` case, not `dynlib!`.** Verified with
`tools/fwcompat.py --lib libSDL2-2.0.so.0 --grep TextInput`: all **14** firmware images in the
inventories — 1.2.0, 1.4.0, 2.2.3, 3.4.0, 3.9.2, 4.4.2, 4.10.0, 5.3.1, 6.4.0, 7.4.0, 8.3.0, 9.2.0,
10.2.0, 11.2.0 — export `SDL_StartTextInput`, `SDL_StopTextInput`, `SDL_SetTextInputRect` and
`SDL_IsTextInputActive`, and `--grep ScreenKeyboard` finds `SDL_HasScreenKeyboardSupport` /
`SDL_IsScreenKeyboardShown` on the same 14. The SONAME does not move either, so this is not a
`dynlib!` candidate — that module is for libraries whose *version* varies, and moving one there
trades link-time symbol checking for tolerance nothing here needs.

That symbol table says nothing about whether the **panel actually rises**, though: those are stock
public API in every SDL2 build ever made. `textinput::available()` therefore probes at runtime rather
than assuming.

## Three traps

**1. The event is SHIFTED, and the vendored header lies about it.**

webOS inserts a `Uint32 inputSource` before the text, so the UTF-8 bytes start at **+16**, not +12.
Both halves are checkable on the dev Mac without a television:

- `include/SDL2/SDL_events.h` — the tree we compile against — declares
  `SDL_TextInputEvent { type, timestamp, windowID, text[] }`, i.e. text at **+12**. That tree is
  stock **2.0.4** (`include/SDL2/SDL_version.h`).
- The NDK sysroot's fork copy
  (`$(WEBOS_SDK)/arm-webos-linux-gnueabi/sysroot/usr/include/SDL2/SDL_events.h`, SDL **2.24.1**)
  declares `{ type, timestamp, windowID, inputSource /* webOS specific field */, text[] }` — text at
  **+16**.

This is the same class of bug as the `SDL_KeyboardEvent` shift `app.rs` already reads around with
raw offsets, and it has the same fix: read the bytes, do not trust the struct. Note the offset is a
`cfg`, not a constant — under `hostsim` desktop SDL2 is stock and the offset really is +12, so a
single hard-coded number ships garbage on one of the two platforms.

**2. `SDL_WINDOW_INPUT_FOCUS` is a silent precondition.**

`SDL_StartTextInput` looks for a window carrying `0x200` (`include/SDL2/SDL_video.h`) before
dispatching to the driver hook. Our window is created with
`SDL_WINDOW_OPENGL | SDL_WINDOW_FULLSCREEN` and nothing else — `app.rs`'s `SDL_WINDOW_FLAGS` is
`0x2 | 0x1` on the device — so it carries neither `SHOWN` nor a focus flag at creation. **If the flag
is clear the panel never rises, silently and with no error return.** That is why the boot probe logs
the flag rather than trusting it: a keyboard that does not appear and a keyboard that appeared and
was dismissed look identical from inside the app.

**3. A reopen wedge we inherit and cannot patch.**

The panel cannot be reopened after dismissal (moonlight-tv issue #435, reproduced on webOS 7.4). The
community fix lives in webosbrew's *bundled* SDL fork; we call the **television's own** SDL, so we
get the bug with no patch. `SDL_SetTextInputRect` is a no-op here too — open and close, no
positioning.

This is a real constraint on the interaction design, not a footnote: the screen must not treat
"dismiss and raise again" as a normal gesture, because on some firmwares it is a one-way door.

## On the simulator, which is a different keyboard stack entirely

Two host-side findings, recorded because both cost time today and neither is a statement about the
television.

**The boot probe's answer on macOS is `keyboard: support=0 active=1 focus=0 winflags=0x26`.** That
is **trap 2 observed**: `0x26` is `OPENGL | SHOWN | RESIZABLE` (`app.rs`'s `hostsim` arm asks for
`0x2 | 0x20`; SDL adds `SHOWN`), and `0x200` — `SDL_WINDOW_INPUT_FOCUS` — is not in it. The trap is
therefore reproducible without a television, which is the useful part; what it does **not** tell you
is anything about the TV's own answer, since `support=0` here just means desktop SDL has no screen
keyboard at all.

**Do not push a synthetic `SDL_TEXTINPUT` through `SDL_PushEvent` on the host — it SIGSEGVs inside
SDL.** macOS `libSDL2` is **sdl2-compat**, a shim forwarding into SDL3, and SDL3's text event
carries a `char *text` **pointer** where SDL2 carries an inline `char[32]`; the compat layer
dereferences what it is handed. There is no Rust panic and no log line — the process is simply gone.
The existing remote-FIFO injection is safe by luck of shape: key events and `ck:` pointer clicks are
all scalar fields, and nothing in them is read as a pointer. Anything that wants to test the field
headlessly must go in **above** SDL — through `textinput`'s own buffer — not by forging the event.

## Two dead ends, so nobody spends a day on them again

- **The Luna route.** There are only four `com.webos.service.ime/*` methods, and all of them sit in
  ACGs this app does not hold: `pkg/appinfo.json`'s `requiredPermissions` is `database.operation`
  and `securitykey.operation` — both the storage helper's, for its DB8 keystore and the platform key
  manager (neither an IME group; `docs/distribution.md` has the full account) — so the app is
  granted those two plus `["public"]`, never the IME ACGs. Declaring more is not a fix; those groups
  are not grantable to a homebrew app.
- **A physical USB or Bluetooth keyboard.** `/dev/input` is not mounted into our jail.
  `rust-modules/src/remote.rs`'s module doc records the general case and why it generalises: on this
  build the wayland compositor (`surface-manager`) opens a **fixed** set of evdev nodes at boot and
  never picks up hotplugged or `uinput` devices, and LG's
  `com.webos.service.tv.keymanager/createKeyEvent` injects into the webOS *web-app* key layer, not
  the wayland path we read. External input does not reach our surface by any route tried.

## Provenance

Re-verified on the host while writing this file: the two SDL header trees and their versions, the
window flags, `SDL_WINDOW_INPUT_FOCUS`'s value, `SDL_webOS.h`'s eight entry points, the 14-firmware
symbol sweep, and `appinfo.json`'s `requiredPermissions` list (neither entry an IME ACG). Measured on the **simulator**, and
so about a Mac and not a television: the boot probe read-out and the `SDL_PushEvent` crash above.
Recorded from the device and
disassembly work that produced `rust-modules/src/textinput.rs`'s module doc, and **not**
re-established here: the four `Wayland_CreateDevice` hooks, `WebOSShowScreenKeyboard`'s `text_model`
client, the moonlight-tv reproduction, the four Luna IME methods, and the jail's missing
`/dev/input`.
