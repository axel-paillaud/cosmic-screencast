//! Encodes frames into a GIF with gifski, while the capture is running.
//!
//! gifski is split in two halves:
//! - the `Collector`, which receives frames,
//! - the `Writer`, which compresses them and writes the file (on its own thread).

use std::fs::File;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::capture::Frame;

pub struct Options {
    pub output: PathBuf,
    /// 1-100. Lower = smaller file.
    pub quality: u8,
    /// Downscale frames wider than this (keeps the aspect ratio).
    pub max_width: Option<u32>,
}

/// Starts the encoder thread. It consumes frames until the channel closes
/// (i.e. the capture stopped), then finishes writing the file.
pub fn spawn(frames: Receiver<Frame>, options: Options) -> JoinHandle<Result<()>> {
    thread::spawn(move || encode(frames, options))
}

fn encode(frames: Receiver<Frame>, options: Options) -> Result<()> {
    let (collector, writer) = gifski::new(gifski::Settings {
        width: options.max_width,
        height: None,
        quality: options.quality,
        fast: false,
        repeat: gifski::Repeat::Infinite,
    })?;

    let file = File::create(&options.output)
        .with_context(|| format!("cannot create {}", options.output.display()))?;
    // The writer blocks until the collector is dropped: give it its own thread.
    let writer_thread =
        thread::spawn(move || writer.write(file, &mut gifski::progress::NoProgress {}));

    let mut start: Option<Instant> = None;
    let mut last = None;
    let mut count = 0;

    // `for ... in receiver` loops until every sender is dropped.
    for frame in frames {
        let start = *start.get_or_insert(frame.at);
        let pts = frame.at.duration_since(start).as_secs_f64();
        // Keep a copy of the last frame, see below.
        last = Some((frame.image.clone(), pts));
        collector.add_frame_rgba(count, frame.image, pts)?;
        count += 1;
    }

    // If the screen didn't change at the end, the compositor sent nothing:
    // repeat the last frame at stop time so the final state stays visible.
    if let (Some(start), Some((image, pts))) = (start, last) {
        let end = start.elapsed().as_secs_f64();
        if end > pts + 0.05 {
            collector.add_frame_rgba(count, image, end)?;
            count += 1;
        }
    }

    // Dropping the collector tells the writer there are no more frames.
    drop(collector);

    if count == 0 {
        // The writer would fail anyway; give a clearer message.
        let _ = writer_thread.join();
        let _ = std::fs::remove_file(&options.output);
        bail!("no frame was captured");
    }

    writer_thread
        .join()
        .map_err(|_| anyhow::anyhow!("encoder thread panicked"))??;
    println!("Encoded {count} frames.");
    Ok(())
}
