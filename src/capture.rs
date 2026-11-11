//! Receives screen frames through PipeWire.
//!
//! PipeWire is built around an event loop (`MainLoop`): we register callbacks,
//! then run the loop, which calls them when something happens (new format,
//! new frame...).

use std::cell::Cell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::Result;
use imgref::ImgVec;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use rgb::RGBA8;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;

/// A rectangle on screen, in logical (screen) coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A cropped frame, ready to be encoded.
pub struct Frame {
    pub image: ImgVec<RGBA8>,
    /// When the frame was received: used to compute GIF frame durations.
    pub at: Instant,
}

pub struct Options {
    /// Area to keep. `None` means the whole screen.
    pub region: Option<Region>,
    /// Logical size of the screen as reported by the portal. Frames come in
    /// physical pixels, so we need it to convert `region` (HiDPI scaling).
    pub logical_size: Option<(i32, i32)>,
    /// Frames per second we want to keep.
    pub fps: u32,
}

/// Counters we want to read after the loop ends. `Cell` allows mutation
/// through a shared reference, and `Rc` lets two owners share it (single thread).
#[derive(Default)]
struct Stats {
    received: Cell<u32>,
    dropped: Cell<u32>,
}

/// State owned by our callbacks.
struct State {
    format: VideoInfoRaw,
    options: Options,
    frames: SyncSender<Frame>,
    stats: Rc<Stats>,
    last_kept: Option<Instant>,
}

/// Runs the capture. This function blocks until a message arrives on `stop`.
/// Every kept frame is sent to `frames`; the channel closes when we return.
pub fn run(
    node_id: u32,
    fd: OwnedFd,
    options: Options,
    frames: SyncSender<Frame>,
    stop: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    // Connect to the "private" PipeWire instance the portal opened for us.
    let core = context.connect_fd_rc(fd, None)?;

    // When the main thread sends a message on `stop`, quit the loop.
    // `clone()` on a MainLoopRc doesn't copy the loop: it creates a second
    // reference to the same loop (like an Rc).
    let loop_for_stop = mainloop.clone();
    let _stop = stop.attach(mainloop.loop_(), move |()| loop_for_stop.quit());

    let stream = pw::stream::StreamBox::new(
        &core,
        "cosmic-screencast",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    // Copy it now: `options` is moved into `state` just below.
    let state_fps = options.fps.max(1);
    let stats = Rc::new(Stats::default());
    let state = State {
        format: Default::default(),
        options,
        frames,
        stats: stats.clone(),
        last_kept: None,
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, _old, new| {
            // Without this, a failed format negotiation would be silent.
            if let pw::stream::StreamState::Error(e) = new {
                eprintln!("PipeWire stream error: {e}");
            }
        })
        .param_changed(|_, state, id, param| {
            // We only care about the "Format" param (size, pixel format).
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if state.format.parse(param).is_ok() {
                let size = state.format.size();
                println!(
                    "Negotiated format: {:?} {}x{}",
                    state.format.format(),
                    size.width,
                    size.height
                );
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            state.stats.received.set(state.stats.received.get() + 1);

            // Skip frames that arrive too soon: this is what keeps
            // CPU usage low.
            let now = Instant::now();
            let interval = Duration::from_secs_f64(1.0 / state.options.fps as f64);
            if let Some(last) = state.last_kept
                && now.duration_since(last) < interval
            {
                return;
            }

            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let stride = data.chunk().stride() as usize;
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            if size == 0 {
                // Empty frame: the compositor is just saying "nothing changed".
                return;
            }
            let Some(bytes) = data.data() else {
                eprintln!("Buffer is not mapped in memory (DMA-BUF?)");
                return;
            };
            let bytes = &bytes[offset..offset + size];

            let Some(image) = crop_to_rgba(bytes, stride, &state.format, &state.options) else {
                eprintln!("Unsupported pixel format: {:?}", state.format.format());
                return;
            };

            // `try_send` never blocks: if the encoder is late, we drop the
            // frame instead of stalling PipeWire (and the compositor).
            match state.frames.try_send(Frame { image, at: now }) {
                Ok(()) => state.last_kept = Some(now),
                Err(TrySendError::Full(_)) => {
                    state.stats.dropped.set(state.stats.dropped.get() + 1);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        })
        .register()?;

    // Tell PipeWire which formats we can read. We only offer simple 32-bit
    // formats, in shared memory (no DMA-BUF for now).
    // Note: COSMIC ignores our preferred framerate and sends frames at screen
    // refresh rate when the screen changes; we skip the extra ones ourselves.
    let fps = state_fps;
    let format_pod = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: fps, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format_pod),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).expect("valid pod")];

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    println!("Recording... press Ctrl+C to stop.");
    mainloop.run();

    println!(
        "Received {} frames from the compositor, dropped {} (encoder too slow).",
        stats.received.get(),
        stats.dropped.get()
    );
    Ok(())
}

/// Crops the raw buffer to the requested region and converts it to RGBA.
///
/// The buffer has a "stride": the real length of a row in bytes, sometimes
/// larger than width × 4.
fn crop_to_rgba(
    bytes: &[u8],
    stride: usize,
    format: &VideoInfoRaw,
    options: &Options,
) -> Option<ImgVec<RGBA8>> {
    let bgr = match format.format() {
        VideoFormat::BGRx | VideoFormat::BGRA => true,
        VideoFormat::RGBx | VideoFormat::RGBA => false,
        _ => return None,
    };
    let frame_w = format.size().width;
    let frame_h = format.size().height;
    let stride = if stride == 0 {
        frame_w as usize * 4
    } else {
        stride
    };

    // Convert the logical region to physical pixels, then clamp it to the frame.
    let (x, y, w, h) = match options.region {
        None => (0, 0, frame_w, frame_h),
        Some(r) => {
            let scale = match options.logical_size {
                Some((lw, _)) if lw > 0 => frame_w as f64 / lw as f64,
                _ => 1.0,
            };
            let x = ((r.x as f64 * scale) as u32).min(frame_w - 1);
            let y = ((r.y as f64 * scale) as u32).min(frame_h - 1);
            let w = ((r.width as f64 * scale) as u32).clamp(1, frame_w - x);
            let h = ((r.height as f64 * scale) as u32).clamp(1, frame_h - y);
            (x, y, w, h)
        }
    };

    let mut pixels = Vec::with_capacity((w * h) as usize);
    for row in y..y + h {
        let start = row as usize * stride + x as usize * 4;
        let line = bytes.get(start..start + w as usize * 4)?;
        for px in line.chunks_exact(4) {
            let (r, g, b) = if bgr {
                (px[2], px[1], px[0])
            } else {
                (px[0], px[1], px[2])
            };
            // "x" formats have no reliable alpha: force full opacity.
            pixels.push(RGBA8::new(r, g, b, 255));
        }
    }
    Some(ImgVec::new(pixels, w as usize, h as usize))
}
