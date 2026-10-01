# Zed for Hyprmux

This branch of Zed opens its windows as tiles in
[Hyprmux](https://github.com/gavrix/hyprmux), a tiling workspace for macOS. It
ships as `Zed.hmapp`, an app bundle that only Hyprmux opens.

## Install

1. Download `Zed-for-Hyprmux-VERSION-aarch64.zip` from
   [Releases](https://github.com/gavrix/zed/releases) and unzip it.
2. Move `Zed.hmapp` into `~/.config/hyprmux/apps/`.
3. Run `hyprmuxctl apps refresh`, or reopen the launcher.
4. Press ⌘D in Hyprmux and open Zed.

You need Hyprmux 0.3.0 or later on an Apple-silicon Mac. To update, replace
`Zed.hmapp` with the new one. It doesn't update itself.

## How it differs from Zed

- **Windows are tiles.** When Hyprmux launches it, every Zed window becomes a
  Hyprmux tile. Outside Hyprmux the binary behaves like a normal Zed.
- **Nightly channel.** It keeps its own database and session apart from a
  stable Zed, so both can run at once. Settings, keymaps, and extensions in
  `~/.config/zed` are shared.
- **No auto-update.** Zed's updater would replace this build with upstream
  Zed.
- **One process per tile launch.** Each launch from Hyprmux starts its own
  process instead of handing the paths to a running one.
- **No Dock icon or menu bar in Hyprmux.** Use the command palette.
- **No `zed` command-line tool.** Open folders with
  `hyprmuxctl launch Zed PATH`.

## The code

- `crates/gpui_macos/src/hyprmux.rs`: the GPUI backend. It renders with GPUI's
  Metal renderer into IOSurfaces it shares with Hyprmux, and turns Hyprmux's
  input events back into GPUI events.
- `crates/gpui_macos/src/platform.rs`: routes windows, cursors, and file
  dialogs to the backend when Hyprmux launched the process.
- `crates/gpui_apple/src/metal_renderer.rs`: an offscreen renderer that draws
  into a caller's texture.
- `crates/zed/src/main.rs`: skips the single-instance check under Hyprmux.
- The protocol client is
  [`hyprmux-client`](https://github.com/gavrix/hyprmux/tree/master/clients/rust/hyprmux-client),
  a git dependency.

## Build

```sh
script/bundle-hyprmux              # build, sign, notarize, zip
script/bundle-hyprmux 1.24.0       # set the version in Info.json and the file name
HYPRMUX_SKIP_NOTARIZATION=1 script/bundle-hyprmux   # local test build
```

Signing needs a `Developer ID Application` certificate, and notarization a
`notarytool` Keychain profile (`hyprmux-notary` by default). The script's header
lists the options. For development, run a normal build and add it to Hyprmux:

```sh
cargo build --profile release-fast
hyprmuxctl apps add "Zed (dev)" target/release-fast/zed
```

Zed is licensed under GPL-3.0-or-later and Apache-2.0. This fork keeps those
licenses. Zed is a trademark of Zed Industries; this build isn't affiliated
with them.
