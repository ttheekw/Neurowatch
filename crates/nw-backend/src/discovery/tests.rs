//! The whole search → credentials → streams → camera flow against a fake
//! ONVIF device on loopback (UDP WS-Discovery + HTTP SOAP with digest auth).

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
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("udp");
    let http = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("tcp");
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
    assert_eq!(started.scope, "127.0.0.1/32");
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
