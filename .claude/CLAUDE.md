# cosmic-screencast

Record a screen region to an animated GIF on the COSMIC desktop (Wayland).
Goal: quick, lightweight demos to send by email ("click here", "here's the result").
Low framerate and modest quality are fine; **not lagging the system is a priority**:
it must run smoothly on low-end machines.

Intended to be published as open source and offered to the COSMIC community as an applet.

Code, comments, console messages and docs are in English.

## Architecture

```
main.rs      CLI (clap). Runs the selector in a child process (`--select-only`),
             then builds a tokio runtime and records.
selector.rs  libcosmic app, no main window: one fullscreen transparent layer-shell
             surface (Layer::Overlay, active output). Returns the region via a channel.
selection.rs Custom iced widget: draw / move / resize a rectangle. Look copied from
             the screenshot portal (30% black dim, 4px accent border, corner handles).
portal.rs    XDG ScreenCast portal (ashpd): gets a PipeWire fd + node id.
capture.rs   PipeWire stream (own thread): skips frames to reach --fps, crops to the
             region, converts to RGBA, sends frames on a bounded channel.
gif.rs       gifski encoder thread, encodes while recording.
```

Flow: overlay → portal → PipeWire capture ⇄ channel ⇄ gifski → `~/Videos/Screencasts/screencast_<date>.gif`
(`dirs::video_dir()` is localized, e.g. `~/Vidéos`).

## Technical decisions & gotchas

- **Restore token**: `PersistMode::ExplicitlyRevoked`, token stored in
  `~/.local/state/cosmic-screencast/restore_token`. COSMIC honors it: the
  screen-picker dialog only shows on first run. A new token is issued every run.
- **Keep the portal session alive** for the whole recording (`PortalStream` owns it),
  close it at the end.
- **Do not constrain framerate in the PipeWire format pod.** COSMIC imposes its own
  max-framerate; a narrower range makes negotiation fail silently
  (`Error("no more output formats")`, 0 frames). COSMIC sends ~40 fps when the screen
  changes, nothing when static; we skip extra frames ourselves.
- Frames are **SHM only** (no DMA-BUF modifiers offered), formats BGRx/BGRA/RGBx/RGBA.
  Respect `chunk().stride()`.
- Region is in **logical** coordinates; frames are physical pixels. `capture.rs` scales
  using the portal's logical size (HiDPI not yet tested).
- Capture never blocks: `try_send` on a bounded channel, frames dropped if the encoder lags.
- On stop, the last frame is re-added at stop time so a static ending stays visible.
- **The overlay runs in a child process** (same binary, hidden `--select-only` flag,
  prints `REGION x,y,w,h` on stdout). After `cosmic::app::run()` returns, libcosmic's
  Wayland event-loop thread (`SctkEventLoop`) keeps running and touches freed Wayland
  objects: random crashes during recording (glibc `tpp.c` assertion in
  `wl_display_cancel_read`, SIGSEGV on focus change). Do not run libcosmic in the
  recording process.
- libcosmic is a git dependency pinned to a `rev`. First build is long (15+ min on a
  low-end machine); dependencies are built with `opt-level = 3` even in debug.
- `cargo clippy` / `cargo check` do **not** rebuild the binary: run `cargo build`
  before testing a change.
- Interactive testing needs a human (overlay). For non-interactive tests:
  `cargo run --release -- --region X,Y,W,H --duration 4 -o /path/test.gif`.

## Checks before handing over

`cargo fmt && cargo clippy && cargo build`

## Roadmap

Done: 1. PipeWire capture · 2. crop + GIF · 3. selection overlay.

Next:
- Turn it into a COSMIC panel applet (libcosmic `applet` feature): start / stop
  recording from the panel. This changes the app structure (long-running process).
- Keep the selection rectangle visible during recording (overlay must not be
  captured / must not take input).
- A way to stop recording without a terminal (panel icon, shortcut, notification...).
- Optional MP4 output.
- Multi-monitor: overlay is on the active output, capture is on the portal-remembered
  monitor; they must match.
- i18n (fluent, see below), packaging, CPU optimization if needed.

## COSMIC conventions (see ../cosmic-screenshot for a reference project)

- `justfile` (+ `cargo.just`) for build / install / vendoring; `debian/` for packaging.
- i18n with fluent: `i18n.toml`, `i18n/<lang>/<crate_name>.ftl`, `fl!()` macro
  (`i18n-embed`, `i18n-embed-fl`, `rust-embed`).
- `resources/<APP_ID>.desktop` + `resources/icons/hicolor/...`.
- Reverse-DNS app ID (System76 uses `com.system76.Cosmic*`; third-party apps use
  their own domain). Current placeholder: `com.github.cosmic-screencast`.
- License: COSMIC projects are GPL-3.0 (project license not chosen yet).
- `rustfmt.toml` in COSMIC repos: `imports_granularity = "Module"`.
- Release profile: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
