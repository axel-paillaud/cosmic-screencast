//! The video formats we offer to PipeWire during negotiation.

use anyhow::Result;
use pipewire::spa;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::VideoFormat;
use spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};

use crate::dmabuf;

/// The formats we accept, in order of preference, as serialized "pods"
/// (PipeWire's binary format). Turn them into `&Pod` with `Pod::from_bytes`.
///
/// 1. DMA-BUF, LINEAR layout (only if `dmabuf` is true): the compositor copies
///    the screen GPU → GPU (cheap), we read only the selected rows.
/// 2. Shared memory: the compositor copies the whole screen to RAM, which
///    stalls it a bit. Fallback when DMA-BUF isn't possible (software
///    rendering, LINEAR not supported...). PipeWire picks the first match.
///
/// The portal ignores our preferred framerate (see "Pacing" in `capture.rs`),
/// and a narrower framerate range makes negotiation fail: keep it wide.
pub fn format_pods(fps: u32, dmabuf: bool) -> Result<Vec<Vec<u8>>> {
    let shm = spa::pod::object!(
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
        let mut object = shm.clone();
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
    objects.push(shm);

    objects.into_iter().map(serialize).collect()
}

/// Turns a pod object into bytes.
fn serialize(object: spa::pod::Object) -> Result<Vec<u8>> {
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )?
    .0
    .into_inner();
    Ok(bytes)
}
