#![cfg(unix)]
//! The daemon served over a socket pair, driven as NeuroWatch drives it:
//! real H.264 of a moving ball in, motion events out.
use gstreamer::prelude::*;
use nw_analytics_protocol::{
    error_code,
    frame::{flags, read_frame, write_frame, Frame, MediaKind, MediaUnit},
    method,
    rpc::Message,
};
use nw_motion::{server, worker::Options};
use serde_json::{json, Value};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

struct Node {
    conn: UnixStream,
    next_id: u64,
}

impl Node {
    fn connect() -> Self {
        let (node, plugin) = UnixStream::pair().unwrap();
        let reader = plugin.try_clone().unwrap();
        std::thread::spawn(move || server::serve(reader, plugin, Options::default()));
        node.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self {
            conn: node,
            next_id: 1,
        }
    }

    fn send(&mut self, message: Message) {
        write_frame(&mut self.conn, &Frame::Message(message.to_value())).unwrap();
    }

    fn notify(&mut self, name: &str, params: Value) {
        self.send(Message::notification(name, params));
    }

    /// A request and its answer; notifications in between are returned too.
    fn call(
        &mut self,
        name: &str,
        mut params: Value,
    ) -> (Result<Value, i64>, Vec<(String, Value)>) {
        let challenge =
            (name == method::HELLO).then(|| nw_analytics_sdk::identity::challenge().unwrap());
        if let Some(challenge) = &challenge {
            params["challenge"] = json!(challenge);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send(Message::request(id, name, params));
        let mut seen = vec![];
        loop {
            match self.read() {
                Message::Response {
                    id: answered,
                    result,
                } if answered == id => {
                    if let (Some(challenge), Ok(value)) = (&challenge, &result) {
                        let manifest = serde_json::from_value(value.clone()).unwrap();
                        assert!(nw_analytics_sdk::identity::verify_manifest(
                            &manifest, challenge, ""
                        ));
                        let auth = nw_analytics_sdk::identity::authenticate(
                            &nw_analytics_sdk::identity::SigningKey::from_bytes(&[9; 32]),
                            challenge,
                            manifest.identity.as_ref().unwrap(),
                            None,
                        );
                        let (authorized, notices) =
                            self.call(method::AUTHENTICATE, serde_json::to_value(auth).unwrap());
                        assert!(authorized.is_ok());
                        seen.extend(notices);
                    }
                    return (result.map_err(|e| e.code), seen);
                }
                Message::Notification { method, params } => seen.push((method, params)),
                _ => {}
            }
        }
    }

    fn read(&mut self) -> Message {
        match read_frame(&mut self.conn).expect("a frame from the plugin") {
            Frame::Message(value) => Message::from_value(value).unwrap(),
            Frame::Media(_) => panic!("plugins never send media"),
        }
    }
}

/// H.264 access units (Annex-B) of a ball bouncing on black, 25 fps.
fn ball_units(frames: u32) -> Vec<(Vec<u8>, u64, bool)> {
    gstreamer::init().unwrap();
    let pipeline = gstreamer::parse::launch(&format!(
        "videotestsrc num-buffers={frames} pattern=ball \
         ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 \
         ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=25 \
         ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false"
    ))
    .unwrap()
    .downcast::<gstreamer::Pipeline>()
    .unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    pipeline.set_state(gstreamer::State::Playing).unwrap();
    let mut units = vec![];
    while let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_seconds(10)) {
        let buffer = sample.buffer().unwrap();
        let keyframe = !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT);
        let pts = buffer.pts().unwrap().nseconds();
        units.push((buffer.map_readable().unwrap().to_vec(), pts, keyframe));
    }
    pipeline.set_state(gstreamer::State::Null).unwrap();
    assert_eq!(units.len(), frames as usize);
    units
}

#[test]
fn a_moving_ball_is_reported_as_motion_and_ends_when_unbound() {
    let units = ball_units(150);
    let mut node = Node::connect();

    let (manifest, _) = node.call(
        method::HELLO,
        json!({"protocol": 1, "node": {"name": "Test", "version": "0"}}),
    );
    let manifest = manifest.unwrap();
    assert_eq!(manifest["id"], "nw.motion");
    assert_eq!(manifest["event_kinds"][0]["id"], "motion");

    // The CPU, named: what the status reports is then known.
    let config = json!({"sensitivity": 10, "min_duration_sec": 0.0, "cooldown_sec": 1.0,
                        "decoder": "software"});
    let (accepted, _) = node.call(
        method::ATTACH,
        json!({"binding": 4, "camera": {"id": "cam-1", "name": "Door"}, "config": config}),
    );
    assert_eq!(accepted.unwrap(), json!({"keyframes_only": false}));

    node.notify(
        method::STREAM_FORMAT,
        json!({"binding": 4, "codec": "h264", "width": 320, "height": 240}),
    );
    let started = 1_761_000_000_000_000i64;
    for (seq, (data, pts, keyframe)) in units.into_iter().enumerate() {
        let mut unit_flags = if keyframe { flags::KEYFRAME } else { 0 };
        if seq == 0 {
            unit_flags |= flags::DISCONTINUITY;
        }
        let unit = MediaUnit {
            media: MediaKind::Video,
            flags: unit_flags,
            binding: 4,
            seq: seq as u64,
            time_us: started + (pts / 1000) as i64,
            pts_ns: Some(pts),
            dts_ns: None,
            data,
        };
        write_frame(&mut node.conn, &Frame::Media(unit)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen: Vec<(String, Value)> = vec![];
    let has = |seen: &[(String, Value)], name: &str, test: &dyn Fn(&Value) -> bool| {
        seen.iter().any(|(m, p)| m == name && test(p))
    };
    let running = |p: &Value| p["bindings"][0]["state"] == "running";
    let boxed = |p: &Value| p["layer"] == "detections" && p["items"][0]["type"] == "box";
    while !(has(&seen, method::EVENT_START, &|_| true)
        && has(&seen, method::STATUS, &running)
        && has(&seen, method::OVERLAY, &boxed))
        && Instant::now() < deadline
    {
        if let Message::Notification {
            method: name,
            params,
        } = node.read()
        {
            seen.push((name, params));
        }
    }
    let start = seen
        .iter()
        .find(|(m, _)| m == method::EVENT_START)
        .map(|(_, p)| p.clone())
        .expect("motion started");
    assert_eq!(start["binding"], 4);
    assert_eq!(start["kind"], "motion");
    assert!(
        has(&seen, method::STATUS, &running),
        "status reported running"
    );
    assert!(
        has(&seen, method::STATUS, &|p| running(p)
            && p["bindings"][0]["decoder"] == "CPU (software)"),
        "the decoder chosen is the one used, and the status names it"
    );
    let detection = seen
        .iter()
        .find(|(m, p)| m == method::OVERLAY && boxed(p))
        .map(|(_, p)| p.clone())
        .expect("the moving area was drawn");
    assert!(detection["items"][0]["label"]
        .as_str()
        .unwrap()
        .starts_with("Motion "));
    assert_eq!(detection["ttl_ms"], 1500);
    assert!(
        has(&seen, method::OVERLAY, &|p| p["layer"] == "debug"
            && p["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["type"] == "text")),
        "the analysis was drawn too"
    );

    // Unbinding ends any open event before the reply: the worker is joined.
    let (detached, during) = node.call(method::DETACH, json!({"binding": 4, "reason": "test"}));
    assert_eq!(detached.unwrap(), Value::Null);
    seen.extend(during);
    let numbers = |name: &str| -> Vec<Value> {
        seen.iter()
            .filter(|(m, _)| m == name)
            .map(|(_, p)| p["event"].clone())
            .collect()
    };
    let (started, ended) = (numbers(method::EVENT_START), numbers(method::EVENT_END));
    assert!(!started.is_empty());
    assert_eq!(started, ended, "every event that started ended");
    assert!(
        has(&seen, method::EVENT_UPDATE, &|p| p["bbox"].is_array()),
        "updates carry the moving area"
    );
}

/// Joining a stream already running gives large timestamps from the first
/// picture. 6 s of 25 fps video, at its own pace, must still be analysed at 5 fps.
#[test]
fn a_stream_joined_mid_way_is_analysed_at_the_same_rate() {
    let units = ball_units(150);
    let mut node = Node::connect();
    let (manifest, _) = node.call(
        method::HELLO,
        json!({"protocol": 1, "node": {"name": "Test", "version": "0"}}),
    );
    manifest.unwrap();
    let (accepted, _) = node.call(
        method::ATTACH,
        json!({"binding": 5, "camera": {"id": "cam-1", "name": "Door"}, "config": {}}),
    );
    accepted.unwrap();
    node.notify(
        method::STREAM_FORMAT,
        json!({"binding": 5, "codec": "h264", "width": 320, "height": 240}),
    );
    const JOINED_AT_NS: u64 = 600_000_000_000;
    let started = 1_761_000_000_000_000i64;
    // On its own thread: the plugin's replies have to be read meanwhile, or
    // they fill the socket and both sides stop.
    let mut conn = node.conn.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        let clock = Instant::now();
        for (seq, (data, pts, keyframe)) in units.into_iter().enumerate() {
            // 25 fps by count: the encoder's first timestamp may be far from zero.
            let due = Duration::from_millis(seq as u64 * 40);
            std::thread::sleep(due.saturating_sub(clock.elapsed()));
            let mut unit_flags = if keyframe { flags::KEYFRAME } else { 0 };
            if seq == 0 {
                unit_flags |= flags::DISCONTINUITY;
            }
            let unit = MediaUnit {
                media: MediaKind::Video,
                flags: unit_flags,
                binding: 5,
                seq: seq as u64,
                time_us: started + (pts / 1000) as i64,
                pts_ns: Some(JOINED_AT_NS + pts),
                dts_ns: None,
                data,
            };
            write_frame(&mut conn, &Frame::Media(unit)).unwrap();
        }
    });
    // Statuses carry the pictures analysed so far. Read on a thread too: a plugin
    // with no more video has nothing to say, and this must end regardless.
    let mut replies = node.conn.try_clone().unwrap();
    let (found, statuses) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(frame) = read_frame(&mut replies) {
            if let Frame::Message(value) = frame {
                if let Ok(Message::Notification {
                    method: name,
                    params,
                }) = Message::from_value(value)
                {
                    if name == method::STATUS && found.send(params).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let begun = Instant::now();
    let mut analysed = 0;
    let mut decoder = None;
    while begun.elapsed() < Duration::from_secs(8) {
        if let Ok(params) = statuses.recv_timeout(Duration::from_millis(250)) {
            let frames = params["bindings"][0]["frames"].as_u64().unwrap_or(0);
            analysed = analysed.max(frames);
            if let Some(name) = params["bindings"][0]["decoder"].as_str() {
                decoder = Some(name.to_owned());
            }
        }
    }
    // Automatic (no setting): whichever decoder this machine has, named.
    eprintln!("Automatic decoded on {decoder:?}");
    assert!(decoder.is_some_and(|name| !name.is_empty()));
    writer.join().unwrap();
    // The reading thread holds the connection open otherwise.
    node.conn.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(
        (20..=36).contains(&analysed),
        "6 s at 5 fps is about 30 pictures, not the 150 sent: {analysed}"
    );
}

#[test]
fn requests_are_checked() {
    let mut node = Node::connect();
    let (early, _) = node.call(method::ATTACH, json!({}));
    assert_eq!(early.unwrap_err(), error_code::INVALID_PARAMS);
    let (old, _) = node.call(
        method::HELLO,
        json!({"protocol": 99, "node": {"name": "x", "version": "0"}}),
    );
    assert_eq!(old.unwrap_err(), error_code::UNSUPPORTED_PROTOCOL);
    node.call(
        method::HELLO,
        json!({"protocol": 1, "node": {"name": "x", "version": "0"}}),
    )
    .0
    .unwrap();

    let (bad, _) = node.call(
        method::ATTACH,
        json!({"binding": 1, "camera": {"id": "c", "name": "C"}, "config": {"sensitivity": 0}}),
    );
    assert_eq!(bad.unwrap_err(), error_code::INVALID_CONFIG);
    let (validation, _) = node.call(
        method::VALIDATE_CONFIG,
        json!({"config": {"cooldown_sec": 9999}}),
    );
    assert_eq!(validation.unwrap()["ok"], false);
    let (low_cpu, _) = node.call(
        method::ATTACH,
        json!({"binding": 2, "camera": {"id": "c", "name": "C"}, "config": {"low_cpu": true}}),
    );
    assert_eq!(low_cpu.unwrap()["keyframes_only"], true);
    let (unknown, _) = node.call(method::CONFIGURE, json!({"binding": 9, "config": {}}));
    assert_eq!(unknown.unwrap_err(), error_code::UNKNOWN_BINDING);
    let (gone, _) = node.call(method::DETACH, json!({"binding": 9, "reason": "x"}));
    assert_eq!(gone.unwrap_err(), error_code::UNKNOWN_BINDING);
}

#[test]
fn local_desktop_pictures_prove_locality_and_keep_capture_identity() {
    use nw_analytics_sdk::{
        local::write_descriptor,
        locality::LocalityProbe,
        ring::{PictureIdentity, RawFormat, RawLayout, RingWriter},
    };
    gstreamer::init().unwrap();
    let dir = std::env::temp_dir().join(format!("nw-motion-raw-{}", std::process::id()));
    let (conn, plugin) = UnixStream::pair().unwrap();
    std::thread::spawn(move || server::serve_local(plugin, Options::default()).unwrap());
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let probe = LocalityProbe::create(&dir).unwrap();
    let challenge = nw_analytics_sdk::identity::challenge().unwrap();
    write_descriptor(&conn, &Frame::Message(Message::request(1, method::HELLO, json!({
        "protocol": 1, "node": {"name": "Test", "version": "0"}, "locality": probe.offer(), "challenge": challenge,
    })).to_value()), &probe.ring.descriptor().unwrap()).unwrap();
    let mut node = Node { conn, next_id: 2 };
    let Message::Response {
        result: Ok(manifest),
        ..
    } = node.read()
    else {
        panic!("manifest");
    };
    assert!(probe.verify(manifest["locality_proof"].as_str()));
    let manifest: nw_analytics_protocol::messages::Manifest =
        serde_json::from_value(manifest).unwrap();
    assert!(nw_analytics_sdk::identity::verify_manifest(
        &manifest, &challenge, ""
    ));
    let auth = nw_analytics_sdk::identity::authenticate(
        &nw_analytics_sdk::identity::SigningKey::from_bytes(&[9; 32]),
        &challenge,
        manifest.identity.as_ref().unwrap(),
        None,
    );
    assert!(node
        .call(method::AUTHENTICATE, serde_json::to_value(auth).unwrap())
        .0
        .is_ok());
    let (accepted, _) = node.call(method::ATTACH, json!({"binding": 3, "camera": {"id": "raw", "name": "Raw"}, "config": {"sensitivity":10,"min_duration_sec":0,"cooldown_sec":0.5}}));
    assert!(accepted.is_ok());
    let layout = RawLayout {
        width: 320,
        height: 180,
        format: RawFormat::Gray8,
        color: None,
    };
    let mut ring = RingWriter::create(&dir, layout).unwrap();
    let first = 1u64 << 40;
    ring.write(
        PictureIdentity {
            seq: first,
            time_us: 1_700_000_000_000_000,
            pts_ns: None,
        },
        [&vec![0; 320 * 180], &[]],
        [320, 0],
    )
    .unwrap();
    write_descriptor(
        &node.conn,
        &Frame::Message(
            Message::notification(
                method::DECODED_AVAILABLE,
                json!({
                    "binding":3,"layout":layout,"ring":{"ring_id":ring.id(),"bytes":ring.bytes()}
                }),
            )
            .to_value(),
        ),
        &ring.descriptor().unwrap(),
    )
    .unwrap();
    for seq in 1..18 {
        let mut pixels = vec![0; 320 * 180];
        let x = 20 + seq as usize * 9;
        for y in 50..85 {
            pixels[y * 320 + x..y * 320 + x + 35].fill(255);
        }
        ring.write(
            PictureIdentity {
                seq: first + seq,
                time_us: 1_700_000_000_000_000 + seq as i64 * 200_000,
                pts_ns: None,
            },
            [&pixels, &[]],
            [320, 0],
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(210));
    }
    let (detached, notices) = node.call(method::DETACH, json!({"binding":3,"reason":"test"}));
    assert!(detached.is_ok());
    let starts: Vec<_> = notices
        .iter()
        .filter(|(method, _)| method == method::EVENT_START)
        .collect();
    assert!(
        !starts.is_empty(),
        "shared raw pictures must be analyzed: {notices:?}"
    );
    assert!(starts
        .iter()
        .all(|(_, p)| p["seq"].as_u64().is_some_and(|seq| seq >= first)));
    assert!(notices
        .iter()
        .any(|(m, p)| m == method::OVERLAY && p["seq"].as_u64().is_some_and(|seq| seq >= first)));
    assert!(notices.iter().any(|(m, p)| m == method::STATUS
        && p["bindings"][0]["decoder"] == "NeuroWatch's pictures (no decoding here)"));
    drop(ring);
    drop(probe);
    let _ = std::fs::remove_dir(dir);
}

fn reply(conn: &mut UnixStream) -> Result<Value, nw_analytics_protocol::rpc::RpcError> {
    match read_frame(conn).unwrap() {
        Frame::Message(value) => match Message::from_value(value).unwrap() {
            Message::Response { result, .. } => result,
            other => panic!("unexpected {other:?}"),
        },
        Frame::Media(_) => panic!("plugins never send media"),
    }
}

/// Hello, then `authenticate` with `credential`, as NeuroWatch does: the
/// refusal's code when turned away.
fn authenticate(conn: &mut UnixStream, credential: Option<&str>) -> Result<(), String> {
    let challenge = nw_analytics_sdk::identity::challenge().unwrap();
    let hello =
        json!({"protocol": 1, "node": {"name": "x", "version": "0"}, "challenge": challenge});
    write_frame(
        conn,
        &Frame::Message(Message::request(1, method::HELLO, hello).to_value()),
    )
    .unwrap();
    let manifest: nw_analytics_protocol::messages::Manifest =
        serde_json::from_value(reply(conn).unwrap()).unwrap();
    let auth = nw_analytics_sdk::identity::authenticate(
        &nw_analytics_sdk::identity::SigningKey::from_bytes(&[9; 32]),
        &challenge,
        manifest.identity.as_ref().unwrap(),
        credential.map(str::to_owned),
    );
    let auth = serde_json::to_value(auth).unwrap();
    write_frame(
        conn,
        &Frame::Message(Message::request(2, method::AUTHENTICATE, auth).to_value()),
    )
    .unwrap();
    reply(conn).map(|_| ()).map_err(|error| {
        assert_eq!(error.code, error_code::REJECTED);
        error.data.unwrap()["code"].as_str().unwrap().to_owned()
    })
}

fn ping(conn: &mut UnixStream) -> Result<Value, nw_analytics_protocol::rpc::RpcError> {
    write_frame(
        conn,
        &Frame::Message(Message::request(9, method::PING, Value::Null).to_value()),
    )
    .unwrap();
    reply(conn)
}

/// The connection was closed by the daemon (end of stream).
fn closed(conn: &mut UnixStream) -> bool {
    matches!(read_frame(conn), Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof)
}

struct Codes {
    path: std::path::PathBuf,
    file: nw_motion::access::CodeFile,
}

impl Codes {
    fn new(codes: &[(&str, &str, usize)]) -> Self {
        let mut file = nw_motion::access::CodeFile::new().unwrap();
        file.iterations = nw_motion::access::MIN_ITERATIONS;
        for (name, code, max) in codes {
            file.add(name, code, *max).unwrap();
        }
        let path = std::env::temp_dir().join(format!(
            "nw-motion-daemon-codes-{}.json",
            nw_motion::access::new_code().unwrap()
        ));
        file.write(&path).unwrap();
        Self { path, file }
    }

    fn save(&self) {
        self.file.write(&self.path).unwrap();
    }
}

impl Drop for Codes {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Connect over a real Unix socket (so the daemon can close it), the way
/// `listen` serves one.
fn connect(access: &std::sync::Arc<nw_motion::access::Access>, silence: Duration) -> UnixStream {
    let (node, plugin) = UnixStream::pair().unwrap();
    let access = std::sync::Arc::clone(access);
    let options = Options {
        silence_limit: silence,
    };
    std::thread::spawn(move || server::serve_local_with_access(plugin, options, access));
    node.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    node
}

#[test]
fn access_codes_admit_nodes_up_to_each_codes_limit() {
    let codes = Codes::new(&[("site-a", "site-a-secret", 2)]);
    let access = std::sync::Arc::new(nw_motion::access::Access::load(&codes.path).unwrap());
    let connect = || connect(&access, Duration::from_secs(15));

    let mut nameless = connect();
    assert_eq!(
        authenticate(&mut nameless, None).unwrap_err(),
        "ACCESS_CODE"
    );
    // Turned away: nothing else is served on that connection.
    write_frame(
        &mut nameless,
        &Frame::Message(
            Message::request(3, method::VALIDATE_CONFIG, json!({"config": {}})).to_value(),
        ),
    )
    .unwrap();
    assert_eq!(reply(&mut nameless).unwrap_err().code, error_code::REJECTED);
    assert_eq!(
        authenticate(&mut connect(), Some("site-b-secret")).unwrap_err(),
        "ACCESS_CODE"
    );

    let mut first = connect();
    authenticate(&mut first, Some("site-a-secret")).unwrap();
    let mut second = connect();
    authenticate(&mut second, Some("site-a-secret")).unwrap();
    assert_eq!(
        authenticate(&mut connect(), Some("site-a-secret")).unwrap_err(),
        "CONNECTION_LIMIT"
    );

    // A recorder that disconnects gives its place back.
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(5);
    while access.holding("site-a") > 1 {
        assert!(Instant::now() < deadline, "the place was never given back");
        std::thread::sleep(Duration::from_millis(10));
    }
    authenticate(&mut connect(), Some("site-a-secret")).unwrap();
}

#[test]
fn a_silent_recorder_is_closed_and_its_place_given_back() {
    let codes = Codes::new(&[("site-a", "site-a-secret", 1)]);
    let access = std::sync::Arc::new(nw_motion::access::Access::load(&codes.path).unwrap());
    let silence = Duration::from_millis(400);

    // Pinging keeps a connection open well past the limit.
    let mut alive = connect(&access, silence);
    authenticate(&mut alive, Some("site-a-secret")).unwrap();
    for _ in 0..8 {
        std::thread::sleep(silence / 4);
        ping(&mut alive).unwrap();
    }
    // Then it goes quiet, as over a lost network: closed, place given back.
    let started = Instant::now();
    assert!(closed(&mut alive));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(access.holding("site-a"), 0);
    authenticate(&mut connect(&access, silence), Some("site-a-secret")).unwrap();
}

#[test]
fn editing_the_code_file_disconnects_recorders_it_no_longer_admits() {
    let mut codes = Codes::new(&[
        ("site-a", "site-a-secret", 2),
        ("site-b", "site-b-secret", 1),
    ]);
    let access = std::sync::Arc::new(nw_motion::access::Access::load(&codes.path).unwrap());
    let connect = || connect(&access, Duration::from_secs(15));
    let mut a1 = connect();
    authenticate(&mut a1, Some("site-a-secret")).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let mut a2 = connect();
    authenticate(&mut a2, Some("site-a-secret")).unwrap();
    let mut b = connect();
    authenticate(&mut b, Some("site-b-secret")).unwrap();

    codes.file.set_limit("site-a", 1);
    codes.file.remove("site-b");
    codes.save();
    assert!(access.reload());

    assert!(
        closed(&mut a2),
        "the newest connection over the new limit is closed"
    );
    assert!(closed(&mut b), "a removed code's connection is closed");
    ping(&mut a1).unwrap();
    assert_eq!(
        authenticate(&mut connect(), Some("site-b-secret")).unwrap_err(),
        "ACCESS_CODE"
    );
    assert_eq!(
        authenticate(&mut connect(), Some("site-a-secret")).unwrap_err(),
        "CONNECTION_LIMIT"
    );
}
