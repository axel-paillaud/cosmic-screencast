# cosmic-screencast

Record a screen region to an animated GIF on the COSMIC desktop (Wayland).
Goal: quick, lightweight demos to send by email ("click here", "here's the result").
Low framerate and modest quality are fine; **not lagging the system is a priority**:
it must run smoothly on low-end machines.

Intended to be published as open source and offered to the COSMIC community as an applet.

Code, comments, console messages and docs are in English.

## Architecture

```
main.rs      CLI (clap). Claims the single instance, picks the region (overlay),
             records (tokio), notifies.
instance.rs  Single instance over D-Bus: a second launch calls `Stop` on the first.
overlay.rs   Runs selector.rs in a child process (`--select-only`); the overlay
             stays as a recording indicator until the parent drops it.
notify.rs    Desktop notifications (saved / failed), like cosmic-screenshot.
selector.rs  libcosmic app, no main window: one fullscreen transparent layer-shell
             surface (Layer::Overlay, active output). Sends the region on a channel,
             then switches to "recording" mode (locked, click-through).
selection.rs Custom iced widget: draw / move / resize a rectangle. Look copied from
             the screenshot portal (30% black dim, 4px accent border, corner handles).
portal.rs    XDG ScreenCast portal (ashpd): gets a PipeWire fd + node id.
capture.rs   PipeWire stream (own thread), paced by holding buffers (≤ --fps frames/s),
             reads each frame (DMA-BUF or SHM), sends it on a bounded channel.
formats.rs   Format pods offered to PipeWire (DMA-BUF LINEAR first, then SHM).
dmabuf.rs    Maps a DMA-BUF (GPU memory) for CPU reads.
region.rs    `Region` + logical → physical pixels conversion (HiDPI, clamping).
pixels.rs    Crop a raw frame + convert BGR/RGB to RGBA.
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
  (`Error("no more output formats")`, 0 frames). The portal ignores framerate requests
  (`// TODO max framerate` in xdg-desktop-portal-cosmic).
- **Pacing by holding buffers.** The portal copies a frame (full screen, GPU → RAM for
  SHM) only into buffers we give back. Returning them immediately = compositor copies
  at refresh rate whenever the screen changes = laggy mouse. `capture.rs` keeps every
  dequeued buffer (raw `pw_buffer` pointers) and a loop timer queues one back every
  1/fps s. Held buffers must be dropped in `remove_buffer` and queued back before the
  stream is destroyed.
- **DMA-BUF first, SHM fallback.** Two EnumFormat pods (BGRx/BGRA/RGBx/RGBA): first with
  modifier LINEAR only (`MANDATORY | DONT_FIXATE`, the portal fixates), then plain SHM.
  With DMA-BUF the compositor copies GPU → GPU (no stall); `dmabuf.rs` mmaps the fd with
  `DMA_BUF_IOCTL_SYNC` and we read only the selected rows. GPU memory is uncached:
  copy whole rows (`copy_from_slice`), never read pixel by pixel (was ~10× slower).
  `--no-dmabuf` forces SHM. Respect `chunk().stride()` / `offset()`.
- **Runtime fallback = new portal session.** If a DMA-BUF can't be mapped, `capture::run`
  returns `Outcome::DmabufUnreadable` and `main.rs` reopens the portal with SHM only.
  Renegotiating in the same stream does NOT work: the portal keeps its modifier and
  either keeps sending DMA-BUFs or fails with `error alloc buffers: Invalid argument`.
- Region is in **logical** coordinates; frames are physical pixels. `capture.rs` scales
  using the portal's logical size (HiDPI not yet tested).
- Capture never blocks: `try_send` on a bounded channel, frames dropped if the encoder lags.
- On stop, the last frame is re-added at stop time so a static ending stays visible.
- **Start/stop toggle.** Meant to be bound to one shortcut (COSMIC Settings > Keyboard >
  Custom shortcuts, e.g. Ctrl+Print → absolute path of the binary). First launch owns
  `com.github.cosmic_screencast` on the session bus and serves
  `com.github.cosmic_screencast.Control.Stop`; a second launch calls it and exits.
  Request the name with `DO_NOT_QUEUE` only (zbus defaults include REPLACE_EXISTING),
  and "taken" comes back as `Err(zbus::Error::NameTaken)`, not as a reply.
  Stop during the selection cancels it (the overlay child is `kill_on_drop`).
- **The overlay runs in a child process** (same binary, hidden `--select-only` flag,
  prints `REGION x,y,w,h` or `REGION full` on stdout, then stays alive as the recording indicator:
  `set_input_zone(Some(vec![]))` = click-through, keyboard interactivity None, no
  handles/toolbar). The parent drops it (`kill_on_drop`) when the capture stops; the
  child also exits when its stdin closes (parent died), so it never stays stuck.
  Dim + border are drawn **outside** the region (2 px gap, border outside the gap):
  the recording is cropped to the region, so the overlay never appears in the GIF.
  Whole screen = `REGION full` / `None`: libcosmic does NOT call `on_window_resize`
  for layer surfaces, so the overlay doesn't know the screen size. While recording,
  the overlay draws the frame along the screen edges, and `main.rs` records
  `Region::inside_screen(portal logical size, FULL_SCREEN_INSET)` (6 px less on each
  side) to keep it out of the GIF. Corner handles sit on the border's outer
  corners (`handle_centers`).
  Originally a child process because after `cosmic::app::run()` returns, libcosmic's
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

`cargo fmt && cargo clippy --all-targets && cargo test && cargo build`

Unit tests cover the pure functions (`region.rs`, `pixels.rs`, `parse_region`).

## Roadmap

Done: 1. PipeWire capture · 2. crop + GIF · 3. selection overlay · 4. pacing + DMA-BUF
· 5. start/stop with the same command (shortcut) · 6. notifications · 7. overlay kept
as recording indicator.

Next:
- Notifications: an "Open" action (needs to stay alive for `ActionInvoked`), our own
  icon (currently the themed `media-record`), i18n of the texts.
- Install the default shortcut (Ctrl+Print) at install time. Panel applet: decided
  not needed for now (cosmic-screenshot has none; demos are short).
- Optional MP4 output.
- Multi-monitor: overlay is on the active output, capture is on the portal-remembered
  monitor; they must match.
- Same menu at bottom screen than cosmic-screenshot, with selector GIF - MP4 (can choose both)
- Thinking: We could keep the menu visible during the recording, including an 'X' icon, so that clicking it would stop the screencast. This way, if a user doesn't know the keyboard shortcut to start the recording, they won't end up stuck with that box on their screen.
but it remains to be seen whether this interferes too much with the capture (I don’t think it does). the menu needs to remain invisible, even when it’s part of the selection—is that possible?
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
