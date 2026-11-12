mod capture;
mod dmabuf;
mod gif;
mod portal;
mod selection;
mod selector;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use capture::Region;

/// Record a screen region to an animated GIF.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Region to record, in screen coordinates: X,Y,WIDTH,HEIGHT.
    /// Shows a selection overlay if omitted.
    #[arg(short, long, value_parser = parse_region)]
    region: Option<Region>,

    /// Frames per second.
    #[arg(long, default_value_t = 10)]
    fps: u32,

    /// Stop after this many seconds (otherwise, stop with Ctrl+C).
    #[arg(short, long)]
    duration: Option<f64>,

    /// GIF quality, 1-100. Lower = smaller file.
    #[arg(short, long, default_value_t = 90, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,

    /// Downscale the GIF if it is wider than this, in pixels.
    #[arg(long)]
    max_width: Option<u32>,

    /// Output file. Defaults to ~/Videos/Screencasts/screencast_<date>.gif.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Read frames from shared memory instead of GPU memory (DMA-BUF).
    /// Slower for the desktop; use it if recording fails.
    #[arg(long)]
    no_dmabuf: bool,

    /// Internal: only show the selection overlay, print the region on stdout
    /// and exit. Used by the parent process, see `pick_region`.
    #[arg(long, hide = true)]
    select_only: bool,
}

/// Prefix of the line the child process prints, so the parent can't mistake
/// a stray log line (from libcosmic or a driver) for the answer.
const REGION_PREFIX: &str = "REGION ";

/// Parses "X,Y,WIDTH,HEIGHT" into a `Region`. clap calls this for `--region`.
fn parse_region(s: &str) -> Result<Region, String> {
    let parts: Vec<u32> = s
        .split(',')
        .map(|p| p.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("invalid number: {e}"))?;
    match parts[..] {
        [x, y, width, height] if width > 0 && height > 0 => Ok(Region {
            x,
            y,
            width,
            height,
        }),
        _ => Err("expected X,Y,WIDTH,HEIGHT with WIDTH and HEIGHT > 0".into()),
    }
}

/// Same convention as COSMIC screenshots (~/Pictures/Screenshots).
/// `video_dir()` follows the XDG user dirs, so it's localized (~/Vidéos).
fn default_output() -> Result<PathBuf> {
    let dir = dirs::video_dir()
        .or_else(dirs::home_dir)
        .context("cannot find the videos directory")?
        .join("Screencasts");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let now = jiff::Zoned::now().strftime("%Y-%m-%d_%H-%M-%S");
    Ok(dir.join(format!("screencast_{now}.gif")))
}

/// Shows the selection overlay in a child process and returns the region.
/// `None` means the user cancelled.
///
/// Why a separate process: when `cosmic::app::run()` returns, libcosmic's
/// Wayland event-loop thread keeps running and later touches freed Wayland
/// objects (use-after-free). It crashed the recording at random (glibc
/// "tpp.c" assertion, SIGSEGV on focus change). When the child process exits,
/// all of its threads, Wayland connection and GPU memory go away with it.
fn pick_region() -> Result<Option<Region>> {
    let exe = std::env::current_exe().context("cannot find our own executable")?;
    // stdout is captured (that's where the answer is); stderr is inherited so
    // the child's errors still show in the terminal.
    let output = std::process::Command::new(exe)
        .arg("--select-only")
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("cannot start the selection overlay")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // `find_map` returns the first line for which the closure returns `Some`.
    let region = stdout
        .lines()
        .find_map(|line| line.strip_prefix(REGION_PREFIX))
        .map(parse_region)
        .transpose()
        .map_err(|e| anyhow::anyhow!("overlay returned a bad region: {e}"))?;

    // A region was printed: use it even if the child crashed while exiting.
    if region.is_none() && !output.status.success() {
        anyhow::bail!("selection overlay failed ({})", output.status);
    }
    Ok(region)
}

/// Child-process side of `pick_region`.
fn run_selector_only() -> Result<()> {
    if let Some(r) = selector::run()? {
        println!("{REGION_PREFIX}{},{},{},{}", r.x, r.y, r.width, r.height);
    }
    // Leave right away: skipping the normal cleanup avoids racing with
    // libcosmic's leftover thread (see `pick_region`).
    use std::io::Write;
    std::io::stdout().flush()?;
    std::process::exit(0);
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.select_only {
        return run_selector_only();
    }

    // 1. Pick the region, in a child process (see `pick_region`).
    let region = match args.region {
        Some(region) => region,
        None => match pick_region()? {
            Some(region) => region,
            None => {
                println!("Cancelled.");
                return Ok(());
            }
        },
    };
    println!("Region: {region:?}");

    // 2. Record. `#[tokio::main]` is just a shortcut for these two lines.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(record(args, region))
}

async fn record(args: Args, region: Region) -> Result<()> {
    let output = match args.output {
        Some(path) => path,
        None => default_output()?,
    };

    // Ask the portal for screen access (async: goes through D-Bus).
    let stream = portal::open().await?;
    println!("Screen logical size: {:?}", stream.size);

    // Start the encoder. The channel is bounded: a few frames of buffer
    //    between capture and encoding.
    let (frame_tx, frame_rx) = mpsc::sync_channel(8);
    let encoder = gif::spawn(
        frame_rx,
        gif::Options {
            output: output.clone(),
            quality: args.quality,
            max_width: args.max_width,
        },
    );

    // Start the capture. The PipeWire loop is blocking: run it on a
    //    dedicated thread so it doesn't block tokio's async runtime.
    let (stop_tx, stop_rx) = pipewire::channel::channel::<()>();
    let options = capture::Options {
        region: Some(region),
        logical_size: stream.size,
        fps: args.fps.max(1),
        dmabuf: !args.no_dmabuf,
    };
    // `fd` is moved into the thread, but `stream` itself stays here so the
    // portal session lives until the end.
    let (node_id, fd) = (stream.node_id, stream.fd.try_clone()?);
    let mut capture =
        tokio::task::spawn_blocking(move || capture::run(node_id, fd, options, frame_tx, stop_rx));

    // Wait for Ctrl+C, the duration, or the capture ending by itself.
    let timeout = async {
        match args.duration {
            Some(secs) => tokio::time::sleep(Duration::from_secs_f64(secs)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("\nStopping..."),
        _ = timeout => println!("Duration reached, stopping..."),
        result = &mut capture => {
            // The capture ended without being asked to: report why.
            result??;
        }
    }
    // If the capture already ended, nobody listens anymore: ignore the error.
    let _ = stop_tx.send(());
    if !capture.is_finished() {
        capture.await??;
    }
    stream.close().await?;

    // The capture thread has dropped its sender: the encoder finishes.
    println!("Encoding GIF...");
    encoder
        .join()
        .map_err(|_| anyhow::anyhow!("encoder thread panicked"))?
        .context("GIF encoding failed")?;

    let size = std::fs::metadata(&output)?.len();
    println!(
        "Saved {} ({:.1} MB)",
        output.display(),
        size as f64 / 1_000_000.0
    );
    Ok(())
}
