//! Receives screen frames through PipeWire.
//!
//! PipeWire is built around an event loop (`MainLoop`): we register callbacks,
//! then run the loop, which calls them when something happens (new format,
//! new frame...).

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;

/// Settings for this first test version.
pub struct Options {
    pub out_dir: PathBuf,
    /// Number of frames to save before stopping.
    pub frame_count: u32,
    /// Minimum interval between two saved frames.
    pub interval: Duration,
}

/// State shared between our callbacks.
struct State {
    format: VideoInfoRaw,
    options: Options,
    saved: u32,
    received: u32,
    last_saved: Option<Instant>,
}

/// Runs the capture. This function blocks until all frames are saved.
pub fn run(node_id: u32, fd: OwnedFd, options: Options) -> Result<()> {
    pw::init();
    std::fs::create_dir_all(&options.out_dir)?;

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    // Connect to the "private" PipeWire instance the portal opened for us.
    let core = context.connect_fd_rc(fd, None)?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "cosmic-screencast",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let state = State {
        format: Default::default(),
        options,
        saved: 0,
        received: 0,
        last_saved: None,
    };

    // `clone()` on a MainLoopRc doesn't copy the loop: it creates a second
    // reference to the same loop (like an Rc).
    let loop_for_callback = mainloop.clone();

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| {
            println!("État du flux : {old:?} -> {new:?}");
        })
        .param_changed(|_, state, id, param| {
            // We only care about the "Format" param (size, pixel format).
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if state.format.parse(param).is_ok() {
                let size = state.format.size();
                let fps = state.format.framerate();
                println!(
                    "Format négocié : {:?} {}x{} à {}/{} img/s max",
                    state.format.format(),
                    size.width,
                    size.height,
                    fps.num,
                    fps.denom
                );
            }
        })
        .process(move |stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            state.received += 1;

            // Skip frames that arrive too soon: this is what keeps
            // CPU usage low.
            let now = Instant::now();
            if let Some(last) = state.last_saved
                && now.duration_since(last) < state.options.interval
            {
                return;
            }

            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let stride = data.chunk().stride() as usize;
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            if size == 0 {
                // Empty frame: the compositor is just saying "nothing changed".
                return;
            }
            let Some(bytes) = data.data() else {
                eprintln!("Tampon non accessible en mémoire (DMA-BUF ?)");
                return;
            };
            let bytes = &bytes[offset..offset + size];

            let width = state.format.size().width;
            let height = state.format.size().height;
            match to_rgba(bytes, width, height, stride, state.format.format()) {
                Some(img) => {
                    let path = state
                        .options
                        .out_dir
                        .join(format!("frame_{:03}.png", state.saved));
                    if let Err(e) = img.save(&path) {
                        eprintln!("Erreur d'enregistrement : {e}");
                        return;
                    }
                    println!(
                        "Image {} enregistrée ({} reçues au total) : {}",
                        state.saved + 1,
                        state.received,
                        path.display()
                    );
                    state.saved += 1;
                    state.last_saved = Some(now);
                }
                None => eprintln!("Format de pixel non géré : {:?}", state.format.format()),
            }

            if state.saved >= state.options.frame_count {
                loop_for_callback.quit();
            }
        })
        .register()?;

    // Tell PipeWire which formats we can read. We only offer simple 32-bit
    // formats, in shared memory (no DMA-BUF for now).
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
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 10, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format_pod),
    )?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).expect("pod valide")];

    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    println!("Connecté au nœud PipeWire {node_id}, en attente d'images...");
    mainloop.run();
    Ok(())
}

/// Converts a raw buffer into an RGBA image, honoring the "stride": the real
/// length of a row in bytes, sometimes larger than width × 4.
fn to_rgba(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: VideoFormat,
) -> Option<image::RgbaImage> {
    let bgr = match format {
        VideoFormat::BGRx | VideoFormat::BGRA => true,
        VideoFormat::RGBx | VideoFormat::RGBA => false,
        _ => return None,
    };
    let stride = if stride == 0 { width as usize * 4 } else { stride };

    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        let row = bytes.get(y * stride..y * stride + width as usize * 4)?;
        for px in row.chunks_exact(4) {
            let (r, g, b) = if bgr { (px[2], px[1], px[0]) } else { (px[0], px[1], px[2]) };
            // "x" formats have no reliable alpha: force full opacity.
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    image::RgbaImage::from_raw(width, height, out)
}
