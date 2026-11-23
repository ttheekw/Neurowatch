use super::*;
use crate::streams::testing::CameraFile;
use std::sync::atomic::AtomicUsize;

fn installed<'a>(names: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
    move |name| names.contains(&name)
}

#[test]
fn encoders_are_tried_in_the_documented_order_strict_before_relaxed() {
    let everything = [
        "x264enc",
        "openh264enc",
        "v4l2h264enc",
        "mfh264enc",
        "qsvh264enc",
        "nvh264enc",
        "vah264lpenc",
        "vah264enc",
    ];
    let mut failed = Vec::new();
    let mut order = Vec::new();
    while let Some(trial) = next_trial(installed(&everything), &failed) {
        order.push((trial.encoder.factory, trial.strict));
        failed.push(trial);
    }
    assert_eq!(
        order,
        vec![
            ("vah264enc", true),
            ("vah264enc", false),
            ("vah264lpenc", true),
            ("vah264lpenc", false),
            ("nvh264enc", true),
            ("nvh264enc", false),
            ("qsvh264enc", true),
            ("qsvh264enc", false),
            ("mfh264enc", true),
            ("mfh264enc", false),
            ("v4l2h264enc", true),
            ("v4l2h264enc", false),
        ]
    );
}

#[test]
fn the_gpl_and_patent_encumbered_encoders_are_never_candidates() {
    assert!(ENCODERS
        .iter()
        .all(|e| e.factory != "x264enc" && e.factory != "openh264enc"));
    // Only those installed: nothing left means the CPU fallback.
    assert_eq!(
        next_trial(installed(&["x264enc", "openh264enc"]), &[]),
        None
    );
    assert_eq!(next_trial(|_| false, &[]), None);
}

#[test]
fn a_failed_encoder_is_passed_over_for_the_next_one_installed() {
    let names = ["vah264lpenc", "nvh264enc"];
    let first = next_trial(installed(&names), &[]).unwrap();
    assert_eq!((first.encoder.factory, first.strict), ("vah264lpenc", true));
    // Its strict trial failed to negotiate: the same encoder, relaxed.
    let mut failed = vec![first];
    let second = next_trial(installed(&names), &failed).unwrap();
    assert_eq!(
        (second.encoder.factory, second.strict),
        ("vah264lpenc", false)
    );
    // A timeout gives up on both variants at once.
    failed.clear();
    exclude(&mut failed, first, true);
    exclude(&mut failed, first, true);
    assert_eq!(failed.len(), 2, "no duplicates");
    let next = next_trial(installed(&names), &failed).unwrap();
    assert_eq!(next.encoder.factory, "nvh264enc");
    exclude(&mut failed, next, true);
    assert_eq!(
        next_trial(installed(&names), &failed),
        None,
        "CPU from here"
    );
}

#[test]
fn the_inventory_names_the_encoder_a_transcode_would_use_now() {
    assert_eq!(encoder_label(installed(&["vah264lpenc"])), "VA-API");
    assert_eq!(
        encoder_label(installed(&["nvh264enc", "mfh264enc"])),
        "NVENC"
    );
    assert_eq!(encoder_label(installed(&["qsvh264enc"])), "Quick Sync");
    assert_eq!(encoder_label(installed(&["mfh264enc"])), "Media Foundation");
    assert_eq!(encoder_label(installed(&["v4l2h264enc"])), "V4L2");
    assert_eq!(encoder_label(installed(&["x264enc"])), "CPU");
    assert_eq!(encoder_label(|_| false), "CPU");
}

#[test]
fn the_target_bit_rate_follows_the_picture_and_never_exceeds_the_grant() {
    // 1080p30 wants a few Mbit/s; the grant's cap wins when lower.
    let wanted = target_kbps(1920, 1080, 0);
    assert!((3_000..6_000).contains(&wanted), "{wanted}");
    assert_eq!(target_kbps(1920, 1080, 1_500), 1_500);
    assert_eq!(target_kbps(1920, 1080, 50_000), wanted);
    // A small picture still gets a usable rate; a tiny cap is honoured.
    assert_eq!(target_kbps(320, 240, 0), MIN_TARGET_KBPS);
    assert_eq!(target_kbps(320, 240, 200), 200);
}

#[test]
fn a_constant_quantiser_encoder_is_steered_by_its_measured_rate() {
    let control = QpControl { qp: 34 };
    assert_eq!(control.next(0, 2_000), 34, "nothing measured yet");
    assert_eq!(control.next(2_100, 2_000), 34, "close enough");
    assert_eq!(control.next(2_500, 2_000), 36, "over: coarser");
    assert_eq!(control.next(900, 2_000), 33, "well under: finer");
    assert_eq!(QpControl { qp: 47 }.next(9_000, 2_000), QpControl::MAX);
    assert_eq!(QpControl { qp: 20 }.next(100, 2_000), QpControl::MIN);
}

#[test]
fn planes_are_packed_without_their_stride_padding() {
    let plane = [1u8, 2, 3, 0, 4, 5, 6, 0];
    assert_eq!(pack(&plane, 4, 3, 2), Some(vec![1, 2, 3, 4, 5, 6]));
    assert_eq!(pack(&[1, 2, 3, 4], 2, 2, 2), Some(vec![1, 2, 3, 4]));
    assert_eq!(pack(&plane, 4, 3, 3), None, "a short plane is refused");
    assert_eq!(pack(&plane, 2, 3, 2), None, "stride below the width");
}

#[test]
fn what_the_status_and_diagnostics_report_has_the_documented_shape() {
    let info = TranscodeInfo {
        camera_id: "cam-1".into(),
        camera_name: "Front door".into(),
        stream: "main",
        from: "h265",
        to: "h264",
        decoder: "VA-API",
        encoder: "VA-API",
        viewers: 2,
    };
    assert_eq!(
        serde_json::to_value(&info).unwrap(),
        serde_json::json!({
            "camera_id": "cam-1", "camera_name": "Front door", "stream": "main",
            "from": "h265", "to": "h264", "decoder": "VA-API",
            "encoder": "VA-API", "viewers": 2
        })
    );
    assert_eq!(
        serde_json::to_value(StreamTranscode::from(&info)).unwrap(),
        serde_json::json!({
            "from": "h265", "to": "h264", "decoder": "VA-API",
            "encoder": "VA-API", "viewers": 2
        })
    );
}

#[derive(Default)]
struct Collector {
    units: Mutex<Vec<EncodedAccessUnit>>,
    pictures: Mutex<Vec<Arc<Picture>>>,
    failed: AtomicUsize,
}

impl TranscodeSink for Collector {
    fn unit(&self, unit: &EncodedAccessUnit) {
        self.units.lock().unwrap().push(unit.clone());
    }
    fn picture(&self, picture: &Arc<Picture>) {
        self.pictures.lock().unwrap().push(Arc::clone(picture));
    }
    fn resync(&self) {}
    fn fail(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }
}

fn camera_in_a_closed_drawer() -> CameraConfig {
    CameraConfig::new("Test camera", "/nonexistent/nw-transcode-test.mkv")
}

#[test]
fn a_transcode_is_shared_by_its_viewers_and_stops_with_the_last_one() {
    let hubs = StreamHubs::default();
    let camera = camera_in_a_closed_drawer();
    let sink: Arc<dyn TranscodeSink> = Arc::new(Collector::default());
    let first = hubs
        .transcode(&camera, "main", 4_000, Arc::clone(&sink))
        .unwrap();
    let second = hubs
        .transcode(&camera, "main", 1_500, Arc::clone(&sink))
        .unwrap();
    assert!(
        Arc::ptr_eq(&first.transcode, &second.transcode),
        "one transcode per camera stream"
    );
    assert_eq!(lock(&hubs.transcodes.map).len(), 1);
    // The encoder follows the lowest grant among its viewers.
    assert_eq!(first.transcode.cap_kbps.load(Ordering::Relaxed), 1_500);
    let transcode = Arc::clone(&first.transcode);
    drop(second);
    assert_eq!(transcode.cap_kbps.load(Ordering::Relaxed), 4_000);
    assert!(!transcode.stopped(), "a viewer is left");
    drop(first);
    assert!(transcode.stopped(), "the last viewer left");
    assert!(lock(&hubs.transcodes.map).is_empty());
    // A session joining afterwards gets a transcode of its own.
    let again = hubs.transcode(&camera, "main", 0, sink).unwrap();
    assert!(!Arc::ptr_eq(&again.transcode, &transcode));
    drop(again);
}

#[test]
fn a_joining_viewer_starts_from_the_cached_gop() {
    let hubs = StreamHubs::default();
    let camera = camera_in_a_closed_drawer();
    let early: Arc<dyn TranscodeSink> = Arc::new(Collector::default());
    let first = hubs.transcode(&camera, "sub", 0, early).unwrap();
    let unit = |keyframe, n: u8| EncodedAccessUnit {
        frame: None,
        codec: VideoCodec::H264,
        payload: vec![n],
        keyframe,
        width: 640,
        height: 360,
        timestamp_us: i64::from(n),
        pts_us: None,
    };
    for (keyframe, n) in [(true, 1), (false, 2), (true, 3), (false, 4)] {
        first.transcode.publish_unit(&unit(keyframe, n));
    }
    let late = Arc::new(Collector::default());
    let joined = hubs
        .transcode(
            &camera,
            "sub",
            0,
            Arc::clone(&late) as Arc<dyn TranscodeSink>,
        )
        .unwrap();
    let got: Vec<_> = late
        .units
        .lock()
        .unwrap()
        .iter()
        .map(|u| (u.keyframe, u.payload[0]))
        .collect();
    assert_eq!(
        got,
        vec![(true, 3), (false, 4)],
        "the current GOP, keyframe first"
    );
    drop((first, joined));
}

#[test]
fn a_transcode_is_listed_once_its_decoder_and_encoder_are_known() {
    let hubs = StreamHubs::default();
    let camera = camera_in_a_closed_drawer();
    let viewer = hubs
        .transcode(&camera, "main", 0, Arc::new(Collector::default()))
        .unwrap();
    assert!(hubs.transcodes().is_empty(), "nothing is running yet");
    {
        let mut state = lock(&viewer.transcode.state);
        state.to = Some("h264");
        state.encoder = Some("VA-API");
        state.decoder = Some("CPU");
    }
    let listed = hubs.transcodes();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (
            listed[0].camera_name.as_str(),
            listed[0].stream,
            listed[0].viewers
        ),
        ("Test camera", "main", 1)
    );
    let row = hubs.transcodes.row(&SourceKey::of(&camera)).unwrap();
    assert_eq!(
        (row.from, row.to, row.encoder, row.decoder),
        ("h265", "h264", "VA-API", "CPU")
    );
    drop(viewer);
    assert!(hubs.transcodes().is_empty());
}

/// Waits until `done` holds or the time is up.
fn wait_for(limit: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
}

/// The units are a valid H.264 stream: a software decoder (when this
/// computer has one) gives a picture for each.
fn decodes_as_h264(units: &[EncodedAccessUnit]) {
    use gstreamer::prelude::*;
    if gstreamer::ElementFactory::find("avdec_h264").is_none() {
        return;
    }
    let pipeline = gstreamer::parse::launch(
        "appsrc name=src format=time ! h264parse ! avdec_h264 ! videoconvert ! \
         appsink name=sink sync=false emit-signals=false",
    )
    .unwrap()
    .downcast::<gstreamer::Pipeline>()
    .unwrap();
    let src = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gstreamer_app::AppSrc>()
        .unwrap();
    src.set_caps(Some(
        &gstreamer::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
    ));
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    pipeline.set_state(gstreamer::State::Playing).unwrap();
    let from = units.iter().position(|u| u.keyframe).unwrap();
    for (n, unit) in units[from..].iter().enumerate() {
        let mut buffer = gstreamer::Buffer::from_slice(unit.payload.clone());
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gstreamer::ClockTime::from_mseconds(n as u64 * 40));
        src.push_buffer(buffer).unwrap();
    }
    src.end_of_stream().unwrap();
    let mut pictures = 0;
    while sink
        .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
        .is_some()
    {
        pictures += 1;
    }
    pipeline.set_state(gstreamer::State::Null).unwrap();
    assert_eq!(
        pictures,
        units.len() - from,
        "every unit decodes to a picture"
    );
}

/// NAL unit types in an Annex-B payload.
fn nal_types(payload: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < payload.len() {
        if payload[i..i + 3] == [0, 0, 1] {
            types.push(payload[i + 3] & 0x1f);
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

/// Real GStreamer: an H.265 camera through the hub, decoded and re-encoded
/// as H.264 by whatever encoder this computer has (the CPU fallback when it
/// has none). Skipped where no `x265enc` exists to make the picture.
#[test]
fn an_h265_camera_comes_out_as_h264_access_units_with_parameter_sets() {
    let Some(fixture) = CameraFile::h265(400, 640, 360) else {
        eprintln!("skipped: no x265enc to make an H.265 camera");
        return;
    };
    let hubs = StreamHubs::default();
    let sink = Arc::new(Collector::default());
    let viewer = hubs
        .transcode(
            &fixture.camera,
            "main",
            2_000,
            Arc::clone(&sink) as Arc<dyn TranscodeSink>,
        )
        .unwrap();
    let encoder_exists =
        next_trial(|name| gstreamer::ElementFactory::find(name).is_some(), &[]).is_some();
    let ok = wait_for(Duration::from_secs(40), || {
        sink.units.lock().unwrap().len() >= 110 || !sink.pictures.lock().unwrap().is_empty()
    });
    assert!(ok, "no output in 40 s");
    let listed = hubs.transcodes();
    let units = sink.units.lock().unwrap().clone();
    if !units.is_empty() {
        assert!(units.iter().all(|u| u.codec == VideoCodec::H264));
        assert!(units[0].keyframe, "starts at a keyframe");
        assert_eq!((units[0].width, units[0].height), (640, 360));
        // Parameter sets travel with the keyframe (SPS 7, PPS 8, IDR 5).
        let types = nal_types(&units[0].payload);
        assert!(
            types.contains(&7) && types.contains(&8) && types.contains(&5),
            "{types:?}"
        );
        assert!(units.iter().any(|u| !u.keyframe), "delta pictures follow");
        // A keyframe on the timer, not only at the start (4 s of video).
        let keyframes = units.iter().filter(|u| u.keyframe).count();
        assert!(
            keyframes >= 2,
            "{keyframes} keyframes in {} units",
            units.len()
        );
        decodes_as_h264(&units);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].to, "h264");
        assert!(ENCODERS.iter().any(|e| e.label == listed[0].encoder));
        assert!(encoder_exists);
        eprintln!(
            "H.264 out: {} units via {} (decoder {})",
            units.len(),
            listed[0].encoder,
            listed[0].decoder
        );
        // A viewer joining now starts at a keyframe from the cache.
        let late = Arc::new(Collector::default());
        let joined = hubs
            .transcode(
                &fixture.camera,
                "main",
                0,
                Arc::clone(&late) as Arc<dyn TranscodeSink>,
            )
            .unwrap();
        let first = late.units.lock().unwrap().first().cloned();
        assert!(first.is_some_and(|u| u.keyframe));
        drop(joined);
    } else {
        // No encoder here: the pictures for the CPU path.
        assert!(!encoder_exists, "an encoder exists but gave no H.264");
        let pictures = sink.pictures.lock().unwrap();
        assert_eq!((pictures[0].width, pictures[0].height), (640, 360));
        assert_eq!(listed[0].to, "vp8");
        assert_eq!(listed[0].encoder, CPU);
    }
    drop(viewer);
    assert!(wait_for(Duration::from_secs(5), || hubs
        .transcodes
        .map
        .lock()
        .unwrap()
        .is_empty()));
}

/// The CPU path's decode on its own: I420 pictures at the stream's size.
#[test]
fn the_cpu_path_decodes_to_packed_i420_pictures() {
    let Some(fixture) = CameraFile::h265(200, 320, 240) else {
        eprintln!("skipped: no x265enc to make an H.265 camera");
        return;
    };
    let hubs = StreamHubs::default();
    let sink = Arc::new(Collector::default());
    // No thread of its own: this test runs the CPU mode directly, as the
    // thread does when no H.264 encoder works.
    let transcode = Arc::new(Transcode {
        camera_id: fixture.camera.id.clone(),
        camera_name: fixture.camera.name.clone(),
        stream: "main",
        state: Mutex::default(),
        stop: AtomicBool::new(false),
        cap_kbps: AtomicU32::new(0),
    });
    lock(&transcode.state).viewers.push(Viewer {
        id: 1,
        cap_kbps: 0,
        sink: Arc::clone(&sink) as Arc<dyn TranscodeSink>,
    });
    let context = Context {
        hubs: hubs.clone(),
        registry: Arc::new(Registry::default()),
        key: SourceKey::of(&fixture.camera),
        transcode: Arc::clone(&transcode),
        camera: fixture.camera.clone(),
    };
    let (probe, collected) = (Arc::clone(&transcode), Arc::clone(&sink));
    let stopper = std::thread::spawn(move || {
        let ok = wait_for(Duration::from_secs(40), || {
            collected.pictures.lock().unwrap().len() >= 5
        });
        probe.stop.store(true, Ordering::Release);
        ok
    });
    let mut health = DecoderHealth::default();
    let end = attempt(&context, Mode::Cpu, &mut health);
    assert!(matches!(end, End::Stopped));
    assert!(stopper.join().unwrap(), "pictures arrived");
    let pictures = sink.pictures.lock().unwrap();
    let picture = &pictures[0];
    assert_eq!((picture.width, picture.height), (320, 240));
    assert_eq!(picture.y.len(), 320 * 240);
    assert_eq!(picture.u.len(), 160 * 120);
    assert_eq!(picture.v.len(), 160 * 120);
    assert!(picture.y.iter().any(|b| *b != 0), "a picture, not black");
    assert_eq!(lock(&transcode.state).to, Some("vp8"));
}
