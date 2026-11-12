mod capture;
mod dmabuf;
mod formats;
mod gif;
mod instance;
mod notify;
mod overlay;
mod pixels;
mod portal;
mod region;
mod selection;
mod selector;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{ArgAction, Parser};

use instance::{Instance, Role};
use overlay::Overlay;
use region::Region;

/// Record a screen region to an animated GIF.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Region to record, in screen coordinates: X,Y,WIDTH,HEIGHT.
    /// Shows a selection overlay if omitted.
    #[arg(short, long, value_parser = Region::parse)]
    region: Option<Region>,

    /// Frames per second.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=50))]
    fps: u32,

    /// Stop after this many seconds (otherwise, run the command again or
    /// press Ctrl+C).
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

    /// Send a notification when the GIF is saved (or the recording fails).
    /// Same syntax as cosmic-screenshot: `--notify=false` to disable.
    #[arg(long,
        default_missing_value("true"),
        default_value("true"),
        num_args(0..=1),
        require_equals(true),
        action = ArgAction::Set)]
    notify: bool,

    /// Internal: only show the selection overlay, print the region on stdout
    /// and stay as a recording indicator. See `overlay.rs`.
    #[arg(long, hide = true)]
    select_only: bool,
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

fn main() -> Result<()> {
    let args = Args::parse();

    if args.select_only {
        return overlay::run_child();
    }

    // `#[tokio::main]` is just a shortcut for these two lines. (The overlay
    // runs libcosmic, which has its own runtime: it stays in the child.)
    let runtime = tokio::runtime::Runtime::new()?;
    let notify = args.notify;
    let result = runtime.block_on(run(args));
    // Launched from a shortcut, nobody sees the terminal: report errors.
    if let Err(error) = &result
        && notify
    {
        runtime.block_on(notify::failed(error));
    }
    result
}

async fn run(args: Args) -> Result<()> {
    // 1. Already recording? Then this launch means "stop" (see `instance.rs`).
    let instance = match instance::claim().await? {
        Role::Recorder(instance) => instance,
        Role::StopSent => {
            println!("Stopped the recording in progress.");
            return Ok(());
        }
    };

    // 2. Pick the region with the overlay (see `overlay.rs`). It stays on
    //    screen during the recording. Pressing the shortcut again during the
    //    selection cancels it.
    let selected = match args.region {
        Some(region) => Some((Some(region), None)),
        None => tokio::select! {
            selected = overlay::select() => {
                selected?.map(|(region, overlay)| (region, Some(overlay)))
            }
            _ = instance.stop_requested() => None,
        },
    };
    let Some((region, overlay)) = selected else {
        println!("Cancelled.");
        return Ok(());
    };
    match region {
        Some(region) => println!("Region: {region:?}"),
        None => println!("Region: whole screen"),
    }

    // 3. Record.
    let notify = args.notify;
    let output = record(args, region, overlay, &instance).await?;
    if notify {
        notify::saved(&output).await;
    }
    Ok(())
}

/// Records `region` (`None` = the whole screen) until stopped, and returns
/// the path of the saved GIF. `overlay` (if any) is closed as soon as the
/// capture stops.
async fn record(
    args: Args,
    region: Option<Region>,
    overlay: Option<Overlay>,
    instance: &Instance,
) -> Result<PathBuf> {
    let output = match args.output {
        Some(path) => path,
        None => default_output()?,
    };

    // Start the encoder. The channel is bounded: a few frames of buffer
    // between capture and encoding.
    let (frame_tx, frame_rx) = mpsc::sync_channel(8);
    let encoder = gif::spawn(
        frame_rx,
        gif::Options {
            output: output.clone(),
            quality: args.quality,
            max_width: args.max_width,
        },
    );

    // What stops the recording: Ctrl+C, the shortcut (another launch, see
    // `instance.rs`) or the duration. Created once and
    // "pinned" so the loop below can wait on it several times.
    let stop_requested = async {
        let timeout = async {
            match args.duration {
                Some(secs) => tokio::time::sleep(Duration::from_secs_f64(secs)).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => println!("\nStopping..."),
            _ = instance.stop_requested() => println!("Stop requested, stopping..."),
            _ = timeout => println!("Duration reached, stopping..."),
        }
    };
    tokio::pin!(stop_requested);

    // The capture itself; in a block so that, even on error, we stop the
    // encoder properly below (it deletes its unfinished file).
    let captured: Result<()> = async {
        // Usually runs once. If GPU memory can't be read, run again with
        // shared memory: this needs a new portal session (see `capture.rs`).
        let mut dmabuf = !args.no_dmabuf;
        loop {
            // Ask the portal for screen access (async: goes through D-Bus).
            let stream = portal::open().await?;
            println!("Screen logical size: {:?}", stream.size);

            // Start the capture. The PipeWire loop is blocking: run it on a
            // dedicated thread so it doesn't block tokio's async runtime.
            let (stop_tx, stop_rx) = pipewire::channel::channel::<()>();
            let options = capture::Options {
                region,
                logical_size: stream.size,
                fps: args.fps,
                dmabuf,
            };
            // `fd` is moved into the thread, but `stream` itself stays here so
            // the portal session lives until the end.
            let (node_id, fd) = (stream.node_id, stream.fd.try_clone()?);
            let frames = frame_tx.clone();
            let mut capture = tokio::task::spawn_blocking(move || {
                capture::run(node_id, fd, options, frames, stop_rx)
            });

            // Wait for a stop request, or the capture ending by itself.
            let (outcome, stop) = tokio::select! {
                _ = &mut stop_requested => {
                    // If the capture already ended, nobody listens: ignore errors.
                    let _ = stop_tx.send(());
                    (capture.await??, true)
                }
                result = &mut capture => (result??, false),
            };
            stream.close().await?;

            match outcome {
                capture::Outcome::DmabufUnreadable if !stop => {
                    println!("Starting again with shared memory...");
                    dmabuf = false;
                }
                _ => break,
            }
        }
        Ok(())
    }
    .await;

    // The capture is over: remove the overlay right away, before encoding.
    drop(overlay);

    // Every capture thread has dropped its sender; drop ours too, so the
    // encoder sees the end of the frames.
    drop(frame_tx);
    if captured.is_ok() {
        println!("Encoding GIF...");
    }
    let encoded = encoder
        .join()
        .map_err(|_| anyhow::anyhow!("encoder thread panicked"))?;
    captured?;
    encoded.context("GIF encoding failed")?;

    let size = std::fs::metadata(&output)?.len();
    println!(
        "Saved {} ({:.1} MB)",
        output.display(),
        size as f64 / 1_000_000.0
    );
    Ok(output)
}
