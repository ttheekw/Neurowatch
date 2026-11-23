//! Local compressed camera fixture; no sockets or application service.
use gstreamer::prelude::*;
use nw_core::camera::CameraConfig;

pub(crate) struct CameraFile {
    pub camera: CameraConfig,
    path: std::path::PathBuf,
}

impl CameraFile {
    pub fn new(frames: u32) -> Self {
        Self::sized(frames, 320, 240)
    }

    pub fn sized(frames: u32, width: u32, height: u32) -> Self {
        Self::encoded(&format!(
            "videotestsrc num-buffers={frames} pattern=ball ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! videoscale ! video/x-raw,width={width},height={height} \
             ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=10"
        ), 30)
    }

    /// An H.265 camera (`None` where this computer has no `x265enc`; used
    /// only to make test pictures, never by the product).
    pub fn h265(frames: u32, width: u32, height: u32) -> Option<Self> {
        gstreamer::init().ok()?;
        gstreamer::ElementFactory::find("x265enc")?;
        Some(Self::encoded_with(
            &format!(
                "videotestsrc num-buffers={frames} pattern=ball ! video/x-raw,format=I420,width={width},height={height},framerate=25/1 \
                 ! x265enc key-int-max=25 tune=zerolatency"
            ),
            "h265parse",
            30,
        ))
    }

    /// Native-resolution detail and motion make decoder CPU worth measuring.
    fn encoded(source: &str, timeout_sec: u64) -> Self {
        Self::encoded_with(source, "h264parse", timeout_sec)
    }

    fn encoded_with(source: &str, parser: &str, timeout_sec: u64) -> Self {
        gstreamer::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("nw-stage4-{}.mkv", uuid::Uuid::new_v4().simple()));
        let make = crate::pipeline::builder::build(&format!(
            "{source} ! {parser} ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        make.set_state(gstreamer::State::Playing).unwrap();
        let message = make.bus().unwrap().timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(timeout_sec),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        make.set_state(gstreamer::State::Null).unwrap();
        assert_eq!(message.unwrap().type_(), gstreamer::MessageType::Eos);
        let camera = CameraConfig::new("Test camera", path.to_str().unwrap());
        Self { camera, path }
    }
}

impl Drop for CameraFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
