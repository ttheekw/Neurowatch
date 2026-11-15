//! A session against a fake plugin on a Unix socket, fed by a local camera
//! file through a real stream hub.
use super::session::{self, Context};
use super::{Desired, DesiredBinding, PluginHost};
use crate::events::EventHub;
use crate::streams::StreamHubs;
use nw_analytics_protocol::{
    address::Address,
    frame::{read_frame, write_frame, Frame, MediaUnit},
    messages::NodeInfo,
    method,
    rpc::Message,
};
use nw_core::event::{EventPhase, EventSource};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex};
use std::time::{Duration, Instant};

struct FakePlugin {
    conn: UnixStream,
}

impl FakePlugin {
    fn next(&mut self) -> Frame {
        read_frame(&mut self.conn).expect("a frame from the node")
    }

    /// The next message, skipping media.
    fn message(&mut self) -> Message {
        loop {
            if let Frame::Message(value) = self.next() {
                return Message::from_value(value).expect("a JSON-RPC message");
            }
        }
    }

    fn media(&mut self) -> MediaUnit {
        loop {
            match self.next() {
                Frame::Media(unit) => return unit,
                Frame::Message(_) => continue,
            }
        }
    }

    fn send(&mut self, message: Message) {
        write_frame(&mut self.conn, &Frame::Message(message.to_value())).unwrap();
    }
}

fn manifest() -> Value {
    json!({
        "id": "nw.test", "name": "Test", "vendor": "NeuroWatch", "version": "0.0.1",
        "protocol": 1, "permissions": ["video", "events", "overlays"],
        "media": {"video": {"stream": "main", "codecs": ["h264"]}},
        "event_kinds": [{"id": "motion", "label": "Motion"}],
        "overlay_layers": [{"id": "detections", "label": "Detections", "default_on": true}]
    })
}

#[test]
fn a_plugin_receives_the_cameras_video_and_its_events_reach_the_hub() {
    let fixture = crate::streams::testing::CameraFile::new(500);
    let dir = std::env::temp_dir().join(format!("nw-plugin-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let db = Arc::new(Mutex::new(crate::db::connection::open_in_memory().unwrap()));
    let events = Arc::new(EventHub::new(db));
    let mut notices = events.subscribe();
    let host = PluginHost::default();
    let hubs = Arc::new(StreamHubs::default());
    let context = Context {
        managed: false,
        tickets: Default::default(),
        server_id: "fake".into(),
        address: Address::Unix(path.clone()),
        node: NodeInfo {
            name: "Test site".into(),
            version: "0".into(),
        },
        streams: Arc::clone(&hubs),
        events: Arc::clone(&events),
        status: Arc::clone(&host.status),
        overlays: Arc::clone(&host.overlays),
        approval: Some(nw_analytics_protocol::auth::Approval {
            public_key: nw_analytics_sdk::identity::public_key(
                &nw_analytics_sdk::identity::SigningKey::from_bytes(&[3; 32]),
            ),
            permissions: vec!["video".into(), "events".into(), "overlays".into()],
        }),
        config_path: dir.join("config.json"),
        queries: Arc::clone(&host.queries),
    };
    let desired = Arc::new(Mutex::new(Desired {
        bindings: vec![DesiredBinding {
            camera: fixture.camera.clone(),
            config: json!({"sensitivity": 7}),
            stream: None,
            moving: false,
            own_decoder: false,
        }],
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let desired = Arc::clone(&desired);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || session::run(context, desired, stop))
    };

    let (conn, _) = listener.accept().unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut plugin = FakePlugin { conn };
    let Message::Request {
        id,
        method: name,
        params,
    } = plugin.message()
    else {
        panic!("expected hello");
    };
    assert_eq!(name, method::HELLO);
    assert_eq!(params["protocol"], 1);
    assert_eq!(params["node"]["name"], "Test site");
    let mut signed = serde_json::from_value(manifest()).unwrap();
    nw_analytics_sdk::identity::sign_manifest(
        &mut signed,
        &nw_analytics_sdk::identity::SigningKey::from_bytes(&[3; 32]),
        params["challenge"].as_str().unwrap(),
        "",
    )
    .unwrap();
    plugin.send(Message::reply(
        id,
        Ok::<_, nw_analytics_protocol::rpc::RpcError>(signed),
    ));
    let Message::Request {
        id,
        method: name,
        params,
    } = plugin.message()
    else {
        panic!("expected authenticate");
    };
    assert_eq!(name, method::AUTHENTICATE);
    assert!(params["credential"].is_null());
    plugin.send(Message::reply(
        id,
        Ok::<_, nw_analytics_protocol::rpc::RpcError>(json!({"authorized":true})),
    ));

    let Message::Request {
        id,
        method: name,
        params,
    } = plugin.message()
    else {
        panic!("expected attach");
    };
    assert_eq!(name, method::ATTACH);
    assert_eq!(params["camera"]["id"], fixture.camera.id.as_str());
    assert_eq!(params["config"]["sensitivity"], 7);
    let binding = params["binding"].as_u64().unwrap() as u32;
    plugin.send(Message::reply(
        id,
        Ok::<_, nw_analytics_protocol::rpc::RpcError>(json!({})),
    ));

    let Message::Notification {
        method: name,
        params,
    } = plugin.message()
    else {
        panic!("expected stream_format");
    };
    assert_eq!(name, method::STREAM_FORMAT);
    assert_eq!(params["codec"], "h264");
    assert_eq!(params["width"], 320);

    let first = plugin.media();
    assert_eq!(first.binding, binding);
    assert!(first.keyframe() && first.discontinuity());
    assert_eq!(&first.data[..4], &[0, 0, 0, 1], "Annex-B");
    let mut last_seq = first.seq;
    for _ in 0..20 {
        let unit = plugin.media();
        assert!(unit.seq > last_seq);
        assert!(!unit.discontinuity());
        last_seq = unit.seq;
    }
    // The shared connection names its codec for diagnostics.
    let rows = hubs.diagnostics(std::slice::from_ref(&fixture.camera));
    assert_eq!(rows[0].codec, Some("h264"), "{rows:?}");

    let event = |name: &str, params: Value| Message::notification(name, params);
    plugin.send(event(
        method::EVENT_START,
        json!({"binding": binding, "event": 1, "kind": "motion", "score": 0.4, "seq": first.seq}),
    ));
    plugin.send(event(
        method::EVENT_START,
        json!({"binding": binding, "event": 2, "kind": "person"}),
    ));
    plugin.send(event(
        method::EVENT_START,
        json!({"binding": binding, "event": 3, "kind": "motion", "seq": u64::MAX}),
    ));
    plugin.send(event(method::EVENT_UPDATE, json!({"binding": binding, "event": 1, "score": 0.6, "bbox": [0.1, 0.2, 0.3, 0.4], "zones": ["door"]})));
    plugin.send(event(
        method::STATUS,
        json!({"bindings": [{"binding": binding, "state": "running", "fps": 5.0, "frames": 10,
            // More figures than kept, one too long and with a line break.
            "figures": std::iter::once(json!({"label": "x\ny".repeat(30), "value": "1"}))
                .chain((0..20).map(|i| json!({"label": format!("Lane {i}"), "value": i.to_string()})))
                .collect::<Vec<_>>()}]}),
    ));
    plugin.send(event(
        method::EVENT_END,
        json!({"binding": binding, "event": 1}),
    ));
    plugin.send(event(
        method::OVERLAY,
        json!({"binding": binding, "layer": "detections", "seq": last_seq,
               "items": [{"type": "box", "rect": [0.1, 0.2, 0.3, 0.4], "label": "Motion"}]}),
    ));
    plugin.send(event(
        method::OVERLAY,
        json!({"binding": binding, "layer": "detections", "seq": u64::MAX,
               "items": [{"type": "box", "rect": [0.0, 0.0, 1.0, 1.0], "label": "Invalid"}]}),
    ));
    plugin.send(event(
        method::OVERLAY,
        json!({"binding": binding, "layer": "secret",
               "items": [{"type": "box", "rect": [0.1, 0.2, 0.3, 0.4]}]}),
    ));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut phases = vec![];
    let mut ended = None;
    while ended.is_none() && Instant::now() < deadline {
        // Keep the node's media flowing while waiting.
        let _ = plugin.media();
        while let Ok(notice) = notices.try_recv() {
            phases.push(notice.phase);
            if notice.phase == EventPhase::End {
                ended = Some(notice.event);
            }
        }
    }
    let ended = ended.expect("the plugin's event ended in the hub");
    assert_eq!(ended.source, EventSource::Plugin);
    assert_eq!(ended.start_ms, first.time_us / 1000);
    assert_eq!(ended.topic.as_deref(), Some("nw.test"));
    assert_eq!(ended.camera_id.as_deref(), Some(fixture.camera.id.as_str()));
    assert_eq!(ended.score, Some(0.6));
    assert_eq!(ended.bbox, Some([0.1, 0.2, 0.3, 0.4]));
    assert_eq!(ended.zones, vec!["door".to_owned()]);
    assert_eq!(phases.first(), Some(&EventPhase::Start));
    // The undeclared kind was ignored: one event only.
    assert_eq!(
        phases.iter().filter(|p| **p == EventPhase::Start).count(),
        1
    );
    assert!(host.detecting(&fixture.camera.id));
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.overlays.snapshot(None).is_empty() && Instant::now() < deadline {
        let _ = plugin.media();
    }
    let drawn = host.overlays.snapshot(None);
    assert_eq!(drawn.len(), 1, "the undeclared layer is ignored");
    assert_eq!(drawn[0].camera_id, fixture.camera.id);
    assert_eq!(drawn[0].plugin, "fake");
    assert_eq!(drawn[0].layer, "detections");
    assert_eq!(drawn[0].frame, Some(last_seq));
    // The plugin's figures reach viewers (`plugins.cameras`), bounded.
    let config = super::PluginsConfig {
        servers: vec![super::PluginServer {
            id: "fake".into(),
            address: "unix:/x".into(),
            enabled: true,
            approval: None,
            credential_revision: 0,
            plugin_id: None,
            managed: None,
        }],
        bindings: vec![super::PluginBinding {
            server: "fake".into(),
            camera_id: fixture.camera.id.clone(),
            enabled: true,
            config: json!({}),
            stream: None,
            wall_layers: None,
            own_decoder: false,
            withdrawn: false,
        }],
    };
    let shown = &host.cameras(&config)[&fixture.camera.id][0];
    assert_eq!(
        shown.figures.len(),
        nw_analytics_protocol::messages::MAX_FIGURES
    );
    let long = &shown.figures[0].label;
    assert_eq!(
        long.chars().count(),
        nw_analytics_protocol::messages::MAX_FIGURE_TEXT
    );
    assert!(!long.contains('\n'), "one line");
    assert_eq!(
        (
            shown.figures[4].label.as_str(),
            shown.figures[4].value.as_str()
        ),
        ("Lane 3", "3")
    );

    // Unbinding detaches and the plugin is told.
    desired.lock().unwrap().bindings.clear();
    let detach = loop {
        if let Message::Request {
            method: name,
            params,
            ..
        } = plugin.message()
        {
            break (name, params);
        }
    };
    assert_eq!(detach.0, method::DETACH);
    assert_eq!(detach.1["binding"], binding);
    assert!(!host.detecting(&fixture.camera.id));
    assert!(
        host.overlays.snapshot(None).is_empty(),
        "unbinding clears the drawings"
    );

    stop.store(true, Ordering::Release);
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_unreachable_plugin_is_reported_and_retried() {
    let dir = std::env::temp_dir().join(format!("nw-plugin-{}", uuid::Uuid::new_v4().simple()));
    let host = PluginHost::default();
    let context = Context {
        managed: false,
        tickets: Default::default(),
        server_id: "gone".into(),
        address: Address::Unix(dir.join("missing.sock")),
        node: NodeInfo {
            name: "Test".into(),
            version: "0".into(),
        },
        streams: Arc::new(StreamHubs::default()),
        events: Arc::new(EventHub::new(Arc::new(Mutex::new(
            crate::db::connection::open_in_memory().unwrap(),
        )))),
        status: Arc::clone(&host.status),
        overlays: Arc::clone(&host.overlays),
        approval: Some(nw_analytics_protocol::auth::Approval {
            public_key: nw_analytics_sdk::identity::public_key(
                &nw_analytics_sdk::identity::SigningKey::from_bytes(&[3; 32]),
            ),
            permissions: vec!["video".into(), "events".into(), "overlays".into()],
        }),
        config_path: dir.join("config.json"),
        queries: Arc::clone(&host.queries),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || session::run(context, Arc::default(), stop))
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut state = "";
    while Instant::now() < deadline {
        let servers = host.status(&[super::PluginServer {
            id: "gone".into(),
            address: "unix:/x".into(),
            enabled: true,
            approval: None,
            credential_revision: 0,
            plugin_id: None,
            managed: None,
        }]);
        state = servers[0].state;
        if state == "error" {
            assert!(servers[0]
                .detail
                .as_deref()
                .unwrap()
                .starts_with("Could not connect"));
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(state, "error");
    stop.store(true, Ordering::Release);
    thread.join().unwrap();
}

#[test]
fn an_unapproved_or_changed_key_gets_no_camera_bindings_or_credentials() {
    for approval in [
        None,
        Some(nw_analytics_protocol::auth::Approval {
            public_key: nw_analytics_sdk::identity::public_key(
                &nw_analytics_sdk::identity::SigningKey::from_bytes(&[4; 32]),
            ),
            permissions: vec!["video".into(), "events".into(), "overlays".into()],
        }),
    ] {
        let dir = std::env::temp_dir().join(format!("nw-plugin-approval-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let host = PluginHost::default();
        let context = Context {
            managed: false,
            tickets: Default::default(),
            server_id: "test".into(),
            address: Address::Unix(path),
            node: NodeInfo {
                name: "Test".into(),
                version: "0".into(),
            },
            streams: Arc::new(StreamHubs::default()),
            events: Arc::new(EventHub::new(Arc::new(Mutex::new(
                crate::db::connection::open_in_memory().unwrap(),
            )))),
            status: Arc::clone(&host.status),
            overlays: Arc::clone(&host.overlays),
            approval,
            config_path: dir.join("config.json"),
            queries: Arc::clone(&host.queries),
        };
        let desired = Arc::new(Mutex::new(Desired {
            bindings: vec![DesiredBinding {
                camera: nw_core::camera::CameraConfig::new(
                    "Secret camera",
                    "rtsp://example.invalid",
                ),
                config: json!({}),
                stream: None,
                moving: false,
                own_decoder: false,
            }],
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || session::run(context, desired, stop))
        };
        let (conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut plugin = FakePlugin { conn };
        let Message::Request { id, params, .. } = plugin.message() else {
            panic!("hello");
        };
        let mut signed = serde_json::from_value(manifest()).unwrap();
        nw_analytics_sdk::identity::sign_manifest(
            &mut signed,
            &nw_analytics_sdk::identity::SigningKey::from_bytes(&[3; 32]),
            params["challenge"].as_str().unwrap(),
            "",
        )
        .unwrap();
        plugin.send(Message::reply(
            id,
            Ok::<_, nw_analytics_protocol::rpc::RpcError>(signed),
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && !host
                .status
                .lock()
                .unwrap()
                .get("test")
                .is_some_and(|status| status.state == "pending_approval")
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            host.status.lock().unwrap()["test"].state,
            "pending_approval"
        );
        plugin
            .conn
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let error = read_frame(&mut plugin.conn).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "No attach, credential, or media before approval"
        );
        stop.store(true, Ordering::Release);
        thread.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn unsigned_legacy_and_invalid_signed_plugins_are_refused_before_authorization() {
    for legacy in [true, false] {
        let dir = std::env::temp_dir().join(format!("nw-plugin-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let host = PluginHost::default();
        let key = nw_analytics_sdk::identity::SigningKey::from_bytes(&[3; 32]);
        let context = Context {
            managed: false,
            tickets: Default::default(),
            server_id: "test".into(),
            address: Address::Unix(path),
            node: NodeInfo {
                name: "Test".into(),
                version: "0".into(),
            },
            streams: Arc::new(StreamHubs::default()),
            events: Arc::new(EventHub::new(Arc::new(Mutex::new(
                crate::db::connection::open_in_memory().unwrap(),
            )))),
            status: Arc::clone(&host.status),
            overlays: Arc::clone(&host.overlays),
            approval: Some(nw_analytics_protocol::auth::Approval {
                public_key: nw_analytics_sdk::identity::public_key(&key),
                permissions: vec!["video".into(), "events".into(), "overlays".into()],
            }),
            config_path: dir.join("config.json"),
            queries: Arc::clone(&host.queries),
        };
        let desired = Arc::new(Mutex::new(Desired {
            bindings: vec![DesiredBinding {
                camera: nw_core::camera::CameraConfig::new(
                    "Private camera",
                    "rtsp://example.invalid",
                ),
                config: json!({}),
                stream: None,
                moving: false,
                own_decoder: false,
            }],
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || session::run(context, desired, stop))
        };
        let (conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut plugin = FakePlugin { conn };
        let Message::Request { id, params, .. } = plugin.message() else {
            panic!("hello expected");
        };
        let mut reply: nw_analytics_protocol::messages::Manifest =
            serde_json::from_value(manifest()).unwrap();
        if !legacy {
            nw_analytics_sdk::identity::sign_manifest(
                &mut reply,
                &key,
                params["challenge"].as_str().unwrap(),
                "",
            )
            .unwrap();
            reply.identity.as_mut().unwrap().signature = "00".repeat(64);
        }
        plugin.send(Message::reply(
            id,
            Ok::<_, nw_analytics_protocol::rpc::RpcError>(reply),
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        let detail = loop {
            if let Some(status) = host
                .status
                .lock()
                .unwrap()
                .get("test")
                .filter(|status| status.state == "error")
            {
                break status.detail.clone().unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "invalid identity must close the session"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            read_frame(&mut plugin.conn).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof,
            "neither credentials nor camera bindings may be sent"
        );
        stop.store(true, Ordering::Release);
        thread.join().unwrap();
        if legacy {
            assert!(detail.contains("does not support session authentication"));
            assert!(detail.contains("Update and restart"));
        } else {
            assert!(detail.contains("did not prove its session identity"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn a_plugin_is_offline_after_staying_unreachable_until_it_answers() {
    let host = PluginHost::default();
    let server = |id: &str, enabled: bool| super::PluginServer {
        id: id.into(),
        address: "unix:/x".into(),
        enabled,
        approval: None,
        credential_revision: 0,
        plugin_id: None,
        managed: None,
    };
    let servers = [server("motion", true), server("off", false)];
    host.set_server(
        "motion",
        "unix:/x",
        "error",
        Some("Could not connect".into()),
    );
    host.set_server("off", "unix:/x", "error", None);
    // Not yet: it may still be starting.
    assert!(host.offline(&servers, Duration::from_secs(60)).is_empty());
    let offline = host.offline(&servers, Duration::ZERO);
    assert_eq!(offline.len(), 1, "a plugin switched off is never offline");
    assert_eq!(offline[0].id, "motion");
    assert_eq!(
        offline[0].name, "motion",
        "unnamed until it introduced itself"
    );
    // A retry keeps it offline; answering again ends it.
    host.set_server("motion", "unix:/x", "connecting", None);
    assert_eq!(host.offline(&servers, Duration::ZERO).len(), 1);
    host.set_server("motion", "unix:/x", "authorizing", None);
    assert!(host.offline(&servers, Duration::ZERO).is_empty());
}

#[test]
fn desktop_stream_consumers_name_only_active_current_plugin_bindings() {
    use nw_analytics_protocol::messages::StreamChoice;
    use nw_core::camera::CameraConfig;
    let host = PluginHost::default();
    let config: super::PluginsConfig = serde_json::from_value(json!({
        "servers":[{"id":"fixture","address":"tcp:127.0.0.1:9999"}],
        "bindings":[{"server":"fixture","camera_id":"a"},
                    {"server":"fixture","camera_id":"b"},
                    {"server":"fixture","camera_id":"c","enabled":false}]
    }))
    .unwrap();
    let mut cameras: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|id| {
            let mut camera = CameraConfig::new(id, "rtsp://private:secret@example.invalid/main");
            camera.id = id.into();
            camera
        })
        .collect();
    cameras[0].sub_url = Some("rtsp://example.invalid/sub".into());
    let mut status = super::ServerStatus {
        id: "fixture".into(),
        state: "connected",
        address: config.servers[0].address.clone(),
        manifest: Some(serde_json::from_value(manifest()).unwrap()),
        ..Default::default()
    };
    for id in ["a", "b", "c", "removed"] {
        status.bindings.insert(
            id.into(),
            super::BindingView {
                state: "active",
                stream: Some(StreamChoice::Sub),
                ..Default::default()
            },
        );
    }
    host.status.lock().unwrap().insert("fixture".into(), status);
    {
        // Camera a: H.265, given the video wall's pictures; b: compressed only.
        let mut statuses = host.status.lock().unwrap();
        let a = statuses
            .get_mut("fixture")
            .unwrap()
            .bindings
            .get_mut("a")
            .unwrap();
        a.format = Some("H.265 1920×1080".into());
        a.shared_pictures = true;
        a.shared_fps = Some(9.5);
        a.plugin = Some(
            serde_json::from_value(json!({"binding": 1, "state": "running",
                "fps": 5.0, "frames": 10, "decoder": "VA-API (Intel, AMD)"}))
            .unwrap(),
        );
    }
    let rows = host.stream_consumers(&config, &cameras);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].codec, Some("h265"));
    assert_eq!(rows[0].pictures, super::PluginPictures::Shared);
    assert_eq!(rows[0].decoder.as_deref(), Some("VA-API (Intel, AMD)"));
    assert_eq!(rows[0].shared_fps, Some(9.5));
    assert_eq!(rows[1].shared_fps, None, "only while sharing");
    assert_eq!(
        (rows[1].codec, rows[1].pictures, rows[1].decoder.as_deref()),
        (None, super::PluginPictures::Compressed, None),
        "nothing known yet: compressed, codec and decoder left out"
    );
    // The portal's view: per stream, named, with counts for older portals.
    let mut streams = crate::streams::StreamHubs::default().diagnostics(&cameras);
    crate::streams::diagnostics::with_plugins(&mut streams, &rows);
    let paths = |camera: &str, stream: &str| {
        streams
            .iter()
            .find(|row| row.camera_id == camera && row.stream == stream)
            .and_then(|row| row.plugin_paths)
    };
    let counted =
        |compressed, shared| Some(crate::streams::diagnostics::PluginPaths { compressed, shared });
    assert_eq!(paths("a", "sub"), counted(0, 1));
    assert_eq!(paths("a", "main"), None, "no plugin on a's main stream");
    assert_eq!(paths("b", "main"), counted(1, 0));
    let wire = serde_json::to_string(&streams).unwrap();
    assert!(wire.contains(r#""plugin_paths":{"compressed":0,"shared":1}"#));
    assert!(
        wire.contains(
            r#""plugins":[{"name":"Test","pictures":"shared","decoder":"VA-API (Intel, AMD)","shared_fps":9.5}]"#
        ),
        "{wire}"
    );
    for forbidden in ["rtsp:", "127.0.0.1", "secret", "address", "manifest"] {
        assert!(!wire.contains(forbidden), "{forbidden} in {wire}");
    }
    // Plugin text is bounded and has no control characters.
    let mut long = crate::plugin_host::PluginStreamConsumer {
        camera_id: "b".into(),
        stream: "main",
        name: format!("x\n{}", "y".repeat(500)),
        codec: None,
        pictures: super::PluginPictures::Compressed,
        decoder: Some("  ".into()),
        shared_fps: None,
    };
    let mut bounded = crate::streams::StreamHubs::default().diagnostics(&cameras[1..2]);
    crate::streams::diagnostics::with_plugins(&mut bounded, std::slice::from_mut(&mut long));
    let plugin = &bounded[0].plugins[0];
    assert_eq!(plugin.name.chars().count(), 96);
    assert!(!plugin.name.contains('\n'));
    assert_eq!(plugin.decoder, None, "blank decoder text is left out");
    assert_eq!(
        (&rows[0].camera_id, rows[0].stream, rows[0].name.as_str()),
        (&"a".into(), "sub", "Test")
    );
    assert_eq!(
        rows[1].stream, "main",
        "Sub without its own URL consumes Main"
    );
    let wire = serde_json::to_string(&rows).unwrap();
    for forbidden in ["rtsp:", "127.0.0.1", "secret", "address", "manifest"] {
        assert!(
            !wire.contains(forbidden),
            "local diagnostic attribution contains only names and stream labels"
        );
    }
    cameras[0].sub_url = Some(cameras[0].url.clone());
    assert_eq!(
        host.stream_consumers(&config, &cameras)[0].stream,
        "main+sub"
    );
    host.status
        .lock()
        .unwrap()
        .get_mut("fixture")
        .unwrap()
        .bindings
        .get_mut("a")
        .unwrap()
        .state = "stream_unavailable";
    assert_eq!(host.stream_consumers(&config, &cameras).len(), 1);
    host.status
        .lock()
        .unwrap()
        .get_mut("fixture")
        .unwrap()
        .state = "error";
    assert!(host.stream_consumers(&config, &cameras).is_empty());
}

#[test]
fn a_bindings_own_decoder_choice_is_saved_only_when_set() {
    let config: super::PluginsConfig = serde_json::from_value(json!({
        "servers": [{"id": "fixture", "address": "tcp:127.0.0.1:9999"}],
        "bindings": [{"server": "fixture", "camera_id": "a", "own_decoder": true},
                     {"server": "fixture", "camera_id": "b"}]
    }))
    .unwrap();
    assert!(config.bindings[0].own_decoder);
    assert!(!config.bindings[1].own_decoder, "automatic unless set");
    let saved = serde_json::to_value(&config).unwrap();
    assert_eq!(saved["bindings"][0]["own_decoder"], true);
    assert!(saved["bindings"][1].get("own_decoder").is_none());
}

#[test]
fn a_hosted_plugin_gets_the_ticket_before_any_camera() {
    let dir = std::env::temp_dir().join(format!("nw-plugin-hosted-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("cloud-node")).unwrap();
    let path = dir.join("p.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let host = PluginHost::default();
    host.set_tickets(HashMap::from([(
        "hosted".to_owned(),
        "claims.signature".to_owned(),
    )]));
    let key = nw_analytics_sdk::identity::SigningKey::from_bytes(&[3; 32]);
    let node_key = crate::cloud::link::ProtectedKeyStore::open(dir.join("cloud-node/node.key"))
        .and_then(|store| store.load_or_generate())
        .unwrap();
    let context = Context {
        managed: true,
        tickets: Arc::clone(&host.tickets),
        server_id: "hosted".into(),
        address: Address::Unix(path),
        node: NodeInfo {
            name: "Test".into(),
            version: "0".into(),
        },
        streams: Arc::new(StreamHubs::default()),
        events: Arc::new(EventHub::new(Arc::new(Mutex::new(
            crate::db::connection::open_in_memory().unwrap(),
        )))),
        status: Arc::clone(&host.status),
        overlays: Arc::clone(&host.overlays),
        approval: Some(nw_analytics_protocol::auth::Approval {
            public_key: nw_analytics_sdk::identity::public_key(&key),
            permissions: vec!["video".into(), "events".into(), "overlays".into()],
        }),
        config_path: dir.join("config.json"),
        queries: Arc::clone(&host.queries),
    };
    let camera = nw_core::camera::CameraConfig::new("Camera", "rtsp://example.invalid");
    let desired = Arc::new(Mutex::new(Desired {
        bindings: vec![DesiredBinding {
            camera,
            config: json!({}),
            stream: None,
            moving: false,
            own_decoder: false,
        }],
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || session::run(context, desired, stop))
    };
    let (conn, _) = listener.accept().unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut plugin = FakePlugin { conn };
    let ok = |value: Value| Ok::<_, nw_analytics_protocol::rpc::RpcError>(value);

    let Message::Request { id, params, .. } = plugin.message() else {
        panic!("hello expected");
    };
    let mut signed = serde_json::from_value(manifest()).unwrap();
    nw_analytics_sdk::identity::sign_manifest(
        &mut signed,
        &key,
        params["challenge"].as_str().unwrap(),
        "",
    )
    .unwrap();
    plugin.send(Message::reply(
        id,
        ok(serde_json::to_value(signed).unwrap()),
    ));

    let Message::Request {
        id, method: name, ..
    } = plugin.message()
    else {
        panic!("authenticate expected");
    };
    assert_eq!(name, method::AUTHENTICATE);
    plugin.send(Message::reply(id, ok(json!({"authorized": true}))));

    let Message::Request {
        id,
        method: name,
        params,
    } = plugin.message()
    else {
        panic!("entitlement expected");
    };
    assert_eq!(name, method::ENTITLEMENT, "no camera before the ticket");
    assert_eq!(params["ticket"], "claims.signature");
    assert!(nw_service_protocol::verify_hex(
        &node_key.verifying_key(),
        &nw_service_protocol::entitlement_payload("claims.signature", b""),
        params["signature"].as_str().unwrap()
    ));
    plugin.send(Message::reply(
        id,
        ok(json!({"cameras": 8, "expires_at_ms": 1})),
    ));

    let Message::Request { method: name, .. } = plugin.message() else {
        panic!("attach expected");
    };
    assert_eq!(name, method::ATTACH);
    // Withdrawn by the portal: the session ends.
    host.set_tickets(HashMap::new());
    let ended = (0..80).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        host.status
            .lock()
            .unwrap()
            .get("hosted")
            .is_some_and(|status| {
                status
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("no longer provides"))
            })
    });
    assert!(ended);
    stop.store(true, Ordering::Release);
    drop(plugin);
    thread.join().unwrap();
}
