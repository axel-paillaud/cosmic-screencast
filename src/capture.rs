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
//! away, the compositor copies the whole screen at refresh rate, which makes
//! the desktop and the mouse stutter. So we keep every buffer we receive, and
//! a timer gives one back every 1/fps second: the compositor never copies
//! more than `fps` frames per second.
//!
//! # Buffer types
//!
//! We prefer DMA-BUF (GPU memory, see `formats.rs` and `dmabuf.rs`). If such a
//! buffer can't be read, the capture stops with `Outcome::DmabufUnreadable`
//! and the caller starts again with shared memory. (Renegotiating in place
//! doesn't work: the COSMIC portal can't leave DMA-BUF within a session.)

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use anyhow::Result;
use imgref::ImgVec;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::Stream;
use pw::sys::pw_buffer;
use rgb::RGBA8;
use spa::buffer::DataType;
use spa::param::video::VideoInfoRaw;
use spa::pod::Pod;

use crate::pixels::{self, PixelOrder};
use crate::region::Region;
use crate::{dmabuf, formats};

/// A cropped frame, ready to be encoded.
pub struct Frame {
    pub image: ImgVec<RGBA8>,
    /// When the frame was received: used to compute GIF frame durations.
    pub at: Instant,
}

pub struct Options {
    /// Area to keep, in logical pixels. `None` means the whole screen.
    pub region: Option<Region>,
    /// Logical size of the screen as reported by the portal. Frames come in
    /// physical pixels, so we need it to convert `region` (HiDPI scaling).
    pub logical_size: Option<(i32, i32)>,
    /// Frames per second we want to keep. Must be at least 1.
    pub fps: u32,
    /// Offer DMA-BUF (GPU memory) first. If false, shared memory only.
    pub dmabuf: bool,
}

/// How the capture ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Stopped through the `stop` channel.
    Stopped,
    /// GPU memory couldn't be read: start again with `dmabuf: false`.
    DmabufUnreadable,
}

/// What we want to read after the loop ends. `Cell` allows mutation through
/// a shared reference, and `Rc` lets two owners share it (single thread).
#[derive(Default)]
struct Report {
    received: Cell<u32>,
    dropped: Cell<u32>,
    dmabuf_unreadable: Cell<bool>,
}

/// Buffers we keep from the portal, oldest first (see "Pacing" above).
/// Raw pointers: they belong to the stream, we only borrow them until we
/// queue them back. Shared by the `process` callback and the timer.
type HeldBuffers = Rc<RefCell<VecDeque<*mut pw_buffer>>>;

/// State owned by our stream callbacks.
struct State {
    /// Negotiated format (pixel format, frame size).
    format: VideoInfoRaw,
    options: Options,
    frames: SyncSender<Frame>,
    report: Rc<Report>,
    held: HeldBuffers,
    /// To stop the loop from a callback.
    mainloop: pw::main_loop::MainLoopRc,
    /// Frames closer than this to the previous one are skipped.
    min_gap: Duration,
    last_kept: Option<Instant>,
    /// Whether we printed the format of the current negotiation.
    announced: bool,
}

/// Why a frame couldn't be read.
enum ReadError {
    /// The DMA-BUF could not be mapped: switch to shared memory.
    Dmabuf(io::Error),
    /// Anything else: skip this frame.
    Invalid(&'static str),
}

/// Runs the capture. This function blocks until a message arrives on `stop`.
/// Every kept frame is sent to `frames`; the channel closes when we return.
pub fn run(
    node_id: u32,
    fd: OwnedFd,
    options: Options,
    frames: SyncSender<Frame>,
    stop: pw::channel::Receiver<()>,
) -> Result<Outcome> {
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

    let tick = Duration::from_secs_f64(1.0 / options.fps as f64);
    let pods = formats::format_pods(options.fps, options.dmabuf)?;
    let report = Rc::new(Report::default());
    let held = HeldBuffers::default();
    let state = State {
        format: Default::default(),
        options,
        frames,
        report: report.clone(),
        held: held.clone(),
        mainloop: mainloop.clone(),
        // The timer already limits the rate: this only absorbs the burst at
        // startup, when the portal fills all its buffers at once.
        min_gap: tick / 2,
        last_kept: None,
        announced: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, _old, new| {
            // Without this, a failed format negotiation would be silent.
            if let pw::stream::StreamState::Error(e) = new {
                eprintln!("PipeWire stream error: {e}");
            }
        })
        .param_changed(on_param_changed)
        .remove_buffer(|_, state, buffer| {
            // The stream is freeing this buffer (e.g. format change): forget
            // it, or the timer would queue a dangling pointer.
            state.held.borrow_mut().retain(|&held| held != buffer);
        })
        .process(on_process)
        .register()?;

    // Tell PipeWire which formats we can read, in order of preference.
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
        report.received.get(),
        report.dropped.get()
    );
    Ok(if report.dmabuf_unreadable.get() {
        Outcome::DmabufUnreadable
    } else {
        Outcome::Stopped
    })
}

/// Called when a stream parameter changes. We only care about the "Format"
/// param: pixel format and frame size.
fn on_param_changed(_stream: &Stream, state: &mut State, id: u32, param: Option<&Pod>) {
    let Some(param) = param else { return };
    if id == spa::param::ParamType::Format.as_raw() && state.format.parse(param).is_ok() {
        // New negotiation: print the result again with the next frame.
        state.announced = false;
    }
}

/// Called by PipeWire when a new frame is available.
fn on_process(stream: &Stream, state: &mut State) {
    // SAFETY: the buffer is given back to the stream by the timer, or after
    // the loop ends. Never used after that.
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if buffer.is_null() {
        return;
    }
    // Keep it: the compositor can't copy a new frame into it until the
    // timer gives it back. This is what keeps the desktop smooth.
    state.held.borrow_mut().push_back(buffer);
    state.report.received.set(state.report.received.get() + 1);

    let now = Instant::now();
    if let Some(last) = state.last_kept
        && now.duration_since(last) < state.min_gap
    {
        return;
    }

    // SAFETY: we hold the buffer (see above).
    let image = match unsafe { read_frame(buffer, state) } {
        Ok(Some(image)) => image,
        Ok(None) => return,
        Err(ReadError::Invalid(reason)) => {
            eprintln!("Skipped a frame: {reason}");
            return;
        }
        Err(ReadError::Dmabuf(e)) => {
            eprintln!("Cannot read GPU memory ({e}).");
            state.report.dmabuf_unreadable.set(true);
            state.mainloop.quit();
            return;
        }
    };

    // `try_send` never blocks: if the encoder is late, we drop the frame
    // instead of stalling PipeWire (and the compositor).
    match state.frames.try_send(Frame { image, at: now }) {
        Ok(()) => state.last_kept = Some(now),
        Err(TrySendError::Full(_)) => {
            state.report.dropped.set(state.report.dropped.get() + 1);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Reads the pixels of `buffer`, cropped to the region.
/// `Ok(None)` means there is nothing to read (empty frame).
///
/// # Safety
///
/// `buffer` must come from `dequeue_raw_buffer` and not be queued back yet.
unsafe fn read_frame(
    buffer: *mut pw_buffer,
    state: &mut State,
) -> Result<Option<ImgVec<RGBA8>>, ReadError> {
    // SAFETY: guaranteed by the caller.
    let Some(data) = (unsafe { first_data(buffer) }) else {
        return Err(ReadError::Invalid("buffer has no data"));
    };
    let stride = data.chunk().stride() as usize;
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    if size == 0 {
        // Empty frame: the compositor is just saying "nothing changed".
        return Ok(None);
    }

    let is_dmabuf = data.type_() == DataType::DmaBuf;
    let frame_size = (state.format.size().width, state.format.size().height);
    if !state.announced {
        state.announced = true;
        println!(
            "Receiving {:?} {}x{} frames, in {}",
            state.format.format(),
            frame_size.0,
            frame_size.1,
            if is_dmabuf {
                "GPU memory (DMA-BUF)"
            } else {
                "shared memory"
            }
        );
    }

    let order = PixelOrder::from_format(state.format.format())
        .ok_or(ReadError::Invalid("unsupported pixel format"))?;
    let rect = match state.options.region {
        Some(region) => region.to_physical(state.options.logical_size, frame_size),
        None => Some(Region {
            x: 0,
            y: 0,
            width: frame_size.0,
            height: frame_size.1,
        }),
    }
    .ok_or(ReadError::Invalid("empty frame size"))?;
    // 0 means "rows are packed": width × 4 bytes.
    let stride = if stride == 0 {
        frame_size.0 as usize * 4
    } else {
        stride
    };

    // `mapping` must outlive `bytes`, so it is declared out here.
    let mapping;
    let bytes = if is_dmabuf {
        mapping =
            dmabuf::Mapping::new(data.fd() as RawFd, offset + size).map_err(ReadError::Dmabuf)?;
        mapping.bytes()
    } else {
        data.data()
            .ok_or(ReadError::Invalid("buffer is not mapped in memory"))?
    };
    let bytes = bytes
        .get(offset..offset + size)
        .ok_or(ReadError::Invalid("buffer is smaller than announced"))?;

    pixels::crop_to_rgba(bytes, stride, order, rect)
        .map(Some)
        .ok_or(ReadError::Invalid("region is outside the buffer"))
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
