# Install Dispatcharr TV on an LG webOS TV

Dispatcharr TV is an unofficial, open-source live-TV app for LG webOS TVs that plays the channels of
your own [Dispatcharr](https://github.com/Dispatcharr/Dispatcharr) server. It is not in the LG
Content Store or in the Homebrew Channel catalogue; you install it on a regular LG webOS TV using
LG's Developer Mode and a computer. **No root is required.** If something goes wrong, see
[troubleshooting](troubleshooting.md).

## Before you start

You will need:

- an LG TV running **webOS 4.0 or newer**; check the [compatibility notes](../README.md#compatibility) —
  playback has been watched on webOS 4.4.3 and 6.5.3, and other versions are expected to work but
  have not been seen to;
- a **Dispatcharr server** your TV can reach on your network, and a Dispatcharr username and
  password;
- a Mac, Windows PC, or Linux computer for the setup described here;
- the TV and computer on the **same local network**.

The route is **set up the TV → connect from your computer → install Dispatcharr TV**. No LG SDK or IDE is required.

> **Important:** Developer Mode needs periodic renewal. If it expires and LG disables Developer Mode, apps installed through it are removed. [How to keep the apps installed](#important-developer-mode-expires).

## 1. Enable Developer Mode on the TV

1. **On your computer**, visit [LG's developer website](https://webostv.developer.lge.com/) and select **Sign In**. Sign in or create an account, completing any account verification LG requests.
2. **On your TV**, open **LG Content Store / Apps**, search for **Developer Mode**, and install it.
3. Open the **Developer Mode** app and sign in with that LG account.
4. Turn **Dev Mode Status** on. The TV will restart.
5. Open **Developer Mode** again, confirm **Dev Mode Status** is on, and turn **Key Server** on.
6. Keep this screen open. You will need the TV's **IP address** and the **Passphrase** displayed here to connect from your computer. The IP address is also available in the TV's network settings.

[LG's official guide](https://webostv.developer.lge.com/develop/getting-started/developer-mode-app) has screenshots of these controls. **Do not continue into its CLI or webOS Studio setup** for this installation; use Dev Manager in the next step instead.

**Ready to continue:** Developer Mode is on, Key Server is on, and you have the TV's IP address and Passphrase.

## 2. Connect your computer to the TV

**webOS Dev Manager is the app on your computer.** It connects to the Developer Mode app on your TV and installs packages for you.

1. [Download the latest webOS Dev Manager](https://github.com/webosbrew/dev-manager-desktop/releases/latest). In **Assets**, choose the installer for your computer: `.dmg` for macOS, `.msi` for Windows, or the appropriate Linux package. The project's [download table](https://github.com/webosbrew/dev-manager-desktop#download) explains the architecture choices. Do not download **Source code**.
2. Install and open Dev Manager. Start its setup wizard to add a TV and select **Use Developer Mode**. Do not select the Homebrew Channel SSH option; that is for an existing rooted setup.
3. The wizard also lists the TV preparation steps. Since you completed them above, mark them complete and continue to the connection form.
4. Fill in the connection details:

   | Field | What to enter |
   |---|---|
   | **Device Name** | A name you choose, such as `living-room-tv`. |
   | **Address** | Your **TV's local IP address**, not your Dispatcharr server's address. |
   | **Passphrase / authentication info** | The **Passphrase shown in the TV's Developer Mode app**, with the same capitalisation. This is **not your LG account password**. |

   Keep the Developer Mode defaults: **Username** `prisoner`, **Port** `9922`, and **Authentication** `Dev Mode`. The wizard sets these for you.
5. Finish adding the TV. In Dev Manager, open **Apps** and check that the **Installed** tab can load the TV's apps.

**Ready to continue:** Dev Manager can display the TV's installed apps. If it cannot connect, see [Connection problems](#connection-problems) before downloading Dispatcharr TV.

## 3. Install Dispatcharr TV

1. On your computer, open the [latest Dispatcharr TV release](https://github.com/andrewmagness/webos-dispatcharr/releases/latest).
2. Under **Assets**, download **`com.magness.dispatcharrtv_X.Y.Z_arm.ipk`**. `X.Y.Z` is the release's version number. The `.ipk` is the TV app, not something to open on your computer; the source archives are not installers.
3. [Check the package against the release checksum](#verifying-the-package) before installing it.
4. In Dev Manager, select your TV, open **Apps**, and click **Install**. Choose the downloaded `.ipk`.
5. Wait for installation to finish and check that **Dispatcharr TV** appears under **Installed**.

For future updates, repeat this with the newer `.ipk`. Install over the existing app rather than uninstalling it first, so it keeps your sign-in and favourites.

Want to try the newest build before it is released? See the [nightly builds guide](nightly-builds.md).

## 4. Open Dispatcharr TV and sign in

Open **Dispatcharr TV** from the TV's app launcher. Setup looks for a Dispatcharr server on your
network and offers what it finds; you can also type the server's address. Then enter your
Dispatcharr username and password with the TV's on-screen keyboard. The TV keeps the API key your
server issues, never your password.

**Installation is complete.** The Guide opens with your channels. The remaining setup responsibility is keeping Developer Mode active, as described below.

## Important: Developer Mode expires

For installations made through LG Developer Mode:

1. While the TV is online, open the **Developer Mode** app and check **Remain Session**.
2. **Before the remaining time runs out**, select **EXTEND**.
3. Check that the remaining time has increased. Repeat before the next expiry; a calendar reminder can help.

LG does not let you extend an already-expired session. It documents that restarting the TV after expiry disables Developer Mode and removes apps installed through it. Re-enable Developer Mode and reinstall the apps if this happens.

Do not turn off Developer Mode after installation. **Key Server is different:** it is needed when initially adding the TV to Dev Manager, not for everyday playback, and it may turn off after a restart.

See [LG's session guidance](https://webostv.developer.lge.com/develop/getting-started/developer-mode-app#extending-developer-mode-time) and [webOSbrew's Developer Mode notes](https://www.webosbrew.org/devmode/).

## Connection problems

**Dev Manager cannot reach the TV:** check that the TV is on, both devices are on the same local network, and you entered the TV's current IP address. Guest Wi-Fi, device isolation, or a VPN can prevent a local connection.

**Adding the TV fails at authentication:** reopen Developer Mode on the TV, enable **Key Server**, and copy the current **Passphrase** exactly. In Dev Manager, use **Use Developer Mode**, not the rooted-TV SSH option or password authentication.

**The apps disappeared:** check **Dev Mode Status** and **Remain Session**. If Developer Mode has been disabled, enable it again and reinstall the apps using the same route.

**Setup does not find your server:** type its address instead (for example `http://192.168.1.10:9191`). The TV must be able to reach the server directly; a server that only your computer can reach, through a VPN for instance, is not reachable from the TV.

For more connection help, see [webOSbrew's troubleshooting guide](https://www.webosbrew.org/devmode/#troubleshooting). To [report a Dispatcharr TV problem](https://github.com/andrewmagness/webos-dispatcharr/issues/new/choose), include your TV model, webOS version, Dispatcharr TV version and the exact error. Do not post passwords, the Passphrase, your server's API key, or screenshots containing them.

If Dispatcharr TV itself is installed and opens but something inside it doesn't work — sign-in, the guide, or playback — see [Troubleshooting](troubleshooting.md).

---

## Checking what you downloaded

The installation walkthrough ends above. The sections below are a technical reference for package verification, release provenance, and what the app reads, writes, and connects to.

The app runs in LG's normal sandbox: an unprivileged uid, chrooted, under the stock jail profile. `appinfo.json` declares two ACGs, `database.operation` and `securitykey.operation`; both belong to the bundled storage helper, and neither reaches the network or anything outside this app's own data.

Per-release facts — the hash, sizes, payload, and what was tested on which set — are in that version's [technical audit](https://github.com/andrewmagness/webos-dispatcharr/tree/main/docs/release-audits).

### Which file to download

**For installation, you need the first one.**

| File | What it is |
|---|---|
| `com.magness.dispatcharrtv_X.Y.Z_arm.ipk` | The app. |
| `ipk.sha256` | The checksum, for `sha256sum -c`. |
| `ffmpeg-9.0.tar.xz` | The pristine upstream FFmpeg source, published because we are obliged to. |
| `build-ffmpeg.sh` | The complete configure invocation that produced the bundled FFmpeg libraries. |
| `dispatcharrtv-source-X.Y.Z.tar.gz` | The app's own complete source, as built, with its `.release.json` record beside it and `source-rebuild.json`, the result of rebuilding the app from that archive. |

### Verifying the package

Nothing in this distribution chain is code-signed, so the SHA-256 published with the release is what tells you that the file you downloaded is the file that was published there.

Download `ipk.sha256` from the same release as the `.ipk` and open a terminal in the download folder. Replace `X.Y.Z` with the version you downloaded. On macOS and Windows, compare the printed hash with the one in `ipk.sha256`; on Linux, the check below should report `OK`. **Do not install the package if the hashes do not match.**

```sh
shasum -a 256 com.magness.dispatcharrtv_X.Y.Z_arm.ipk              # macOS
sha256sum -c ipk.sha256                                    # Linux, with the checksum asset beside it
certutil -hashfile com.magness.dispatcharrtv_X.Y.Z_arm.ipk SHA256 # Windows
```

[Return to the installation](#3-install-dispatcharr-tv) after checking the file.

#### Rebuilding it to compare

Two builds of one commit on one machine produce a byte-identical `.ipk`. It is **not** reproducible across machines yet — the bundled FFmpeg records the toolchain paths it was built against — so a hash from your own rebuild will differ, and that is not evidence of tampering. Each audit's **Reproducibility evidence** section shows exactly which paths a given package carries.

Every release is built and uploaded by GitHub Actions from the tag. If a release's assets were uploaded by a person rather than by `github-actions[bot]`, the build and verification gates did not run — the audit records the uploader for exactly this reason.

## What the app does on your television

This section is invariant across releases. Where a release changes one of these behaviours, its release note says so and its audit measures it. [`PRIVACY.md`](https://github.com/andrewmagness/webos-dispatcharr/blob/main/PRIVACY.md) is the full contract.

### What it writes

- **Your account** — the server's address, your username and the API key the server issued — in one file under `/media/developer` (or `/media/internal`), mode `0600`, readable only by the app. LG's Developer Mode widens that directory's permissions at every boot; the app narrows the file back the next time it reads it.
- **Your choices for that server** — favourite channels, the last channel watched and whether the app starts on it — in a second file beside it. Channels are named by your server's identifiers for them.
- **Channel logos**, cached as bounded files for reuse across restarts.
- **Local logs** in `/tmp`, mode `0640` — owner read/write and the app's group read, so that Dev Manager can fetch them for a bug report: `dispatcharrtv-events.log` and `dispatcharrtv-stderr.log` (emptied every launch) and `dispatcharrtv-crash.log` (kept until the TV restarts). The event log replaces your username before it is written. The app never sends them anywhere.

Signing out (in Setup) removes the account file. webOS gives an app no way to run code as it is removed, so these files can survive an uninstall.

A crash writes no core file.

### What it reads outside its own directory

The television's codec table at `/etc/umediaserver/device_codec_capability_config.json`, and its firmware identity at `/var/run/nyx/os_info.json` and `/var/run/nyx/device_info.json`. All three are published by the platform, read once at boot, and never written.

### What it reaches

Your Dispatcharr server, at the address Setup found or you typed — over HTTPS when the address says `https://`, otherwise over plain HTTP on your network — and the channel logo images your server's channels point at. During Setup it also looks for a Dispatcharr server on the local network.

Nothing is sent anywhere else: the app has no crash reporting and no analytics. A build carries an endpoint only if one was compiled into it, so `strings` on the binary answers the question directly, and each release audit reports what it found there.

### What listens

Nothing. A release build compiles out the whole `/tmp` trigger surface, the remote-control FIFO, and the TCP capture listener that exist in a development build. Each audit measures this on the shipped bytes rather than asserting it.

## Scope

Live TV from a Dispatcharr server: the channels, groups and programme guide your server provides. No recordings, no catch-up, no video on demand.

## The bundled FFmpeg

The package contains three FFmpeg shared libraries — `libavformat-plx.so.63`, `libavcodec-plx.so.63`, and `libavutil-plx.so.61` — built from **FFmpeg 9.0**, unmodified, and licensed **LGPL-2.1-or-later**.

They contain demuxers, parsers, bitstream filters, and subtitle decoders only: video and audio are decoded by the television's own hardware.

The complete corresponding source accompanies every release, as LGPL-2.1 §6 requires and not as a courtesy: `ffmpeg-9.0.tar.xz` is the pristine upstream tarball with no patches applied, and `build-ffmpeg.sh` is the complete configure invocation that produced the libraries.

It is built with `--disable-everything` plus an explicit component list, and **without** `--enable-gpl`, `--enable-version3`, or `--enable-nonfree`, so no GPL or non-free FFmpeg component is present. Each audit quotes the configure string recorded inside `libavutil` itself, which is the primary evidence for that.

They are ordinary shared libraries, `dlopen`ed by absolute path out of the app's own directory under exactly those names, so they can neither shadow nor be shadowed by the television's own FFmpeg — and a build of your own with the same names replaces ours.

Full licence text travels inside the package, in `THIRD-PARTY-NOTICES.md` and `licenses/`.

The bundled build is configured with `--disable-network` and `file` as its only protocol, so it cannot open a URL at all; everything it demuxes arrives through the app's own transport.
