//! Runs the selection overlay (`selector.rs`) in a child process.
//!
//! Why a separate process: when `cosmic::app::run()` returns, libcosmic's
//! Wayland event-loop thread keeps running and later touches freed Wayland
//! objects (use-after-free). It crashed the recording at random (glibc
//! "tpp.c" assertion, SIGSEGV on focus change). When the child process exits,
//! all of its threads, Wayland connection and GPU memory go away with it.
//!
//! The child is the same binary, started with the hidden `--select-only`
//! flag. It prints `REGION x,y,w,h` (or `REGION full` for the whole screen)
//! on stdout when the user confirms, then
//! stays on screen as a recording indicator until we close it.

use std::io::Write;
use std::process::Stdio;
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::region::Region;
use crate::selector;

/// Prefix of the line the child prints, so a stray log line (from libcosmic
/// or a driver) can't be mistaken for the answer.
const REGION_PREFIX: &str = "REGION ";
/// What follows `REGION_PREFIX` for the whole screen.
const FULL_SCREEN: &str = "full";

/// The overlay shown during the recording. Dropping it closes it.
pub struct Overlay {
    // Never read: owning it is the point. `kill_on_drop` stops the child when
    // this is dropped, and closing its stdin makes it exit by itself.
    _child: Child,
}

/// Shows the overlay and waits for the user. Returns the region (`None` =
/// the whole screen) and the overlay (still visible), or `None` if the user
/// cancelled.
pub async fn select() -> Result<Option<(Option<Region>, Overlay)>> {
    let exe = std::env::current_exe().context("cannot find our own executable")?;
    let mut child = Command::new(exe)
        .arg("--select-only")
        // stdin: lets the child notice when we're gone (see `run_child`).
        .stdin(Stdio::piped())
        // stdout: that's where the answer comes.
        .stdout(Stdio::piped())
        // stderr: the child's errors still show in our terminal.
        .stderr(Stdio::inherit())
        // If we stop waiting (e.g. cancelled by the shortcut), close it.
        .kill_on_drop(true)
        .spawn()
        .context("cannot start the selection overlay")?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        let Some(region) = line.strip_prefix(REGION_PREFIX) else {
            continue;
        };
        let region = if region == FULL_SCREEN {
            None
        } else {
            Some(
                Region::parse(region)
                    .map_err(|e| anyhow::anyhow!("overlay sent a bad region: {e}"))?,
            )
        };
        // Keep reading in the background, so the child never blocks on a
        // full pipe if it prints something else.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        return Ok(Some((region, Overlay { _child: child })));
    }

    // stdout closed without a region: the overlay exited.
    let status = child.wait().await?;
    if !status.success() {
        bail!("selection overlay failed ({status})");
    }
    Ok(None)
}

/// Child-process side: runs the overlay, prints the region when confirmed.
/// Never returns normally.
pub fn run_child() -> Result<()> {
    // If the parent dies without closing us, stdin closes: leave, so the
    // overlay never stays stuck on screen.
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });

    // The overlay sends the region on a channel; print it right away.
    let (selected, regions) = mpsc::channel::<Option<Region>>();
    std::thread::spawn(move || {
        for region in regions {
            match region {
                Some(r) => println!("{REGION_PREFIX}{},{},{},{}", r.x, r.y, r.width, r.height),
                None => println!("{REGION_PREFIX}{FULL_SCREEN}"),
            }
            let _ = std::io::stdout().flush();
        }
    });

    // Returns only when the user cancels.
    selector::run(selected)?;
    // Leave right away: skipping the normal cleanup avoids racing with
    // libcosmic's leftover thread (see the module docs).
    std::process::exit(0);
}
