//! Reader-negotiated NV12; unknown formats and legacy readers retain RGB.
use crate::shm::ring_buffer::ShmRingBufferWriter;
use gstreamer::glib::translate::IntoGlib;
use gstreamer::prelude::*;
use gstreamer_video::{VideoChromaSite, VideoColorMatrix, VideoColorRange, VideoFormat, VideoInfo};
use std::sync::{Arc, Mutex};

pub(crate) fn descriptor(info: &VideoInfo) -> Option<u32> {
    if info.width() == 0
        || info.height() == 0
        || info.width() > 4096
        || info.height() > 4096
        || !matches!(info.format(), VideoFormat::Nv12 | VideoFormat::I420)
        || matches!(
            info.colorimetry().transfer().into_glib(),
            gstreamer_video::ffi::GST_VIDEO_TRANSFER_SMPTE2084
                | gstreamer_video::ffi::GST_VIDEO_TRANSFER_ARIB_STD_B67
        )
        || info.is_interlaced()
        || info.chroma_site().contains(VideoChromaSite::ALT_LINE)
    {
        return None;
    }
    let mut value = match info.colorimetry().matrix() {
        VideoColorMatrix::Bt601 => 1,
        VideoColorMatrix::Bt709 => 2,
        VideoColorMatrix::Bt2020 => 3,
        _ => return None,
    };
    match info.colorimetry().range() {
        VideoColorRange::Range0_255 => value |= 16,
        VideoColorRange::Range16_235 => (),
        _ => return None,
    }
    if info.chroma_site().contains(VideoChromaSite::H_COSITED) {
        value |= 32;
    }
    if info.chroma_site().contains(VideoChromaSite::V_COSITED) {
        value |= 64;
    }
    Some(value)
}

pub(super) fn install(appsink: &gstreamer_app::AppSink, writer: Arc<Mutex<ShmRingBufferWriter>>) {
    let mut upstream = appsink.static_pad("sink").and_then(|pad| pad.peer());
    let mut filter = None;
    while let Some(pad) = upstream {
        let Some(element) = pad.parent_element() else {
            return;
        };
        match element
            .factory()
            .map(|factory| factory.name().to_string())
            .as_deref()
        {
            Some("capsfilter") if filter.is_none() => filter = Some(element.clone()),
            Some("queue" | "tee" | "identity" | "capsfilter") => (),
            Some("videoconvert") => {
                let Some(filter) = filter else { return };
                let Some(input) = element.static_pad("sink") else {
                    return;
                };
                let active = Mutex::new(false);
                input.add_probe(gstreamer::PadProbeType::BUFFER, move |pad, _| {
                    let requested = writer.lock().map(|mut w| w.accepts_nv12()).unwrap_or(false);
                    let supported = requested
                        && pad
                            .current_caps()
                            .and_then(|caps| VideoInfo::from_caps(&caps).ok())
                            .and_then(|info| descriptor(&info))
                            .is_some();
                    if let Ok(mut active) = active.lock() {
                        if *active != supported {
                            let caps = gstreamer::Caps::builder("video/x-raw")
                                .field("format", if supported { "NV12" } else { "RGB" })
                                .build();
                            filter.set_property("caps", &caps);
                            *active = supported;
                        }
                    }
                    gstreamer::PadProbeReturn::Ok
                });
                return;
            }
            _ => return,
        }
        upstream = element.static_pad("sink").and_then(|pad| pad.peer());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_colour_or_layout_retains_rgb() {
        gstreamer::init().unwrap();
        for extra in [
            "format=RGB,colorimetry=sRGB",
            "format=NV12,colorimetry=bt2100-pq",
            "format=NV12,colorimetry=bt2100-hlg",
            "format=NV12,colorimetry=bt709,interlace-mode=interleaved",
            "format=NV12,colorimetry=bt709,chroma-site=dv",
        ] {
            let caps: gstreamer::Caps =
                format!("video/x-raw,width=854,height=480,framerate=15/1,{extra}")
                    .parse()
                    .unwrap();
            let info = VideoInfo::from_caps(&caps).unwrap();
            assert_eq!(descriptor(&info), None, "{extra}");
        }
    }

    #[test]
    fn switches_rgb_nv12_rgb_without_restarting_live_pipeline() {
        gstreamer::init().unwrap();
        for (format, shared) in [("NV12", false), ("I420", false), ("NV12", true)] {
            let tail = if shared {
                "tee name=publishtee ! queue ! appsink name=frames sync=false publishtee. ! queue ! videoconvert ! video/x-raw,format=I420 ! fakesink sync=false"
            } else {
                "appsink name=frames sync=false"
            };
            let pipeline = gstreamer::parse::launch(&format!("videotestsrc is-live=true ! video/x-raw,format={format},width=854,height=480,framerate=30/1,colorimetry=bt709,chroma-site=mpeg2 ! videoconvert ! video/x-raw,format=RGB ! {tail}"))
                .unwrap().downcast::<gstreamer::Pipeline>().unwrap();
            let sink = pipeline
                .by_name("frames")
                .unwrap()
                .downcast::<gstreamer_app::AppSink>()
                .unwrap();
            let writer = Arc::new(Mutex::new(
                ShmRingBufferWriter::create(&format!("test-format-{}", uuid::Uuid::new_v4()))
                    .unwrap(),
            ));
            install(&sink, writer.clone());
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            for (enabled, expected) in [
                (false, VideoFormat::Rgb),
                (true, VideoFormat::Nv12),
                (false, VideoFormat::Rgb),
                (true, VideoFormat::Nv12),
            ] {
                writer.lock().unwrap().request_nv12(enabled);
                let mut matched = false;
                for _ in 0..10 {
                    let sample = sink
                        .try_pull_sample(gstreamer::ClockTime::from_seconds(2))
                        .expect("live stream stalled");
                    let info = VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
                    if info.format() == expected {
                        matched = true;
                        break;
                    }
                }
                assert!(matched, "failed to switch from {format} to {expected:?}");
            }
            pipeline.set_state(gstreamer::State::Null).unwrap();
        }
    }
}
