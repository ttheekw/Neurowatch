//! One-frame camera samples for viewer-demand cloud thumbnails.
//!
//! The GStreamer callback does one atomic read while idle. It retains a sample
//! only after an active page has requested that camera; colour/JPEG conversion happens
//! on a blocking worker, outside the capture callback.
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use gstreamer_video::prelude::*;
use tokio::sync::oneshot;

struct RawRgbFrame {
    width: u32,
    height: u32,
    stride: usize,
    bytes: Vec<u8>,
}

enum PreviewFrame {
    #[cfg(test)]
    Rgb(RawRgbFrame),
    Sample(gstreamer::Sample),
}

#[derive(Default)]
pub struct PreviewFrameHub {
    pending: Mutex<HashMap<String, oneshot::Sender<PreviewFrame>>>,
    waiting: AtomicUsize,
}

impl PreviewFrameHub {
    #[cfg(test)]
    fn submit(&self, camera_id: &str, width: u32, height: u32, stride: usize, bytes: &[u8]) {
        if self.waiting.load(Ordering::Relaxed) == 0 {
            return;
        }
        let sender = self
            .pending
            .lock()
            .expect("preview hub poisoned")
            .remove(camera_id);
        if let Some(sender) = sender {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            let _ = sender.send(PreviewFrame::Rgb(RawRgbFrame {
                width,
                height,
                stride,
                bytes: bytes.to_vec(),
            }));
        }
    }

    /// Whether a thumbnail request is waiting for this camera's next frame.
    pub fn wants(&self, camera_id: &str) -> bool {
        self.pending
            .lock()
            .map(|pending| pending.contains_key(camera_id))
            .unwrap_or(false)
    }

    pub fn submit_sample(&self, camera_id: &str, sample: &gstreamer::Sample) {
        if self.waiting.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Some(sender) = self
            .pending
            .lock()
            .expect("preview hub poisoned")
            .remove(camera_id)
        {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            let _ = sender.send(PreviewFrame::Sample(sample.clone()));
        }
    }

    pub async fn capture(&self, camera_id: &str, timeout: Duration) -> Option<String> {
        let (sender, receiver) = oneshot::channel();
        let replaced = self
            .pending
            .lock()
            .expect("preview hub poisoned")
            .insert(camera_id.to_owned(), sender);
        if replaced.is_none() {
            self.waiting.fetch_add(1, Ordering::Relaxed);
        }
        let frame = tokio::time::timeout(timeout, receiver)
            .await
            .ok()
            .and_then(Result::ok);
        if self
            .pending
            .lock()
            .expect("preview hub poisoned")
            .remove(camera_id)
            .is_some()
        {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
        }
        let frame = frame?;
        tokio::task::spawn_blocking(move || encode(frame))
            .await
            .ok()
            .flatten()
    }
}

/// Keep only demanded cameras that are delivering frames right now, so a
/// camera that is down costs no capture wait. The browser demands every
/// visible camera; the node's live state decides which can answer.
pub fn streaming_only(
    demand: Vec<String>,
    states: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    demand
        .into_iter()
        .filter(|id| {
            matches!(
                states.get(id).map(String::as_str),
                Some("streaming" | "recording_ready")
            )
        })
        .collect()
}

/// A decoded picture as a JPEG thumbnail (base64), like a captured frame.
pub(crate) fn encode_sample(sample: gstreamer::Sample) -> Option<String> {
    encode(PreviewFrame::Sample(sample))
}

fn encode(input: PreviewFrame) -> Option<String> {
    let frame = match input {
        #[cfg(test)]
        PreviewFrame::Rgb(frame) => frame,
        PreviewFrame::Sample(sample) => {
            let info = gstreamer_video::VideoInfo::from_caps(sample.caps()?).ok()?;
            if info.width() > 4096 || info.height() > 4096 {
                return None;
            }
            let output = gstreamer_video::VideoInfo::builder(
                gstreamer_video::VideoFormat::Rgb,
                info.width(),
                info.height(),
            )
            .fps(info.fps())
            .interlace_mode(info.interlace_mode())
            .build()
            .ok()?;
            let source =
                gstreamer_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer()?, &info)
                    .ok()?;
            let mut buffer = gstreamer::Buffer::with_size(output.size()).ok()?;
            let mut target = gstreamer_video::VideoFrameRef::from_buffer_ref_writable(
                buffer.get_mut()?,
                &output,
            )
            .ok()?;
            let converter = gstreamer_video::VideoConverter::new(&info, &output, None).ok()?;
            converter.frame_ref(&source, &mut target);
            RawRgbFrame {
                width: info.width(),
                height: info.height(),
                stride: usize::try_from(target.plane_stride()[0]).ok()?,
                bytes: target.plane_data(0).ok()?.to_vec(),
            }
        }
    };
    if frame.width == 0 || frame.height == 0 || frame.width > 4096 || frame.height > 4096 {
        return None;
    }
    let row = (frame.width as usize).checked_mul(3)?;
    if frame.stride < row {
        return None;
    }
    let mut pixels = Vec::with_capacity(row.checked_mul(frame.height as usize)?);
    for y in 0..frame.height as usize {
        let start = y.checked_mul(frame.stride)?;
        pixels.extend_from_slice(frame.bytes.get(start..start + row)?);
    }
    let image = image::RgbImage::from_raw(frame.width, frame.height, pixels)?;
    let thumb = image::imageops::thumbnail(&image, 320, 180);
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 60)
        .encode_image(&thumb)
        .ok()?;
    if jpeg.len() > 90_000 {
        return None;
    }
    Some(base64::engine::general_purpose::STANDARD.encode(jpeg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_sample_encodes_correct_red_thumbnail() {
        gstreamer::init().unwrap();
        let info = gstreamer_video::VideoInfo::from_caps(
            &gstreamer::Caps::builder("video/x-raw")
                .field("format", "NV12")
                .field("width", 4i32)
                .field("height", 4i32)
                .field("framerate", gstreamer::Fraction::new(15, 1))
                .field("colorimetry", "bt601")
                .build(),
        )
        .unwrap();
        let mut bytes = vec![81; info.size()];
        for pair in bytes[info.offset()[1]..].chunks_exact_mut(2) {
            pair.copy_from_slice(&[90, 240]);
        }
        let sample = gstreamer::Sample::builder()
            .buffer(&gstreamer::Buffer::from_slice(bytes))
            .caps(&info.to_caps().unwrap())
            .build();
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(encode(PreviewFrame::Sample(sample)).unwrap())
            .unwrap();
        let image = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        let pixel = image.get_pixel(0, 0).0;
        assert!(
            pixel[0] > 245 && pixel[1] < 10 && pixel[2] < 10,
            "{pixel:?}"
        );
    }

    #[test]
    fn only_cameras_streaming_right_now_are_captured() {
        let states: std::collections::HashMap<String, String> = [
            ("live", "streaming"),
            ("rec", "recording_ready"),
            ("down", "offline"),
            ("starting", "connecting"),
        ]
        .into_iter()
        .map(|(id, state)| (id.to_owned(), state.to_owned()))
        .collect();
        let demand = ["live", "down", "rec", "unknown", "starting"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(streaming_only(demand, &states), vec!["live", "rec"]);
    }
    #[tokio::test]
    async fn captures_only_when_requested_and_encodes_a_small_jpeg() {
        let hub = std::sync::Arc::new(PreviewFrameHub::default());
        assert_eq!(hub.waiting.load(Ordering::Relaxed), 0);
        hub.submit("cam", 2, 2, 6, &[0; 12]);
        let task = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.capture("cam", Duration::from_secs(2)).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while hub.waiting.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(hub.waiting.load(Ordering::Relaxed), 1);
        hub.submit("cam", 2, 2, 6, &[64; 12]);
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(task.await.unwrap().unwrap())
            .unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(hub.waiting.load(Ordering::Relaxed), 0);
    }
}
