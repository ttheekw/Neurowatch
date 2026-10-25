//! The whole search → credentials → streams → camera flow against fake
//! devices on loopback: ONVIF (UDP WS-Discovery + HTTP SOAP with digest
//! auth), Hikvision SADP responders and RTSP servers. Loopback addresses
//! other than 127.0.0.1 (127.0.0.2, …) stand in for further devices.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct FakeDevice {
    udp_port: u16,
    http_port: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for FakeDevice {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// Does the request carry a valid digest for admin/secret?
fn authorized(request: &str) -> bool {
    let Some(user) = between(request, "<Username>", "</Username>") else {
        return false;
    };
    let password = between(request, "#PasswordDigest\">", "</Password>").unwrap_or_default();
    let nonce = between(request, "#Base64Binary\">", "</Nonce>").unwrap_or_default();
    let created = between(request, "<wsu:Created>", "</wsu:Created>").unwrap_or_default();
    let nonce = STANDARD.decode(nonce).unwrap_or_default();
    user == "admin" && password == onvif::password_digest(&nonce, created, "secret")
}

fn soap(body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

fn answer(request: &str, http_port: u16, requires_auth: bool) -> (u16, String) {
    if request.contains("GetSystemDateAndTime") {
        return (200, soap("<GetSystemDateAndTimeResponse><UTCDateTime><Time><Hour>1</Hour><Minute>2</Minute><Second>3</Second></Time><Date><Year>2026</Year><Month>10</Month><Day>2</Day></Date></UTCDateTime></GetSystemDateAndTimeResponse>"));
    }
    if requires_auth && !authorized(request) {
        return (400, soap("<s:Fault><s:Code><s:Subcode><s:Value>ter:NotAuthorized</s:Value></s:Subcode></s:Code></s:Fault>"));
    }
    if request.contains("GetDeviceInformation") {
        return (200, soap("<GetDeviceInformationResponse><Manufacturer>HIKVISION</Manufacturer><Model>DS-2CD2143G2-I</Model><FirmwareVersion>V5.7</FirmwareVersion></GetDeviceInformationResponse>"));
    }
    if request.contains("GetCapabilities") {
        return (200, soap(&format!("<GetCapabilitiesResponse><Capabilities><Media><XAddr>http://192.0.2.99:{http_port}/onvif/media</XAddr></Media></Capabilities></GetCapabilitiesResponse>")));
    }
    if request.contains("GetProfiles") {
        return (
            200,
            soap(concat!(
                "<GetProfilesResponse>",
                r#"<Profiles token="Profile_101"><Name>mainStream</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>2560</Width><Height>1440</Height></Resolution></VideoEncoderConfiguration></Profiles>"#,
                r#"<Profiles token="Profile_102"><Name>subStream</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>640</Width><Height>360</Height></Resolution></VideoEncoderConfiguration></Profiles>"#,
                "</GetProfilesResponse>"
            )),
        );
    }
    if request.contains("GetStreamUri") {
        let channel = if request.contains("Profile_102") {
            "102"
        } else {
            "101"
        };
        return (200, soap(&format!("<GetStreamUriResponse><MediaUri><Uri>rtsp://192.0.2.99:554/Streaming/Channels/{channel}</Uri></MediaUri></GetStreamUriResponse>")));
    }
    (500, soap("<s:Fault/>"))
}

async fn fake_device(requires_auth: bool) -> FakeDevice {
    fake_device_at(requires_auth, "127.0.0.1", 0).await
}

/// A fake ONVIF device answering WS-Discovery on `ip:udp_port` (0: any port).
async fn fake_device_at(requires_auth: bool, ip: &str, udp_port: u16) -> FakeDevice {
    let udp = tokio::net::UdpSocket::bind((ip, udp_port))
        .await
        .expect("udp");
    let http = tokio::net::TcpListener::bind((ip, 0)).await.expect("tcp");
    let udp_port = udp.local_addr().expect("addr").port();
    let http_port = http.local_addr().expect("addr").port();
    let probe_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65_536];
        while let Ok((len, from)) = udp.recv_from(&mut buf).await {
            let request = String::from_utf8_lossy(&buf[..len]).into_owned();
            let Some(id) = between(&request, "<w:MessageID>uuid:", "</w:MessageID>") else {
                continue;
            };
            let reply = format!(
                concat!(
                    r#"<?xml version="1.0"?><e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery">"#,
                    "<e:Header><a:RelatesTo>uuid:{id}</a:RelatesTo></e:Header><e:Body><d:ProbeMatches><d:ProbeMatch>",
                    "<a:EndpointReference><a:Address>urn:uuid:fake-camera</a:Address></a:EndpointReference>",
                    "<d:Types>dn:NetworkVideoTransmitter</d:Types>",
                    "<d:Scopes>onvif://www.onvif.org/name/Front%20Gate onvif://www.onvif.org/hardware/DS-2CD2143G2-I</d:Scopes>",
                    "<d:XAddrs>http://192.0.2.99:{port}/onvif/device_service</d:XAddrs>",
                    "</d:ProbeMatch></d:ProbeMatches></e:Body></e:Envelope>"
                ),
                id = id,
                port = http_port
            );
            let _ = udp.send_to(reply.as_bytes(), from).await;
        }
    });
    let http_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = http.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                let (status, body) =
                    answer(&String::from_utf8_lossy(&request), http_port, requires_auth);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    FakeDevice {
        udp_port,
        http_port,
        tasks: vec![probe_task, http_task],
    }
}

async fn finished(
    service: &DiscoveryService,
    scan_id: &str,
    cameras: &[CameraConfig],
) -> ScanResults {
    for _ in 0..200 {
        let results = service.results(scan_id, cameras).expect("results");
        if results.status != ScanStatus::Running {
            return results;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("search did not finish");
}

fn creds(password: &str) -> Option<Credentials> {
    Some(Credentials {
        username: "admin".into(),
        password: password.into(),
    })
}

#[tokio::test]
async fn a_search_finds_the_camera_then_its_streams_with_its_own_password() {
    let device = fake_device(true).await;
    let service = DiscoveryService::for_test(device.udp_port);
    let started = service
        .start(Scope::Cidr("127.0.0.1/32".into()), false)
        .expect("started");
    assert_eq!(started.scope, "127.0.0.1");
    assert_eq!(started.total, 1);
    // One search at a time.
    assert!(service
        .start(Scope::Cidr("127.0.0.1/32".into()), false)
        .is_err());

    let results = finished(&service, &started.scan_id, &[]).await;
    assert_eq!(results.status, ScanStatus::Complete);
    assert_eq!(results.candidates.len(), 1, "{results:?}");
    let candidate = &results.candidates[0];
    assert_eq!(candidate.address, "127.0.0.1");
    assert_eq!(candidate.state, CandidateState::CredentialsNeeded);
    assert_eq!(candidate.name.as_deref(), Some("Front Gate"));
    assert_eq!(candidate.model.as_deref(), Some("DS-2CD2143G2-I"));

    let wrong = service
        .profiles(&candidate.candidate_id, creds("guess"), false, &[])
        .await;
    assert_eq!(
        wrong.unwrap_err().0,
        "The camera refused that username and password."
    );

    let (shown, streams, main, sub) = service
        .profiles(&candidate.candidate_id, creds("secret"), false, &[])
        .await
        .expect("streams");
    assert_eq!(shown.manufacturer.as_deref(), Some("HIKVISION"));
    assert_eq!(streams.len(), 2);
    assert!(
        streams.iter().all(|s| s.url.is_none()),
        "no URLs unless asked"
    );
    assert_eq!(
        (streams[0].width, streams[0].height),
        (Some(2560), Some(1440))
    );
    assert_eq!((main.as_deref(), sub.as_deref()), (Some("s1"), Some("s2")));

    let camera = service
        .build_camera(&candidate.candidate_id, "s1", Some("s2"), None)
        .expect("camera");
    // Advertised 192.0.2.99 is replaced by the address that answered.
    assert_eq!(camera.url, "rtsp://127.0.0.1:554/Streaming/Channels/101");
    assert_eq!(
        camera.sub_url.as_deref(),
        Some("rtsp://127.0.0.1:554/Streaming/Channels/102")
    );
    assert_eq!(camera.name, "Front Gate");
    assert_eq!(camera.username.as_deref(), Some("admin"));
    assert_eq!(
        camera.password_b64.as_deref(),
        Some(STANDARD.encode("secret").as_str())
    );
    assert_eq!(camera.streams.len(), 2);
    assert_eq!(
        camera.discovery,
        Some(DiscoveryIdentity {
            endpoint_reference: "urn:uuid:fake-camera".into(),
            source: Some("Profile_101".into())
        })
    );
    // Where it answered ONVIF, for its events and PTZ later.
    assert!(camera
        .onvif
        .as_ref()
        .is_some_and(|onvif| onvif.device_url.starts_with("http://127.0.0.1")));

    // Once added, the next look at the same search says so.
    let cameras = vec![camera];
    let again = service
        .results(&started.scan_id, &cameras)
        .expect("results");
    assert_eq!(again.candidates[0].state, CandidateState::AlreadyAdded);
    assert_eq!(
        again.candidates[0]
            .matched_camera
            .as_ref()
            .map(|c| c.name.as_str()),
        Some("Front Gate")
    );

    service.forget_credentials(&candidate.candidate_id);
    let after = service
        .build_camera(&candidate.candidate_id, "s1", None, Some("Gate"))
        .expect("camera");
    assert_eq!(after.username, None);
}

#[tokio::test]
async fn an_open_camera_is_ready_and_recognised_from_its_streams() {
    let device = fake_device(false).await;
    let service = DiscoveryService::for_test(device.udp_port);
    let mut existing = CameraConfig::new(
        "Old entry",
        "rtsp://user:pw@127.0.0.1/Streaming/Channels/101",
    );
    existing.id = "old".into();
    let started = service
        .start(Scope::Cidr("127.0.0.1/32".into()), false)
        .expect("started");
    let results = finished(&service, &started.scan_id, &[]).await;
    assert_eq!(results.candidates[0].state, CandidateState::Ready);
    let matched = service
        .results(&started.scan_id, &[existing])
        .expect("results");
    assert_eq!(matched.candidates[0].state, CandidateState::AlreadyAdded);
}

#[tokio::test]
async fn a_typed_address_is_looked_up_and_a_shared_address_is_only_a_possible_match() {
    let device = fake_device(true).await;
    let service = DiscoveryService::for_test(device.udp_port);
    let mut manual = CameraConfig::new("Hand entered", "rtsp://127.0.0.1:8554/other");
    manual.id = "m".into();
    let candidate = service
        .lookup("127.0.0.1", Some(device.http_port), false, &[manual])
        .await
        .expect("candidate");
    assert_eq!(candidate.state, CandidateState::PossibleMatch);
    assert_eq!(candidate.matched_camera.map(|c| c.id), Some("m".into()));
    assert!(service.lookup("rtsp://x", None, false, &[]).await.is_err());
    assert!(
        service.lookup("8.8.8.8", None, true, &[]).await.is_err(),
        "portal stays private"
    );
}

#[tokio::test]
async fn a_search_can_be_cancelled_and_expired_ids_are_refused() {
    let device = fake_device(true).await;
    let service = DiscoveryService::for_test(device.udp_port);
    let started = service
        .start(Scope::Cidr("127.0.0.0/24".into()), false)
        .expect("started");
    service.cancel(&started.scan_id).expect("cancel");
    let results = finished(&service, &started.scan_id, &[]).await;
    assert_eq!(results.status, ScanStatus::Cancelled);
    assert!(service.results("nope", &[]).is_err());
    assert!(service.build_camera("nope", "s1", None, None).is_err());
    assert!(
        service
            .start(Scope::Cidr("10.0.0.0/16".into()), false)
            .is_err(),
        "bounded"
    );
}

#[tokio::test]
async fn a_refusal_with_the_right_password_is_not_reported_as_a_wrong_password() {
    let device = fake_device(true).await;
    let url = format!("http://127.0.0.1:{}/onvif/device_service", device.http_port);
    let client = SoapClient::new().expect("client");
    // The bare attempt is "not authorized"; the signed one reaches the
    // service, which does not know the action: that is the answer.
    let unknown = client
        .call(&url, "<tds:SomethingUnknown/>", creds("secret").as_ref())
        .await;
    assert!(
        matches!(unknown, Err(OnvifError::Invalid(_))),
        "{unknown:?}"
    );
    let wrong = client
        .call(&url, "<tds:SomethingUnknown/>", creds("nope").as_ref())
        .await;
    assert_eq!(wrong, Err(OnvifError::Unauthorized));
}

/// A port nothing listens on (TCP and UDP alike, as far as tests go).
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn sadp_reply(address: &str, serial: &str, http_port: u16, activated: bool) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?><ProbeMatch><Uuid>X</Uuid><Types>inquiry</Types>"#,
            "<DeviceType>139913</DeviceType><DeviceDescription>DS-2CD2143G2-I</DeviceDescription>",
            "<DeviceSN>{serial}</DeviceSN><CommandPort>8000</CommandPort><HttpPort>{http_port}</HttpPort>",
            "<MAC>bc-ba-c2-01-02-0{last}</MAC><IPv4Address>{address}</IPv4Address>",
            "<SoftwareVersion>V5.7.3build 220112</SoftwareVersion><Activated>{activated}</Activated></ProbeMatch>"
        ),
        serial = serial,
        http_port = http_port,
        last = address.len() % 10,
        address = address,
        activated = activated
    )
}

/// A SADP responder: every inquiry gets one reply per device, as a real
/// network of Hikvision devices answers.
struct FakeSadp {
    address: SocketAddr,
    inquiries: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeSadp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fake_sadp(replies: Vec<String>) -> FakeSadp {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("udp");
    let address = socket.local_addr().expect("addr");
    let inquiries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&inquiries);
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        while let Ok((len, from)) = socket.recv_from(&mut buf).await {
            let request = String::from_utf8_lossy(&buf[..len]).into_owned();
            if !request.contains("<Types>inquiry</Types>") || !request.contains("<Probe>") {
                continue;
            }
            seen.fetch_add(1, Ordering::SeqCst);
            for reply in &replies {
                let _ = socket.send_to(reply.as_bytes(), from).await;
            }
        }
    });
    FakeSadp {
        address,
        inquiries,
        task,
    }
}

/// An RTSP server on `ip:port` answering OPTIONS with `server` (if any).
async fn fake_rtsp(
    ip: &str,
    port: u16,
    server: Option<&'static str>,
) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind((ip, port))
        .await
        .expect("rtsp listen");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let server = server
                .map(|s| format!("Server: {s}\r\n"))
                .unwrap_or_default();
            let reply =
                format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\n{server}Public: OPTIONS, DESCRIBE\r\n\r\n");
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    })
}

fn ports(ws_discovery: u16) -> Ports {
    Ports {
        ws_discovery,
        sadp: None,
        sadp_listen: None,
        rtsp: None,
        onvif_http: closed_port(),
    }
}

fn by_address<'a>(results: &'a ScanResults, address: &str) -> &'a DiscoveryCandidate {
    results
        .candidates
        .iter()
        .find(|c| c.address == address)
        .unwrap_or_else(|| panic!("{address} not found in {results:?}"))
}

#[tokio::test]
async fn sadp_finds_hikvision_cameras_with_onvif_off_and_flags_unactivated_ones() {
    let http = closed_port();
    let sadp = fake_sadp(vec![
        sadp_reply(
            "127.0.0.1",
            "DS-2CD2143G2-I20220101AAWRJ00000001",
            http,
            true,
        ),
        sadp_reply(
            "127.0.0.2",
            "DS-2CD2143G2-I20220101AAWRJ00000002",
            http,
            false,
        ),
        // Outside the range typed (and no interface network to list it under).
        sadp_reply(
            "127.0.0.9",
            "DS-2CD2143G2-I20220101AAWRJ00000009",
            http,
            true,
        ),
        sadp_reply(
            "10.99.0.5",
            "DS-2CD2143G2-I20220101AAWRJ00000005",
            http,
            true,
        ),
    ])
    .await;
    let service = DiscoveryService::with_ports(Ports {
        sadp: Some(sadp.address),
        ..ports(closed_port())
    });
    let started = service
        .start(Scope::Cidr("127.0.0.1-2".into()), false)
        .expect("started");
    assert_eq!((started.scope.as_str(), started.total), ("127.0.0.1-2", 2));
    let results = finished(&service, &started.scan_id, &[]).await;
    assert!(sadp.inquiries.load(Ordering::SeqCst) >= 1);
    assert_eq!(results.candidates.len(), 2, "{results:?}");

    let camera = by_address(&results, "127.0.0.1");
    assert_eq!(camera.state, CandidateState::CredentialsNeeded);
    assert_eq!(camera.manufacturer.as_deref(), Some("Hikvision"));
    assert_eq!(camera.model.as_deref(), Some("DS-2CD2143G2-I"));
    assert_eq!(
        camera.serial.as_deref(),
        Some("DS-2CD2143G2-I20220101AAWRJ00000001")
    );
    assert!(camera.mac.is_some());
    assert_eq!(camera.found_by, ["sadp"]);
    assert_eq!(camera.activated, Some(true));
    assert!(!camera.stream_url_needed);
    assert!(
        camera
            .note
            .as_deref()
            .is_some_and(|n| n.contains("Hikvision stream addresses")),
        "{camera:?}"
    );

    let inactive = by_address(&results, "127.0.0.2");
    assert_eq!(inactive.state, CandidateState::Unavailable);
    assert_eq!(inactive.activated, Some(false));
    assert!(inactive
        .note
        .as_deref()
        .is_some_and(|n| n.starts_with("Not activated")));
    let refused = service
        .profiles(&inactive.candidate_id, creds("secret"), true, &[])
        .await
        .unwrap_err();
    assert!(refused.0.starts_with("Not activated"), "{refused}");

    // ONVIF off: Hikvision's stream paths, and the camera is added with them.
    let (_, streams, main, sub) = service
        .profiles(&camera.candidate_id, creds("secret"), true, &[])
        .await
        .expect("streams");
    assert_eq!(
        streams.iter().map(|s| s.url.as_deref()).collect::<Vec<_>>(),
        [
            Some("rtsp://127.0.0.1:554/Streaming/Channels/101"),
            Some("rtsp://127.0.0.1:554/Streaming/Channels/102")
        ]
    );
    let added = service
        .build_camera(
            &camera.candidate_id,
            main.as_deref().expect("main"),
            sub.as_deref(),
            None,
        )
        .expect("camera");
    assert_eq!(added.url, "rtsp://127.0.0.1:554/Streaming/Channels/101");
    assert_eq!(added.name, "Hikvision DS-2CD2143G2-I");
    assert_eq!(added.username.as_deref(), Some("admin"));
    assert_eq!(added.discovery, None);
}

#[tokio::test]
async fn a_camera_found_by_onvif_and_sadp_is_one_result_with_both_details() {
    let device = fake_device(false).await;
    let sadp = fake_sadp(vec![sadp_reply(
        "127.0.0.1",
        "DS-2CD2143G2-I20220101AAWRJ00000001",
        device.http_port,
        true,
    )])
    .await;
    let service = DiscoveryService::with_ports(Ports {
        sadp: Some(sadp.address),
        ..ports(device.udp_port)
    });
    let started = service
        .start(Scope::Cidr("127.0.0.1".into()), false)
        .expect("started");
    let results = finished(&service, &started.scan_id, &[]).await;
    assert_eq!(results.candidates.len(), 1, "{results:?}");
    let camera = &results.candidates[0];
    // ONVIF's answer wins; SADP adds what only it knows.
    assert_eq!(camera.name.as_deref(), Some("Front Gate"));
    assert_eq!(camera.manufacturer.as_deref(), Some("HIKVISION"));
    assert_eq!(camera.state, CandidateState::Ready);
    assert_eq!(
        camera.serial.as_deref(),
        Some("DS-2CD2143G2-I20220101AAWRJ00000001")
    );
    assert_eq!(camera.found_by, ["onvif", "sadp"]);
    assert_eq!(
        camera.endpoint_reference.as_deref(),
        Some("urn:uuid:fake-camera")
    );
    assert_eq!(camera.note, None);
}

#[tokio::test]
async fn the_rtsp_port_check_finds_devices_that_answer_nothing_else() {
    let rtsp_port = closed_port();
    let _dahua = fake_rtsp("127.0.0.1", rtsp_port, Some("Dahua Rtsp Server/3.0")).await;
    let _unknown = fake_rtsp("127.0.0.3", rtsp_port, None).await;
    // An ONVIF camera that also serves RTSP is listed once, as ONVIF.
    let onvif = fake_device_at(false, "127.0.0.4", 0).await;
    let _onvif_rtsp = fake_rtsp("127.0.0.4", rtsp_port, Some("Hikvision")).await;
    let service = DiscoveryService::with_ports(Ports {
        rtsp: Some(rtsp_port),
        ..ports(onvif.udp_port)
    });
    let started = service
        .start(Scope::Cidr("127.0.0.1-4".into()), false)
        .expect("started");
    assert_eq!(started.total, 4);
    let results = finished(&service, &started.scan_id, &[]).await;
    assert_eq!(results.status, ScanStatus::Complete);
    assert_eq!((results.probed, results.total), (4, 4));
    assert_eq!(results.candidates.len(), 3, "{results:?}");

    let dahua = by_address(&results, "127.0.0.1");
    assert_eq!(dahua.found_by, ["rtsp"]);
    assert_eq!(dahua.manufacturer.as_deref(), Some("Dahua"));
    assert_eq!(dahua.state, CandidateState::CredentialsNeeded);
    let (_, streams, _, _) = service
        .profiles(&dahua.candidate_id, creds("secret"), true, &[])
        .await
        .expect("streams");
    assert_eq!(
        streams[0].url.as_deref(),
        Some("rtsp://127.0.0.1:554/cam/realmonitor?channel=1&subtype=0")
    );

    let unknown = by_address(&results, "127.0.0.3");
    assert_eq!(unknown.found_by, ["rtsp"]);
    assert_eq!(unknown.manufacturer, None);
    assert_eq!(unknown.state, CandidateState::Unavailable);
    assert!(unknown.stream_url_needed);
    let refused = service
        .profiles(&unknown.candidate_id, creds("secret"), true, &[])
        .await
        .unwrap_err();
    assert!(refused.0.contains("stream URL"), "{refused}");

    let camera = by_address(&results, "127.0.0.4");
    assert_eq!(camera.found_by, ["onvif"]);
    assert_eq!(camera.name.as_deref(), Some("Front Gate"));
}

#[tokio::test]
async fn cameras_sharing_a_cloned_identity_are_listed_once_per_address() {
    let first = fake_device_at(false, "127.0.0.1", 0).await;
    let _second = fake_device_at(false, "127.0.0.2", first.udp_port).await;
    let service = DiscoveryService::for_test(first.udp_port);
    let started = service
        .start(Scope::Cidr("127.0.0.1-2".into()), false)
        .expect("started");
    let results = finished(&service, &started.scan_id, &[]).await;
    let mut addresses: Vec<_> = results
        .candidates
        .iter()
        .map(|c| c.address.as_str())
        .collect();
    addresses.sort_unstable();
    assert_eq!(addresses, ["127.0.0.1", "127.0.0.2"]);
    assert!(results
        .candidates
        .iter()
        .all(|c| c.endpoint_reference.as_deref() == Some("urn:uuid:fake-camera")));
}

#[test]
fn an_interface_search_probes_its_network_or_the_part_around_the_recorder() {
    let lan = interface_plan(Ipv4Addr::new(192, 168, 1, 20), 24);
    assert_eq!(lan.label, "192.168.1.0/24");
    assert_eq!(lan.unicast.count(), 254);
    assert_eq!(lan.multicast_from, Some(Ipv4Addr::new(192, 168, 1, 20)));

    let big = interface_plan(Ipv4Addr::new(10, 0, 5, 20), 16);
    assert_eq!(
        big.label,
        "10.0.5.0/24 (the part of 10.0.0.0/16 around this recorder)"
    );
    assert_eq!(big.unicast.count(), 254);
    assert!(big.unicast.contains(Ipv4Addr::new(10, 0, 5, 1)));
    // Answers from anywhere on the network are kept (multicast reaches it all).
    assert!(big.accept.contains(Ipv4Addr::new(10, 0, 200, 1)));

    let slash22 = interface_plan(Ipv4Addr::new(10, 0, 5, 20), 22);
    assert_eq!(slash22.unicast.count(), 1022);
}
