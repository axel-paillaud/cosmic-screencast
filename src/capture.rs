//! Receives screen frames through PipeWire.
//!
//! PipeWire is built around an event loop (`MainLoop`): we register callbacks,
//! then run the loop, which calls them when something happens (new format,
//! new frame...).
//!
//! # Pacing (why we hold buffers)
//!
//! The portal copies the screen into a buffer only when we give one back to
//! it, and it ignores any framerate we ask for. If we hand buffers back right
//! away, the compositor copies the whole screen (GPU → RAM) at refresh rate,
//! which makes the desktop and the mouse stutter. So we keep every buffer we
//! receive, and a timer gives one back every 1/fps second: the compositor
//! never copies more than `fps` frames per second.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::fd::{OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::Result;
use imgref::ImgVec;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::sys::pw_buffer;
use rgb::RGBA8;

use crate::dmabuf;
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
    /// Offer DMA-BUF (GPU memory) first. If false, shared memory only.
    pub dmabuf: bool,
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
    /// Whether we printed the buffer type yet.
    announced: bool,
    held: HeldBuffers,
}

/// Buffers we keep from the portal, oldest first (see "Pacing" above).
/// Raw pointers: they belong to the stream, we only borrow them until we
/// queue them back. Shared by the `process` callback and the timer.
type HeldBuffers = Rc<RefCell<VecDeque<*mut pw_buffer>>>;

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

    // `StreamRc` (not `StreamBox`): the timer below needs its own handle on
    // the stream.
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "cosmic-screencast",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    // Copy it now: `options` is moved into `state` just below.
    let state_fps = options.fps.max(1);
    let use_dmabuf = options.dmabuf;
    let stats = Rc::new(Stats::default());
    let held = HeldBuffers::default();
    let state = State {
        format: Default::default(),
        options,
        frames,
        stats: stats.clone(),
        last_kept: None,
        announced: false,
        held: held.clone(),
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
        .remove_buffer(|_, state, buffer| {
            // The stream is freeing this buffer (e.g. format change): forget
            // it, or the timer would queue a dangling pointer.
            state.held.borrow_mut().retain(|&held| held != buffer);
        })
        .process(|stream, state| {
            // SAFETY: the buffer is given back to the stream by the timer, or
            // after the loop ends. Never used after that.
            let buffer = unsafe { stream.dequeue_raw_buffer() };
            if buffer.is_null() {
                return;
            }
            // Keep it: the compositor can't copy a new frame into it until
            // the timer gives it back. This is what keeps the desktop smooth.
            state.held.borrow_mut().push_back(buffer);
            state.stats.received.set(state.stats.received.get() + 1);

            // The pacing above already limits the rate. This only absorbs
            // the burst at startup, when the portal fills all its buffers.
            let now = Instant::now();
            let interval = Duration::from_secs_f64(1.0 / state.options.fps as f64);
            if let Some(last) = state.last_kept
                && now.duration_since(last) < interval / 2
            {
                return;
            }

            // SAFETY: we hold the buffer (see above).
            let Some(data) = (unsafe { first_data(buffer) }) else {
                return;
            };
            let stride = data.chunk().stride() as usize;
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            if size == 0 {
                // Empty frame: the compositor is just saying "nothing changed".
                return;
            }

            // Two kinds of buffers, see `format_pods`.
            let is_dmabuf = data.type_() == spa::buffer::DataType::DmaBuf;
            if !state.announced {
                state.announced = true;
                println!(
                    "Buffers: {}",
                    if is_dmabuf {
                        "DMA-BUF (GPU memory)"
                    } else {
                        "shared memory (CPU)"
                    }
                );
            }
            // `mapping` must live while we read `bytes`: declared out here.
            let mapping;
            let bytes = if is_dmabuf {
                mapping = match dmabuf::Mapping::new(data.fd() as RawFd, offset + size) {
                    Ok(mapping) => mapping,
                    Err(e) => {
                        eprintln!("Cannot read the DMA-BUF: {e} (try --no-dmabuf)");
                        return;
                    }
                };
                mapping.bytes()
            } else {
                let Some(bytes) = data.data() else {
                    eprintln!("Buffer is not mapped in memory");
                    return;
                };
                bytes
            };
            let Some(bytes) = bytes.get(offset..offset + size) else {
                eprintln!("Buffer is smaller than announced");
                return;
            };

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

    // Tell PipeWire which formats we can read, in order of preference.
    let fps = state_fps;
    let pods = format_pods(fps, use_dmabuf)?;
    let mut params: Vec<&Pod> = pods
        .iter()
        .map(|bytes| Pod::from_bytes(bytes).expect("valid pod"))
        .collect();

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    // Every 1/fps second, give the oldest held buffer back to the portal,
    // allowing the compositor to copy one new frame.
    let timer_stream = stream.clone();
    let timer_held = held.clone();
    let timer = mainloop.loop_().add_timer(move |_expirations| {
        // Take it out first, so the `RefCell` is not borrowed while
        // PipeWire runs (it could call our callbacks).
        let next = timer_held.borrow_mut().pop_front();
        if let Some(buffer) = next {
            // SAFETY: dequeued from this stream, and no longer held.
            unsafe { timer_stream.queue_raw_buffer(buffer) };
        }
    });
    let tick = Duration::from_secs_f64(1.0 / fps as f64);
    timer.update_timer(Some(tick), Some(tick)).into_result()?;

    println!("Recording... press Ctrl+C to stop.");
    mainloop.run();

    // Give everything back before the stream is destroyed.
    let remaining: Vec<_> = held.borrow_mut().drain(..).collect();
    for buffer in remaining {
        // SAFETY: same as in the timer.
        unsafe { stream.queue_raw_buffer(buffer) };
    }

    println!(
        "Received {} frames from the compositor, dropped {} (encoder too slow).",
        stats.received.get(),
        stats.dropped.get()
    );
    Ok(())
}

/// The formats we accept, as serialized "pods" (PipeWire's binary format).
///
/// 1. DMA-BUF, LINEAR layout: the compositor copies the screen GPU → GPU
///    (cheap), we read only the selected rows (see `dmabuf.rs`).
/// 2. Shared memory: the compositor copies the whole screen to RAM, which
///    stalls it a bit. Fallback when DMA-BUF isn't possible (software
///    rendering, LINEAR not supported...). PipeWire picks the first match.
///
/// Note: COSMIC ignores our preferred framerate (see "Pacing" at the top).
fn format_pods(fps: u32, dmabuf: bool) -> Result<Vec<Vec<u8>>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};

    let base = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
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
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    let mut objects = Vec::new();
    if dmabuf {
        // Same format + a "modifier" (memory layout): LINEAR only.
        // MANDATORY: only DMA-BUF producers match this pod.
        // DONT_FIXATE: the producer (the portal) makes the final choice.
        let mut object = base.clone();
        object.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            // (`DONT_FIXATE` is behind a libspa feature: use the raw value.)
            flags: PropertyFlags::MANDATORY
                | PropertyFlags::from_bits_retain(spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE),
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: dmabuf::MODIFIER_LINEAR,
                    alternatives: vec![dmabuf::MODIFIER_LINEAR],
                },
            ))),
        });
        objects.push(object);
    }
    objects.push(base);

    objects
        .into_iter()
        .map(|object| {
            let bytes = spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &Value::Object(object),
            )?
            .0
            .into_inner();
            Ok(bytes)
        })
        .collect()
}

/// Returns the first data block (the pixels) of a raw PipeWire buffer.
///
/// # Safety
///
/// `buffer` must come from `dequeue_raw_buffer` and not be queued back yet.
unsafe fn first_data<'a>(buffer: *mut pw_buffer) -> Option<&'a mut spa::buffer::Data> {
    // SAFETY: guaranteed by the caller; `Data` is a transparent wrapper
    // around `spa_data`, so the pointer cast is valid.
    unsafe {
        let spa_buffer = (*buffer).buffer;
        if spa_buffer.is_null() || (*spa_buffer).n_datas == 0 {
            return None;
        }
        ((*spa_buffer).datas as *mut spa::buffer::Data).as_mut()
    }
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
    // Each row is first copied in one go: reading GPU memory (DMA-BUF) byte
    // by byte is very slow, a single big copy is much faster.
    let mut line = vec![0u8; w as usize * 4];
    for row in y..y + h {
        let start = row as usize * stride + x as usize * 4;
        line.copy_from_slice(bytes.get(start..start + w as usize * 4)?);
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
