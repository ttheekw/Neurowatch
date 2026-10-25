//! Is a discovered device, or a camera being added, already configured?
//!
//! **Exact** — the same stored discovery identity (endpoint reference and
//! profile token, without conflicting literal IP hosts) or the same normalized
//! stream URL. **Possible** — only the
//! device identity or the network address matches: an NVR answers with one
//! endpoint reference for many channels, and one address can serve several
//! cameras, so neither alone proves it is the same stream.

use nw_core::camera::CameraConfig;
use nw_core::camera_discovery::{CameraRef, DiscoveryIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    Exact(CameraRef),
    Possible(CameraRef),
    None,
}

fn camera_ref(camera: &CameraConfig) -> CameraRef {
    CameraRef {
        id: camera.id.clone(),
        name: camera.name.clone(),
    }
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "rtsp" => Some(554),
        "rtsps" => Some(322),
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "rtmp" => Some(1935),
        _ => None,
    }
}

/// A stream URL reduced to what identifies the stream: no user info, lower
/// case scheme and host, no default port and no fragment; path and query
/// are kept as they are. Text that is not a URL (a capture device path)
/// is only trimmed.
pub fn canonical_url(raw: &str) -> String {
    let raw = raw.trim();
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_owned();
    };
    let scheme = scheme.to_ascii_lowercase();
    let rest = rest.split('#').next().unwrap_or_default();
    let (authority, path_query) = match rest.find(['/', '?']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    // User info ends at the last '@' (a password may itself contain '@').
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = split_host_port(host_port);
    let host = host.to_ascii_lowercase();
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    let port = port.filter(|p| Some(*p) != default_port(&scheme));
    let path_query = if path_query.is_empty() || path_query.starts_with('?') {
        format!("/{path_query}")
    } else {
        path_query.to_owned()
    };
    match port {
        Some(port) => format!("{scheme}://{host}:{port}{path_query}"),
        None => format!("{scheme}://{host}{path_query}"),
    }
}

fn split_host_port(host_port: &str) -> (&str, Option<u16>) {
    if let Some(rest) = host_port.strip_prefix('[') {
        // [IPv6]:port
        return match rest.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => (host_port, None),
        };
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host, Some(port)),
            Err(_) => (host_port, None),
        },
        _ => (host_port, None),
    }
}

/// The host of a stream URL, lower case, without user info or port.
pub fn url_host(raw: &str) -> Option<String> {
    let (_, rest) = raw.trim().split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = split_host_port(host_port).0.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn camera_urls(camera: &CameraConfig) -> impl Iterator<Item = &str> {
    std::iter::once(camera.url.as_str())
        .chain(camera.sub_url.as_deref())
        .chain(camera.streams.iter().map(|s| s.url.as_str()))
        .filter(|url| !url.trim().is_empty())
}

/// Cloned camera firmware can reuse both UUID and profile token. Distinct
/// literal IP hosts then take precedence over that identity. Hostnames may
/// alias an address, so they cannot establish this conflict without DNS.
fn different_literal_hosts(left: &str, right: &str) -> bool {
    let host_ip = |url| url_host(url).and_then(|host| host.parse::<std::net::IpAddr>().ok());
    matches!((host_ip(left), host_ip(right)), (Some(left), Some(right)) if left != right)
}

/// How a discovered device or stream relates to the configured cameras.
///
/// `identity` with a `source` is a specific stream; without one it is only
/// the device. `stream_url` is the stream that would be recorded.
pub fn match_camera(
    cameras: &[CameraConfig],
    identity: Option<&DiscoveryIdentity>,
    stream_url: Option<&str>,
    address: Option<&str>,
) -> Match {
    let stream_url = stream_url.map(canonical_url);
    for camera in cameras {
        let same_identity = identity.is_some_and(|wanted| {
            wanted.source.is_some()
                && camera.discovery.as_ref() == Some(wanted)
                && !stream_url
                    .as_deref()
                    .is_some_and(|url| different_literal_hosts(&camera.url, url))
        });
        let same_url = stream_url
            .as_deref()
            .is_some_and(|url| canonical_url(&camera.url) == url);
        if same_identity || same_url {
            return Match::Exact(camera_ref(camera));
        }
    }
    let address = address.map(str::to_ascii_lowercase);
    for camera in cameras {
        let same_device = identity.is_some_and(|wanted| {
            camera
                .discovery
                .as_ref()
                .is_some_and(|stored| stored.endpoint_reference == wanted.endpoint_reference)
        });
        let same_address = address.as_deref().is_some_and(|address| {
            camera_urls(camera).any(|url| url_host(url).as_deref() == Some(address))
        });
        if same_device || same_address {
            return Match::Possible(camera_ref(camera));
        }
    }
    Match::None
}

/// The configured camera `candidate` would duplicate, for the final check
/// in `camera.add`: the same non-conflicting discovery identity or the same
/// main stream.
pub fn duplicate_of<'a>(
    cameras: &'a [CameraConfig],
    candidate: &CameraConfig,
) -> Option<&'a CameraConfig> {
    let url = canonical_url(&candidate.url);
    cameras.iter().find(|camera| {
        camera.id != candidate.id
            && ((!url.is_empty() && canonical_url(&camera.url) == url)
                || (candidate
                    .discovery
                    .as_ref()
                    .is_some_and(|identity| identity.source.is_some())
                    && camera.discovery == candidate.discovery
                    && !different_literal_hosts(&camera.url, &candidate.url)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(id: &str, url: &str) -> CameraConfig {
        let mut camera = CameraConfig::new(id, url);
        camera.id = id.into();
        camera
    }

    fn identity(endpoint: &str, source: Option<&str>) -> DiscoveryIdentity {
        DiscoveryIdentity {
            endpoint_reference: endpoint.into(),
            source: source.map(Into::into),
        }
    }

    #[test]
    fn canonical_urls_ignore_credentials_case_default_port_and_fragment() {
        assert_eq!(
            canonical_url("RTSP://admin:p@ss@Cam.Local:554/Streaming/Channels/101?transport=tcp#x"),
            "rtsp://cam.local/Streaming/Channels/101?transport=tcp"
        );
        assert_eq!(
            canonical_url("rtsp://10.0.0.5:8554/live"),
            "rtsp://10.0.0.5:8554/live"
        );
        assert_eq!(canonical_url("rtsp://10.0.0.5"), "rtsp://10.0.0.5/");
        assert_eq!(
            canonical_url("rtsp://[FE80::1]:554/a"),
            "rtsp://[fe80::1]/a"
        );
        assert_eq!(canonical_url(" /dev/video0 "), "/dev/video0");
        // The path is case sensitive and kept.
        assert_ne!(
            canonical_url("rtsp://h/Live"),
            canonical_url("rtsp://h/live")
        );
    }

    #[test]
    fn the_same_stream_with_other_credentials_is_already_added() {
        let cameras = [camera(
            "a",
            "rtsp://user:old@10.0.0.5/Streaming/Channels/101",
        )];
        assert_eq!(
            match_camera(
                &cameras,
                None,
                Some("rtsp://10.0.0.5:554/Streaming/Channels/101"),
                None
            ),
            Match::Exact(CameraRef {
                id: "a".into(),
                name: "a".into()
            })
        );
    }

    #[test]
    fn a_shared_address_or_device_is_only_a_possible_match() {
        let mut nvr = camera("ch1", "rtsp://10.0.0.9/ch1");
        nvr.discovery = Some(identity("urn:uuid:nvr", Some("profile_ch1")));
        let cameras = [nvr];
        // Same NVR, another channel: possible, never exact.
        assert!(matches!(
            match_camera(
                &cameras,
                Some(&identity("urn:uuid:nvr", Some("profile_ch2"))),
                Some("rtsp://10.0.0.9/ch2"),
                None
            ),
            Match::Possible(_)
        ));
        // The device alone, before its streams are known.
        assert!(matches!(
            match_camera(
                &cameras,
                Some(&identity("urn:uuid:nvr", None)),
                None,
                Some("10.0.0.9")
            ),
            Match::Possible(_)
        ));
        // Same identity and channel: exact.
        assert!(matches!(
            match_camera(
                &cameras,
                Some(&identity("urn:uuid:nvr", Some("profile_ch1"))),
                None,
                None
            ),
            Match::Exact(_)
        ));
        // A hand-entered camera at the same address.
        let manual = [camera("m", "rtsp://admin:x@10.0.0.20/live")];
        assert!(matches!(
            match_camera(&manual, None, None, Some("10.0.0.20")),
            Match::Possible(_)
        ));
        assert_eq!(
            match_camera(&manual, None, None, Some("10.0.0.21")),
            Match::None
        );
    }

    #[test]
    fn duplicates_are_the_same_main_stream_or_the_same_stream_identity() {
        let mut first = camera("a", "rtsp://u:p@10.0.0.5/main");
        first.discovery = Some(identity("urn:uuid:cam", Some("main")));
        let cameras = [first];
        assert_eq!(
            duplicate_of(&cameras, &camera("b", "rtsp://10.0.0.5:554/main")).map(|c| c.id.as_str()),
            Some("a")
        );
        let mut same_identity = camera("c", "rtsp://other/url");
        same_identity.discovery = Some(identity("urn:uuid:cam", Some("main")));
        assert!(duplicate_of(&cameras, &same_identity).is_some());
        let mut other_channel = camera("d", "rtsp://10.0.0.5/sub");
        other_channel.discovery = Some(identity("urn:uuid:cam", Some("sub")));
        assert!(duplicate_of(&cameras, &other_channel).is_none());
        // Updating a camera is not a duplicate of itself.
        assert!(duplicate_of(&cameras, &camera("a", "rtsp://10.0.0.5/main")).is_none());
    }

    #[test]
    fn cloned_identities_at_different_addresses_remain_addable() {
        let mut first = camera("a", "rtsp://10.0.0.5/main");
        first.discovery = Some(identity("urn:uuid:cloned", Some("main")));
        let mut second = camera("b", "rtsp://10.0.0.6/main");
        second.discovery = first.discovery.clone();
        let cameras = [first];
        assert!(matches!(
            match_camera(
                &cameras,
                second.discovery.as_ref(),
                Some(&second.url),
                Some("10.0.0.6")
            ),
            Match::Possible(_)
        ));
        assert!(duplicate_of(&cameras, &second).is_none());
    }
}
