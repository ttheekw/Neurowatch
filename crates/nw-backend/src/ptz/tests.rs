use super::*;
use nw_core::camera::OnvifEndpoint;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn options_xml(timeout: &str) -> String {
    let xy="<tt:URI>urn:velocity:xy</tt:URI><tt:XRange><tt:Min>-2</tt:Min><tt:Max>2</tt:Max></tt:XRange><tt:YRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:YRange>";
    let z="<tt:URI>urn:velocity:z</tt:URI><tt:XRange><tt:Min>-1</tt:Min><tt:Max>1</tt:Max></tt:XRange>";
    format!("<Options><ContinuousPanTiltVelocitySpace>{xy}</ContinuousPanTiltVelocitySpace><ContinuousZoomVelocitySpace>{z}</ContinuousZoomVelocitySpace><RelativePanTiltTranslationSpace>{xy}</RelativePanTiltTranslationSpace><PTZTimeout><Min>{timeout}</Min><Max>PT10S</Max></PTZTimeout></Options>")
}
fn soap(body: &str) -> String {
    format!("<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:tt=\"http://www.onvif.org/ver10/schema\" xmlns:ter=\"http://www.onvif.org/ver10/error\"><s:Body>{body}</s:Body></s:Envelope>")
}
#[test]
fn ptz_parsing_advertises_only_safe_supported_axes_and_escapes_camera_values() {
    let options = parse::options(&soap(&options_xml("PT0.1S"))).unwrap();
    assert_eq!(options.pan.unwrap().x(-0.5), -1.0);
    assert!(options.zoom.is_some());
    assert!(
        parse::options(&soap(&options_xml("PT5S")))
            .unwrap()
            .pan
            .is_none(),
        "a camera requiring long timeouts cannot offer safe held controls"
    );
    assert!(
        parse::profile("<Profiles token='video'><VideoEncoderConfiguration/></Profiles>").is_err()
    );
    assert_eq!(parse::escape("<&\"'"), "&lt;&amp;&quot;&apos;");
    assert_eq!(
        parse::idle("<MoveStatus><PanTilt>IDLE</PanTilt><Zoom>MOVING</Zoom></MoveStatus>"),
        Some(false)
    );
    let presets =
        parse::presets("<r><Preset token='opaque&amp;1'><Name>Gate &amp; yard</Name></Preset></r>")
            .unwrap();
    assert_eq!(presets[0].token, "opaque&1");
    assert_eq!(presets[0].name, "Gate & yard");
}

#[derive(Default)]
struct DeviceState {
    fail_commands: bool,
    calls: Vec<(String, String)>,
    position: (f64, f64, f64),
    presets: HashMap<String, (String, (f64, f64, f64))>,
}
struct Device {
    camera: CameraConfig,
    state: Arc<Mutex<DeviceState>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Device {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn device() -> Device {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let shared = Arc::clone(&state);
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = socket.accept().await {
            let state = Arc::clone(&shared);
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0; 8192];
                let body_start = loop {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&data);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + length {
                            break end + 4;
                        }
                    }
                };
                let xml = String::from_utf8_lossy(&data[body_start..]);
                // This also asserts that namespace/action construction produces valid XML.
                let doc = roxmltree::Document::parse(&xml).unwrap();
                let operation = doc
                    .descendants()
                    .find(|n| n.is_element() && n.tag_name().name() == "Body")
                    .unwrap()
                    .children()
                    .find(|n| n.is_element())
                    .unwrap()
                    .tag_name()
                    .name();
                let response = {
                    let mut state = state.lock().unwrap();
                    state.calls.push((operation.into(), xml.to_string()));
                    match operation {
                        _ if state.fail_commands && matches!(operation, "SetPreset" | "RelativeMove") =>
                            "<s:Fault><s:Reason>Failed after accepting request</s:Reason></s:Fault>".into(),
                        "GetSystemDateAndTime"=>"<GetSystemDateAndTimeResponse/>".into(),
                        // Advertised host is deliberately wrong: the client must pin it.
                        "GetCapabilities"=>format!("<GetCapabilitiesResponse><Capabilities><Media><XAddr>http://192.0.2.8:{port}/media</XAddr></Media><PTZ><XAddr>http://192.0.2.8:{port}/ptz</XAddr></PTZ></Capabilities></GetCapabilitiesResponse>"),
                        "GetProfiles"=>"<GetProfilesResponse><Profiles token='video-only'/><Profiles token='main'><PTZConfiguration token='cfg'><NodeToken>node</NodeToken></PTZConfiguration></Profiles></GetProfilesResponse>".into(),
                        "GetConfigurationOptions"=>options_xml("PT0.1S"),
                        "GetNode"=>"<Node><HomeSupported>true</HomeSupported><MaximumNumberOfPresets>8</MaximumNumberOfPresets></Node>".into(),
                        "GetStatus"=>"<Status><MoveStatus><PanTilt>IDLE</PanTilt><Zoom>IDLE</Zoom></MoveStatus></Status>".into(),
                        "RelativeMove"=>{
                            let pos=doc.descendants().find(|n|n.is_element() && n.tag_name().name()=="PanTilt").unwrap();
                            state.position.0+=pos.attribute("x").unwrap().parse::<f64>().unwrap();
                            state.position.1+=pos.attribute("y").unwrap().parse::<f64>().unwrap();
                            "<RelativeMoveResponse/>".into()
                        }
                        "SetPreset"=>{
                            let token=parse::text(doc.root_element(),"PresetToken").unwrap_or_else(||format!("p{}",state.presets.len()+1));
                            let name=parse::text(doc.root_element(),"PresetName").unwrap();let position=state.position;
                            state.presets.insert(token.clone(),(name,position));format!("<SetPresetResponse><PresetToken>{token}</PresetToken></SetPresetResponse>")
                        }
                        "GetPresets"=>format!("<GetPresetsResponse>{}</GetPresetsResponse>",state.presets.iter().map(|(token,(name,_))|format!("<Preset token=\"{}\"><Name>{}</Name></Preset>",escape(token),escape(name))).collect::<String>()),
                        "RemovePreset"=>{state.presets.remove(&parse::text(doc.root_element(),"PresetToken").unwrap());"<RemovePresetResponse/>".into()}
                        "GotoPreset"=>{let token=parse::text(doc.root_element(),"PresetToken").unwrap();state.position=state.presets[&token].1;"<GotoPresetResponse/>".into()}
                        "GotoHomePosition"=>{state.position=(0.0,0.0,0.0);"<GotoHomePositionResponse/>".into()}
                        _=>format!("<{operation}Response/>")
                    }
                };
                let body = soap(&response);
                let status = if response.contains("<s:Fault>") {
                    500
                } else {
                    200
                };
                let reply=format!("HTTP/1.1 {status} OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    let mut camera = CameraConfig::new("Fake PTZ", "rtsp://127.0.0.1/fixture");
    camera.id = "ptz-fixture".into();
    camera.onvif = Some(OnvifEndpoint {
        device_url: format!("http://127.0.0.1:{port}/device"),
        events_url: None,
        ptz_url: None,
    });
    Device {
        camera,
        state,
        task,
    }
}
fn hub() -> Arc<EventHub> {
    Arc::new(EventHub::new(Arc::new(Mutex::new(
        crate::db::connection::open_in_memory().unwrap(),
    ))))
}
#[tokio::test]
async fn ptz_rejects_malformed_requests_without_opening_camera_connections() {
    let service = PtzService::default();
    let cam = CameraConfig::new("No network", "rtsp://example.invalid/never");
    let hub = hub();
    for (method, params) in [
        ("ptz.move", json!({"pan":2})),
        ("ptz.move", json!({"zoom":"1"})),
        ("ptz.save_preset", json!({"name":" "})),
        ("ptz.goto_preset", json!({"token":""})),
    ] {
        assert!(service
            .execute(&cam, Arc::clone(&hub), method, &params)
            .await
            .is_err());
    }
    assert!(service.sessions.lock().unwrap().is_empty());
}
#[tokio::test]
async fn ptz_lease_renewal_lost_client_and_foreign_stop_are_bounded() {
    let device = device().await;
    let service = PtzService::default();
    let hub = hub();
    let caps = service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.capabilities",
            &json!({}),
        )
        .await
        .unwrap();
    assert_eq!(caps["pan_tilt"], true);
    assert_eq!(caps["zoom"], true);
    assert_eq!(caps["home"], true);
    let motion = json!({"client_id":"pad","pan":0.5,"tilt":-0.25,"zoom":0.2});
    service
        .execute(&device.camera, Arc::clone(&hub), "ptz.move", &motion)
        .await
        .unwrap();
    assert!(hub.activity_paused(&device.camera.id));
    assert!(service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.stop",
            &json!({"client_id":"other"})
        )
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(600)).await;
    service
        .execute(&device.camera, Arc::clone(&hub), "ptz.move", &motion)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        device
            .state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(op, _)| op == "Stop")
            .count(),
        0,
        "old lease must not stop a renewed move"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    let state = device.state.lock().unwrap();
    assert_eq!(
        state.calls.iter().filter(|(op, _)| op == "Stop").count(),
        1,
        "lost client triggers explicit Stop"
    );
    let moves: Vec<_> = state
        .calls
        .iter()
        .filter(|(op, _)| op == "ContinuousMove")
        .collect();
    assert_eq!(moves.len(), 2);
    assert!(moves[0].1.contains("<tptz:Timeout>PT1.000S</tptz:Timeout>"));
    assert!(
        moves[0].1.contains("x=\"1\" y=\"-0.25\""),
        "normalized velocity uses advertised range"
    );
}
#[tokio::test]
async fn ptz_a_slow_client_may_hold_a_longer_lease_within_the_cameras_timeout() {
    let device = device().await;
    let service = PtzService::default();
    let hub = hub();
    for lease_ms in [json!(100), json!(3001), json!("2000"), json!(-1)] {
        let error = service
            .execute(
                &device.camera,
                Arc::clone(&hub),
                "ptz.move",
                &json!({"client_id":"web-1","pan":0.5,"lease_ms":lease_ms}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("900–3000 ms"), "{error}");
    }
    let answer = service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.move",
            &json!({"client_id":"web-1","pan":0.5,"lease_ms":2500}),
        )
        .await
        .unwrap();
    assert_eq!(answer["lease_ms"], 2500);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let stops = |device: &Device| {
        device
            .state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(op, _)| op == "Stop")
            .count()
    };
    assert_eq!(stops(&device), 0, "the lease outlives the default 900 ms");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(stops(&device), 1, "an unrenewed long lease still stops");
    let state = device.state.lock().unwrap();
    let held = state
        .calls
        .iter()
        .find(|(op, _)| op == "ContinuousMove")
        .unwrap();
    assert!(
        held.1.contains("<tptz:Timeout>PT2.500S</tptz:Timeout>"),
        "the camera stops by itself when the lease ends: {}",
        held.1
    );
    let short = parse::options(&soap(&options_xml("PT0.1S")).replace("PT10S", "PT1S")).unwrap();
    assert_eq!((short.timeout, short.longest), (1.0, 1.0));
}
#[tokio::test]
async fn ptz_presets_save_update_goto_delete_and_home_use_the_camera() {
    let device = device().await;
    let service = PtzService::default();
    let hub = hub();
    for (method, params) in [
        ("ptz.relative", json!({"pan":0.2,"tilt":-0.3})),
        ("ptz.save_preset", json!({"name":"Gate & yard"})),
    ] {
        service
            .execute(&device.camera, Arc::clone(&hub), method, &params)
            .await
            .unwrap();
    }
    let presets = service
        .execute(&device.camera, Arc::clone(&hub), "ptz.presets", &json!({}))
        .await
        .unwrap();
    assert_eq!(presets[0]["name"], "Gate & yard");
    let token = presets[0]["token"].as_str().unwrap();
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.save_preset",
            &json!({"name":"Renamed","token":token}),
        )
        .await
        .unwrap();
    service
        .execute(&device.camera, Arc::clone(&hub), "ptz.home", &json!({}))
        .await
        .unwrap();
    assert_eq!(device.state.lock().unwrap().position, (0.0, 0.0, 0.0));
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.goto_preset",
            &json!({"token":token}),
        )
        .await
        .unwrap();
    assert_eq!(device.state.lock().unwrap().position, (0.4, -0.3, 0.0));
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.delete_preset",
            &json!({"token":token}),
        )
        .await
        .unwrap();
    assert!(service
        .execute(&device.camera, Arc::clone(&hub), "ptz.presets", &json!({}))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    service.forget(&device.camera.id, Arc::clone(&hub));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(device
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|(op, _)| op == "Stop"));
}

#[tokio::test]
async fn ptz_does_not_replay_ambiguous_preset_failures() {
    let device = device().await;
    let service = PtzService::default();
    let hub = hub();
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.capabilities",
            &json!({}),
        )
        .await
        .unwrap();
    device.state.lock().unwrap().fail_commands = true;
    assert!(service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.save_preset",
            &json!({"name":"Only once"})
        )
        .await
        .is_err());
    assert_eq!(
        device
            .state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(op, _)| op == "SetPreset")
            .count(),
        1
    );
}
#[tokio::test]
async fn ptz_passive_close_does_not_stop_a_camera_and_explicit_stop_can_override_a_lease() {
    let device = device().await;
    let service = PtzService::default();
    let hub = hub();
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.capabilities",
            &json!({}),
        )
        .await
        .unwrap();
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.stop",
            &json!({"client_id":"idle-pad"}),
        )
        .await
        .unwrap();
    assert!(!device
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|(op, _)| op == "Stop"));
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.move",
            &json!({"client_id":"one","pan":0.2}),
        )
        .await
        .unwrap();
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.stop",
            &json!({"client_id":"two","force":true}),
        )
        .await
        .unwrap();
    let before = device
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(op, _)| op == "GetProfiles")
        .count();
    service
        .execute(
            &device.camera,
            Arc::clone(&hub),
            "ptz.capabilities",
            &json!({"refresh":true}),
        )
        .await
        .unwrap();
    assert_eq!(
        device
            .state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(op, _)| op == "GetProfiles")
            .count(),
        before + 1
    );
}

#[test]
fn ptz_settling_closes_motion_but_preserves_camera_alarm_inputs() {
    use crate::events::NewEvent;
    use nw_core::event::{EventKind, EventSource};
    let hub = hub();
    let movement = hub.start(NewEvent::new(
        Some("cam".into()),
        EventKind::Motion,
        EventSource::Builtin,
    ));
    let alarm = hub.start(NewEvent::new(
        Some("cam".into()),
        EventKind::DigitalInput,
        EventSource::Onvif,
    ));
    let other = hub.start(NewEvent::new(
        Some("other".into()),
        EventKind::Motion,
        EventSource::Onvif,
    ));
    let before = hub.activity_generation("cam");
    hub.pause_activity("cam", Duration::from_secs(2));
    let open = hub.open_events();
    assert!(!open.iter().any(|e| e.id == movement.id));
    assert!(open.iter().any(|e| e.id == alarm.id));
    assert!(open.iter().any(|e| e.id == other.id));
    assert!(hub.activity_generation("cam") > before);
    assert_eq!(hub.activity_generation("other"), 0);
}
