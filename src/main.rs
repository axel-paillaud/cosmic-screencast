mod capture;
mod gif;
mod portal;

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
    /// Records the whole screen if omitted.
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

    /// Output file. Defaults to ~/Videos/screencast_<date>.gif.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

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

fn default_output() -> PathBuf {
    let dir = dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let now = jiff::Zoned::now().strftime("%Y-%m-%d_%H-%M-%S");
    dir.join(format!("screencast_{now}.gif"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let output = args.output.unwrap_or_else(default_output);

    // 1. Ask the portal for screen access (async: goes through D-Bus).
    let stream = portal::open().await?;
    println!("Screen logical size: {:?}", stream.size);

    // 2. Start the encoder. The channel is bounded: a few frames of buffer
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

    // 3. Start the capture. The PipeWire loop is blocking: run it on a
    //    dedicated thread so it doesn't block tokio's async runtime.
    let (stop_tx, stop_rx) = pipewire::channel::channel::<()>();
    let options = capture::Options {
        region: args.region,
        logical_size: stream.size,
        fps: args.fps.max(1),
    };
    // `fd` is moved into the thread, but `stream` itself stays here so the
    // portal session lives until the end.
    let (node_id, fd) = (stream.node_id, stream.fd.try_clone()?);
    let mut capture = tokio::task::spawn_blocking(move || {
        capture::run(node_id, fd, options, frame_tx, stop_rx)
    });

    // 4. Wait for Ctrl+C, the duration, or the capture ending by itself.
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

    // 5. The capture thread has dropped its sender: the encoder finishes.
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
