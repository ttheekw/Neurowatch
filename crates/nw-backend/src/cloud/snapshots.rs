//! Camera-provided JPEG snapshots for portal thumbnails.
//!
//! A thumbnail request never creates or keeps a video decoder. When a
//! desktop view is already decoding a camera, its frame is used (see
//! `previews`); otherwise the camera is asked for its own JPEG snapshot over
//! ONVIF (`GetSnapshotUri` for its smallest profile) and that JPEG is passed
//! on unchanged. A camera without one, or whose snapshot is too large for a
//! thumbnail, has none: the portal shows it as unavailable.
//!
//! Requests go only to the camera's own IPv4 address (the snapshot URL is
//! pinned to it, redirects are refused), with the camera's credentials, and
//! neither URLs nor credentials are logged.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use md5::{Digest as _, Md5};
use nw_core::camera::CameraConfig;

use crate::discovery::onvif::{self, Credentials, SoapClient};

/// Largest JPEG passed on: the portal accepts 120 000 base64 characters.
pub const MAX_SNAPSHOT_BYTES: usize = 88_000;
/// How long a found snapshot URL (or that there is none) is remembered.
const SOURCE_TTL: Duration = Duration::from_secs(600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Per camera: its stream URL when looked up, the snapshot URL (or none), and when.
type SnapshotSources = HashMap<String, (String, Option<String>, Instant)>;

#[derive(Default)]
pub struct CameraSnapshots {
    sources: Mutex<SnapshotSources>,
}

impl CameraSnapshots {
    /// The camera's own JPEG snapshot, base64, if it has one small enough.
    pub async fn fetch(&self, camera: &CameraConfig) -> Option<String> {
        let device = camera_address(&camera.url)?;
        let credentials = credentials(camera);
        let url = match self.cached(camera) {
            Some(known) => known?,
            None => {
                let found = snapshot_url(device, credentials.as_ref()).await;
                if let Ok(mut sources) = self.sources.lock() {
                    sources.insert(
                        camera.id.clone(),
                        (camera.url.clone(), found.clone(), Instant::now()),
                    );
                }
                found?
            }
        };
        let jpeg = get_jpeg(&url, credentials.as_ref()).await?;
        Some(STANDARD.encode(jpeg))
    }

    fn cached(&self, camera: &CameraConfig) -> Option<Option<String>> {
        let sources = self.sources.lock().ok()?;
        let (url, snapshot, at) = sources.get(&camera.id)?;
        (*url == camera.url && at.elapsed() < SOURCE_TTL).then(|| snapshot.clone())
    }
}

/// The camera's IPv4 address from its stream URL (LAN cameras only).
fn camera_address(url: &str) -> Option<Ipv4Addr> {
    let parsed = reqwest::Url::parse(url).ok()?;
    parsed.host_str()?.parse().ok()
}

fn credentials(camera: &CameraConfig) -> Option<Credentials> {
    let username = camera.username.clone().filter(|name| !name.is_empty())?;
    let password = camera
        .password_b64
        .as_deref()
        .and_then(|b64| STANDARD.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    Some(Credentials {
        username,
        password,
        allow_password_text: camera
            .onvif
            .as_ref()
            .is_some_and(|endpoint| endpoint.allow_password_text),
    })
}

/// Find the snapshot URL of the camera's smallest profile.
async fn snapshot_url(device: Ipv4Addr, credentials: Option<&Credentials>) -> Option<String> {
    let mut client = SoapClient::new().ok()?;
    for device_url in onvif::device_url_candidates(device, None) {
        client.sync_clock(&device_url).await;
        let media_url = match client
            .call(
                &device_url,
                "<tds:GetCapabilities><tds:Category>Media</tds:Category></tds:GetCapabilities>",
                credentials,
            )
            .await
        {
            Ok(reply) => {
                onvif::parse_media_url(&reply, device).unwrap_or_else(|| device_url.clone())
            }
            Err(_) => continue,
        };
        let Ok(reply) = client
            .call(&media_url, "<trt:GetProfiles/>", credentials)
            .await
        else {
            continue;
        };
        let mut profiles = onvif::parse_profiles(&reply).ok()?;
        // Smallest first: its snapshot is the one most likely to fit.
        profiles.sort_by_key(|profile| profile.width.unwrap_or(u32::MAX));
        for profile in profiles {
            if let Ok(reply) = client
                .call(
                    &media_url,
                    &onvif::snapshot_uri_body(&profile.token),
                    credentials,
                )
                .await
            {
                if let Some(url) = onvif::parse_snapshot_uri(&reply, device) {
                    return Some(url);
                }
            }
        }
        return None;
    }
    None
}

async fn get_jpeg(url: &str, credentials: Option<&Credentials>) -> Option<Vec<u8>> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let mut response = http.get(url).send().await.ok()?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        let credentials = credentials?;
        let challenge = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let request = http.get(url);
        response = if challenge.to_ascii_lowercase().starts_with("digest") {
            let path = reqwest::Url::parse(url).ok().map(|u| match u.query() {
                Some(query) => format!("{}?{query}", u.path()),
                None => u.path().to_owned(),
            })?;
            request
                .header(
                    reqwest::header::AUTHORIZATION,
                    digest_authorization(&challenge, credentials, "GET", &path)?,
                )
                .send()
                .await
                .ok()?
        } else {
            request
                .basic_auth(&credentials.username, Some(&credentials.password))
                .send()
                .await
                .ok()?
        };
    }
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length as usize > MAX_SNAPSHOT_BYTES)
    {
        return None;
    }
    let body = response.bytes().await.ok()?;
    (body.len() <= MAX_SNAPSHOT_BYTES && body.starts_with(&[0xFF, 0xD8])).then(|| body.to_vec())
}

/// RFC 7616 Digest (MD5, `qop=auth` or none) for one request.
fn digest_authorization(
    challenge: &str,
    credentials: &Credentials,
    method: &str,
    uri: &str,
) -> Option<String> {
    let field = |name: &str| -> Option<String> {
        let start = challenge.find(&format!("{name}="))? + name.len() + 1;
        let rest = &challenge[start..];
        Some(match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next()?.to_owned(),
            None => rest.split(',').next()?.trim().to_owned(),
        })
    };
    let algorithm = field("algorithm").unwrap_or_else(|| "MD5".into());
    if !algorithm.eq_ignore_ascii_case("MD5") {
        return None;
    }
    let md5 = |text: String| format!("{:x}", Md5::digest(text.as_bytes()));
    let realm = field("realm")?;
    let nonce = field("nonce")?;
    let ha1 = md5(format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    let ha2 = md5(format!("{method}:{uri}"));
    let qop_auth = field("qop").is_some_and(|qop| qop.split(',').any(|q| q.trim() == "auth"));
    let mut header = format!(
        "Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\"",
        credentials.username
    );
    if qop_auth {
        let cnonce = uuid::Uuid::new_v4().simple().to_string();
        let response = md5(format!("{ha1}:{nonce}:00000001:{cnonce}:auth:{ha2}"));
        header.push_str(&format!(
            ", qop=auth, nc=00000001, cnonce=\"{cnonce}\", response=\"{response}\""
        ));
    } else {
        header.push_str(&format!(
            ", response=\"{}\"",
            md5(format!("{ha1}:{nonce}:{ha2}"))
        ));
    }
    if let Some(opaque) = field("opaque") {
        header.push_str(&format!(", opaque=\"{opaque}\""));
    }
    Some(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_the_rfc_2617_example() {
        // RFC 2617 §3.5: Mufasa / "Circle Of Life" at testrealm@host.com.
        let challenge = r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#;
        let creds = Credentials {
            username: "Mufasa".into(),
            password: "Circle Of Life".into(),
            allow_password_text: false,
        };
        let header = digest_authorization(challenge, &creds, "GET", "/dir/index.html").unwrap();
        assert!(header.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
        assert!(header.contains("qop=auth, nc=00000001"));
        // The RFC's response uses a fixed cnonce; recompute ours the same way.
        let cnonce = header
            .split("cnonce=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let md5 = |text: String| format!("{:x}", Md5::digest(text.as_bytes()));
        let ha1 = md5("Mufasa:testrealm@host.com:Circle Of Life".into());
        assert_eq!(ha1, "939e7578ed9e3c518a452acee763bce9");
        let ha2 = md5("GET:/dir/index.html".into());
        let expected = md5(format!(
            "{ha1}:dcd98b7102dd2f0e8b11d0f600bfb0c093:00000001:{cnonce}:auth:{ha2}"
        ));
        assert!(header.contains(&format!("response=\"{expected}\"")));
        let with_rfc_cnonce = md5(format!(
            "{ha1}:dcd98b7102dd2f0e8b11d0f600bfb0c093:00000001:0a4f113b:auth:{ha2}"
        ));
        assert_eq!(with_rfc_cnonce, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn only_lan_ipv4_cameras_are_asked() {
        assert_eq!(
            camera_address("rtsp://admin@192.168.1.64:554/stream"),
            Some(Ipv4Addr::new(192, 168, 1, 64))
        );
        assert_eq!(camera_address("rtsp://camera.example.com/stream"), None);
        assert_eq!(camera_address("/dev/video0"), None);
    }

    #[test]
    fn snapshot_urls_are_pinned_to_the_camera() {
        let reply = r#"<Envelope><Body><GetSnapshotUriResponse><MediaUri><Uri>http://10.9.9.9:8080/snap.jpg?x=1</Uri></MediaUri></GetSnapshotUriResponse></Body></Envelope>"#;
        assert_eq!(
            onvif::parse_snapshot_uri(reply, Ipv4Addr::new(192, 168, 1, 64)).as_deref(),
            Some("http://192.168.1.64:8080/snap.jpg?x=1")
        );
        let rtsp = reply.replace("http://", "rtsp://");
        assert!(onvif::parse_snapshot_uri(&rtsp, Ipv4Addr::new(192, 168, 1, 64)).is_none());
    }

    #[tokio::test]
    async fn a_jpeg_is_passed_on_unchanged_and_an_oversized_one_is_refused() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for body in [
                vec![0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9],
                vec![0xFF; MAX_SNAPSHOT_BYTES + 1],
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await.unwrap();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let url = format!("http://{address}/snap.jpg");
        assert_eq!(
            get_jpeg(&url, None).await,
            Some(vec![0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9])
        );
        assert_eq!(get_jpeg(&url, None).await, None);
        server.await.unwrap();
    }

    /// Serves `replies` (status line, extra headers, body), one connection
    /// each; returns the address and what each request head said.
    async fn serve_sequence(
        replies: Vec<(&'static str, &'static str, Vec<u8>)>,
    ) -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let heads = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = heads.clone();
        tokio::spawn(async move {
            for (status, headers, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let n = socket.read(&mut request).await.unwrap();
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..n]).to_string());
                let head = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        (format!("http://{address}/snap.jpg?channel=1"), heads)
    }

    fn jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 9, 9, 0xFF, 0xD9]
    }

    fn creds() -> Credentials {
        Credentials {
            username: "admin".into(),
            password: "secret".into(),
            allow_password_text: false,
        }
    }

    #[tokio::test]
    async fn a_digest_challenge_is_answered_for_the_same_path_and_query() {
        let (url, heads) = serve_sequence(vec![
            (
                "401 Unauthorized",
                "WWW-Authenticate: Digest realm=\"cam\", nonce=\"abc123\", qop=\"auth\"\r\n",
                vec![],
            ),
            ("200 OK", "Content-Type: image/jpeg\r\n", jpeg()),
        ])
        .await;
        assert_eq!(get_jpeg(&url, Some(&creds())).await, Some(jpeg()));
        let heads = heads.lock().unwrap();
        assert!(!heads[0].to_ascii_lowercase().contains("authorization"));
        let second = heads[1].to_ascii_lowercase();
        assert!(second.contains("authorization: digest username=\"admin\""));
        assert!(second.contains("uri=\"/snap.jpg?channel=1\""), "{second}");
    }

    #[tokio::test]
    async fn a_basic_challenge_is_answered_with_basic_credentials() {
        let (url, heads) = serve_sequence(vec![
            (
                "401 Unauthorized",
                "WWW-Authenticate: Basic realm=\"cam\"\r\n",
                vec![],
            ),
            ("200 OK", "", jpeg()),
        ])
        .await;
        assert_eq!(get_jpeg(&url, Some(&creds())).await, Some(jpeg()));
        // base64("admin:secret")
        assert!(heads.lock().unwrap()[1].contains("YWRtaW46c2VjcmV0"));
    }

    #[tokio::test]
    async fn a_challenge_without_credentials_and_bad_replies_give_no_picture() {
        let (url, _) = serve_sequence(vec![("401 Unauthorized", "", vec![])]).await;
        assert_eq!(get_jpeg(&url, None).await, None);
        // A digest this node cannot compute (SHA-256) is not answered.
        let (url, _) = serve_sequence(vec![(
            "401 Unauthorized",
            "WWW-Authenticate: Digest realm=\"cam\", nonce=\"n\", algorithm=SHA-256\r\n",
            vec![],
        )])
        .await;
        assert_eq!(get_jpeg(&url, Some(&creds())).await, None);
        let (url, _) = serve_sequence(vec![("404 Not Found", "", vec![])]).await;
        assert_eq!(get_jpeg(&url, None).await, None);
        // A body that is not a JPEG.
        let (url, _) = serve_sequence(vec![("200 OK", "", b"<html>".to_vec())]).await;
        assert_eq!(get_jpeg(&url, None).await, None);
        // Unreachable.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/s.jpg", listener.local_addr().unwrap())
        };
        assert_eq!(get_jpeg(&closed, None).await, None);
    }

    #[test]
    fn digest_without_qop_answers_with_the_short_form_and_refuses_other_algorithms() {
        let challenge = r#"Digest realm=cam, nonce=n0nce, algorithm=md5"#;
        let header = digest_authorization(challenge, &creds(), "GET", "/s").unwrap();
        let md5 = |text: String| format!("{:x}", Md5::digest(text.as_bytes()));
        let ha1 = md5("admin:cam:secret".into());
        let ha2 = md5("GET:/s".into());
        assert!(header.contains(&format!(
            "response=\"{}\"",
            md5(format!("{ha1}:n0nce:{ha2}"))
        )));
        assert!(!header.contains("cnonce") && !header.contains("opaque"));
        assert!(digest_authorization(
            "Digest realm=cam, nonce=n, algorithm=SHA-256",
            &creds(),
            "GET",
            "/s"
        )
        .is_none());
        assert!(digest_authorization("Digest nonce=n", &creds(), "GET", "/s").is_none());
        assert!(digest_authorization("Digest realm=cam", &creds(), "GET", "/s").is_none());
    }

    fn camera(url: &str) -> CameraConfig {
        let mut camera = CameraConfig::new("Camera", url);
        camera.id = "cam-1".into();
        camera
    }

    #[test]
    fn credentials_come_from_the_camera_when_it_has_a_user() {
        let mut cam = camera("rtsp://192.168.1.64/s");
        assert!(credentials(&cam).is_none());
        cam.username = Some(String::new());
        assert!(credentials(&cam).is_none());
        cam.username = Some("admin".into());
        let none = credentials(&cam).unwrap();
        assert_eq!(
            (none.username.as_str(), none.password.as_str()),
            ("admin", "")
        );
        cam.password_b64 = Some(STANDARD.encode("p@ss"));
        assert_eq!(credentials(&cam).unwrap().password, "p@ss");
        cam.password_b64 = Some("%%not base64%%".into());
        assert_eq!(credentials(&cam).unwrap().password, "");
    }

    #[tokio::test]
    async fn a_remembered_snapshot_url_is_used_without_asking_the_camera_again() {
        let (url, _) = serve_sequence(vec![("200 OK", "", jpeg())]).await;
        let cam = camera("rtsp://127.0.0.1:554/stream");
        let snapshots = CameraSnapshots::default();
        assert_eq!(snapshots.cached(&cam), None, "nothing remembered yet");
        snapshots
            .sources
            .lock()
            .unwrap()
            .insert(cam.id.clone(), (cam.url.clone(), Some(url), Instant::now()));
        assert!(snapshots.cached(&cam).unwrap().is_some());
        assert_eq!(
            snapshots.fetch(&cam).await,
            Some(STANDARD.encode(jpeg())),
            "the picture is passed on as base64"
        );
        // Another stream address: the lookup is stale.
        let moved = camera("rtsp://127.0.0.1:554/other");
        assert_eq!(snapshots.cached(&moved), None);
    }

    #[tokio::test]
    async fn a_camera_known_to_have_no_snapshot_or_no_lan_address_gives_none() {
        let cam = camera("rtsp://127.0.0.1:554/stream");
        let snapshots = CameraSnapshots::default();
        snapshots
            .sources
            .lock()
            .unwrap()
            .insert(cam.id.clone(), (cam.url.clone(), None, Instant::now()));
        assert_eq!(snapshots.cached(&cam), Some(None));
        assert_eq!(snapshots.fetch(&cam).await, None);
        assert_eq!(
            snapshots.fetch(&camera("rtsp://cam.example.com/s")).await,
            None
        );
    }
}
