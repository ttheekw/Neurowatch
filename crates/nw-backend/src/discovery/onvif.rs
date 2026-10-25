//! ONVIF over SOAP: WS-Discovery probes and the few device and media calls
//! adding a camera needs.
//!
//! Nothing a device advertises is trusted as a destination. Follow-up HTTP
//! requests and stream URLs are pinned to the address the device answered
//! from, redirects are never followed, and every reply is size-capped.

use std::net::Ipv4Addr;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use sha1::{Digest, Sha1};

/// Largest WS-Discovery datagram or SOAP reply read.
pub const MAX_REPLY_BYTES: usize = 256 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
pub const WS_DISCOVERY_PORT: u16 = 3702;
pub const WS_DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

/// Device-service paths and ports tried when only an address is known.
const DEVICE_PATHS: &[&str] = &["/onvif/device_service", "/onvif/device"];
const FALLBACK_PORTS: &[u16] = &[80, 8080, 8899, 8000, 5000];
/// Hikvision's SDK port; an operator who types it means a Hikvision camera.
pub const HIKVISION_SDK_PORT: u16 = 8000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OnvifError {
    #[error("the camera asks for a username and password")]
    Unauthorized,
    #[error("ONVIF is turned off on this camera; enable ONVIF (Integration Protocol) in its web settings")]
    Disabled,
    #[error("the camera did not answer: {0}")]
    Unreachable(String),
    #[error("the camera's answer could not be read: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------------
// WS-Discovery
// ---------------------------------------------------------------------------

/// A Probe for ONVIF video devices (`dn:NetworkVideoTransmitter`) or, with
/// `device_type: false`, for any ONVIF device (`tds:Device`).
pub fn probe_message(message_id: &str, video_transmitter: bool) -> String {
    let types = if video_transmitter {
        r#"<d:Types xmlns:dn="http://www.onvif.org/ver10/network/wsdl">dn:NetworkVideoTransmitter</d:Types>"#
    } else {
        r#"<d:Types xmlns:tds="http://www.onvif.org/ver10/device/wsdl">tds:Device</d:Types>"#
    };
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope""#,
            r#" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing""#,
            r#" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery">"#,
            "<e:Header><w:MessageID>uuid:{id}</w:MessageID>",
            "<w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>",
            "<w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header>",
            "<e:Body><d:Probe>{types}</d:Probe></e:Body></e:Envelope>"
        ),
        id = message_id,
        types = types
    )
}

/// One device's answer to a Probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeMatch {
    pub endpoint_reference: Option<String>,
    /// Device-service URLs pinned to the responding address.
    pub device_urls: Vec<String>,
    pub name: Option<String>,
    pub hardware: Option<String>,
    pub manufacturer: Option<String>,
    pub video: bool,
}

fn local<'a>(node: &roxmltree::Node<'a, '_>) -> &'a str {
    node.tag_name().name()
}

fn text_of(root: roxmltree::Node, tag: &str) -> Option<String> {
    root.descendants()
        .find(|n| n.is_element() && local(n) == tag)
        .and_then(|n| n.text())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn parse_xml(bytes: &[u8]) -> Result<String, OnvifError> {
    if bytes.len() > MAX_REPLY_BYTES {
        return Err(OnvifError::Invalid("answer too large".into()));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| OnvifError::Invalid("not UTF-8".into()))
}

fn bounded(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a ProbeMatches reply from `from`. Replies that are not a probe
/// match, or that answer another probe than `message_id`, are ignored.
pub fn parse_probe_match(
    bytes: &[u8],
    from: Ipv4Addr,
    message_ids: &[String],
) -> Option<ProbeMatch> {
    let text = parse_xml(bytes).ok()?;
    let doc = roxmltree::Document::parse(&text).ok()?;
    let root = doc.root_element();
    root.descendants()
        .find(|n| n.is_element() && local(n) == "ProbeMatch")?;
    if let Some(relates) = text_of(root, "RelatesTo") {
        let relates = relates.trim_start_matches("uuid:");
        if !message_ids.iter().any(|id| id == relates) {
            return None;
        }
    }
    let endpoint_reference = root
        .descendants()
        .find(|n| n.is_element() && local(n) == "EndpointReference")
        .and_then(|epr| text_of(epr, "Address"))
        .map(|address| bounded(&address, 256));
    let types = text_of(root, "Types").unwrap_or_default();
    let scopes = text_of(root, "Scopes").unwrap_or_default();
    let mut found = ProbeMatch {
        endpoint_reference,
        device_urls: Vec::new(),
        name: None,
        hardware: None,
        manufacturer: None,
        video: types.contains("NetworkVideoTransmitter")
            || scopes.contains("onvif://www.onvif.org/type/video_encoder")
            || scopes.contains("onvif://www.onvif.org/type/Network_Video_Transmitter"),
    };
    for scope in scopes.split_whitespace() {
        let Some(rest) = scope.strip_prefix("onvif://www.onvif.org/") else {
            continue;
        };
        let (kind, value) = rest.split_once('/').unwrap_or((rest, ""));
        let value = bounded(&percent_decode(value), 128);
        if value.is_empty() {
            continue;
        }
        match kind.to_ascii_lowercase().as_str() {
            "name" => found.name.get_or_insert(value),
            "hardware" => found.hardware.get_or_insert(value),
            "mfr" | "manufacturer" => found.manufacturer.get_or_insert(value),
            _ => continue,
        };
    }
    for xaddr in text_of(root, "XAddrs")
        .unwrap_or_default()
        .split_whitespace()
    {
        if let Some(url) = pin_http_url(xaddr, from) {
            if !found.device_urls.contains(&url) {
                found.device_urls.push(url);
            }
        }
    }
    if found.device_urls.is_empty() {
        found.device_urls = device_url_candidates(from, None);
    }
    found.device_urls.truncate(4);
    Some(found)
}

/// `url` with its host replaced by `device`, if it is plain HTTP(S); the
/// port and path are kept. A device may advertise another address (a NAT,
/// a hostname, `0.0.0.0`), but follow-up requests only ever go to the
/// address that answered.
pub fn pin_http_url(url: &str, device: Ipv4Addr) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    pin_authority(&scheme, rest, device)
}

/// A stream URL from the device, pinned to `device` the same way, without
/// any credentials the device put in it.
pub fn pin_stream_url(url: &str, device: Ipv4Addr) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "rtsp" | "rtsps" | "http" | "https") {
        return None;
    }
    pin_authority(&scheme, rest, device)
}

fn pin_authority(scheme: &str, rest: &str, device: Ipv4Addr) -> Option<String> {
    if rest.chars().any(|c| c.is_control() || c.is_whitespace()) || rest.len() > 1024 {
        return None;
    }
    let (authority, path) = match rest.find(['/', '?']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let port = if host_port.starts_with('[') {
        host_port.rsplit_once("]:").map(|(_, p)| p)
    } else {
        host_port.rsplit_once(':').map(|(_, p)| p)
    }
    .and_then(|p| p.parse::<u16>().ok());
    let path = if path.is_empty() { "/" } else { path };
    Some(match port {
        Some(port) => format!("{scheme}://{device}:{port}{path}"),
        None => format!("{scheme}://{device}{path}"),
    })
}

/// Device-service URLs to try for a device known only by address.
pub fn device_url_candidates(device: Ipv4Addr, port: Option<u16>) -> Vec<String> {
    let mut ports: Vec<u16> = port.into_iter().collect();
    ports.extend(FALLBACK_PORTS.iter().filter(|p| Some(**p) != port));
    ports
        .into_iter()
        .flat_map(|port| {
            DEVICE_PATHS.iter().map(move |path| {
                if port == 80 {
                    format!("http://{device}{path}")
                } else {
                    format!("http://{device}:{port}{path}")
                }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SOAP
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoapVersion {
    V12,
    V11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    None,
    Digest,
    Text,
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// WS-Security UsernameToken digest: Base64(SHA1(nonce + created + password)).
pub fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    STANDARD.encode(hasher.finalize())
}

fn security_header(creds: &Credentials, auth: Auth, clock_offset: time::Duration) -> String {
    let mut nonce = [0u8; 16];
    let _ = getrandom::getrandom(&mut nonce);
    let created = (time::OffsetDateTime::now_utc() + clock_offset)
        .replace_nanosecond(0)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default();
    let profile =
        "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0";
    let password = match auth {
        Auth::Text => format!(
            r#"<Password Type="{profile}#PasswordText">{}</Password>"#,
            xml_escape(&creds.password)
        ),
        _ => format!(
            r#"<Password Type="{profile}#PasswordDigest">{}</Password>"#,
            password_digest(&nonce, &created, &creds.password)
        ),
    };
    format!(
        concat!(
            r#"<Security xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd""#,
            r#" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd" s:mustUnderstand="1">"#,
            "<UsernameToken><Username>{user}</Username>{password}",
            r#"<Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce}</Nonce>"#,
            "<wsu:Created>{created}</wsu:Created></UsernameToken></Security>"
        ),
        user = xml_escape(&creds.username),
        password = password,
        nonce = STANDARD.encode(nonce),
        created = created
    )
}

/// WS-Addressing for a call to a subscription: where it goes and the
/// reference parameters the device handed out with the subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addressing {
    pub to: String,
    /// Already serialised elements, each declaring its own namespace.
    pub reference_parameters: String,
}

fn addressing_header(addressing: &Addressing, action: Option<&str>) -> String {
    let mut id = [0u8; 16];
    let _ = getrandom::getrandom(&mut id);
    let message_id: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let action = action
        .map(|a| format!("<wsa:Action>{}</wsa:Action>", xml_escape(a)))
        .unwrap_or_default();
    format!(
        "{action}<wsa:MessageID>urn:uuid:{message_id}</wsa:MessageID><wsa:To>{to}</wsa:To>{params}",
        to = xml_escape(&addressing.to),
        params = addressing.reference_parameters,
    )
}

fn envelope(version: SoapVersion, header: &str, body: &str) -> String {
    let ns = match version {
        SoapVersion::V12 => "http://www.w3.org/2003/05/soap-envelope",
        SoapVersion::V11 => "http://schemas.xmlsoap.org/soap/envelope/",
    };
    let header = if header.is_empty() {
        String::new()
    } else {
        format!("<s:Header>{header}</s:Header>")
    };
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="{ns}""#,
            r#" xmlns:tt="http://www.onvif.org/ver10/schema""#,
            r#" xmlns:tds="http://www.onvif.org/ver10/device/wsdl""#,
            r#" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl""#,
            r#" xmlns:trt="http://www.onvif.org/ver10/media/wsdl""#,
            r#" xmlns:tr2="http://www.onvif.org/ver20/media/wsdl""#,
            r#" xmlns:tev="http://www.onvif.org/ver10/events/wsdl""#,
            r#" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2""#,
            r#" xmlns:wsa="http://www.w3.org/2005/08/addressing">{header}<s:Body>{body}</s:Body></s:Envelope>"#
        ),
        ns = ns,
        header = header,
        body = body
    )
}

fn soap_action(body: &str) -> Option<&'static str> {
    [
        ("tptz:GetConfigurationOptions", "http://www.onvif.org/ver20/ptz/wsdl/GetConfigurationOptions"),
        ("tptz:GetNode", "http://www.onvif.org/ver20/ptz/wsdl/GetNode"),
        ("tptz:ContinuousMove", "http://www.onvif.org/ver20/ptz/wsdl/ContinuousMove"),
        ("tptz:RelativeMove", "http://www.onvif.org/ver20/ptz/wsdl/RelativeMove"),
        ("tptz:Stop", "http://www.onvif.org/ver20/ptz/wsdl/Stop"),
        ("tptz:GetPresets", "http://www.onvif.org/ver20/ptz/wsdl/GetPresets"),
        ("tptz:GotoPreset", "http://www.onvif.org/ver20/ptz/wsdl/GotoPreset"),
        ("tptz:SetPreset", "http://www.onvif.org/ver20/ptz/wsdl/SetPreset"),
        ("tptz:RemovePreset", "http://www.onvif.org/ver20/ptz/wsdl/RemovePreset"),
        ("tptz:GotoHomePosition", "http://www.onvif.org/ver20/ptz/wsdl/GotoHomePosition"),
        ("tptz:GetStatus", "http://www.onvif.org/ver20/ptz/wsdl/GetStatus"),
        (
            "GetSystemDateAndTime",
            "http://www.onvif.org/ver10/device/wsdl/GetSystemDateAndTime",
        ),
        (
            "GetDeviceInformation",
            "http://www.onvif.org/ver10/device/wsdl/GetDeviceInformation",
        ),
        (
            "GetCapabilities",
            "http://www.onvif.org/ver10/device/wsdl/GetCapabilities",
        ),
        (
            "GetProfiles",
            "http://www.onvif.org/ver10/media/wsdl/GetProfiles",
        ),
        (
            "GetStreamUri",
            "http://www.onvif.org/ver10/media/wsdl/GetStreamUri",
        ),
        (
            "GetSnapshotUri",
            "http://www.onvif.org/ver10/media/wsdl/GetSnapshotUri",
        ),
        (
            "CreatePullPointSubscription",
            "http://www.onvif.org/ver10/events/wsdl/EventPortType/CreatePullPointSubscriptionRequest",
        ),
        (
            "PullMessages",
            "http://www.onvif.org/ver10/events/wsdl/PullPointSubscription/PullMessagesRequest",
        ),
        (
            "GetEventProperties",
            "http://www.onvif.org/ver10/events/wsdl/EventPortType/GetEventPropertiesRequest",
        ),
        (
            "wsnt:Subscribe",
            "http://docs.oasis-open.org/wsn/bw-2/NotificationProducer/SubscribeRequest",
        ),
        (
            "wsnt:Renew",
            "http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/RenewRequest",
        ),
        (
            "wsnt:Unsubscribe",
            "http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/UnsubscribeRequest",
        ),
    ]
    .into_iter()
    .find(|(tag, _)| body.contains(tag))
    .map(|(_, action)| action)
}

/// Is a SOAP fault or HTTP status an authentication refusal?
fn is_auth_failure(status: u16, body: &str) -> bool {
    status == 401
        || [
            "NotAuthorized",
            "FailedAuthentication",
            "Sender not Authorized",
            "not authorized",
        ]
        .iter()
        .any(|marker| body.contains(marker))
}

/// A small ONVIF SOAP client: one device, no redirects, capped replies.
pub struct SoapClient {
    http: reqwest::Client,
    clock_offset: time::Duration,
    /// The SOAP version and authentication the device accepted last, tried
    /// first from then on (a subscription polls every few seconds).
    accepted: std::sync::Mutex<Option<(SoapVersion, Auth)>>,
}

/// Extras for one call: WS-Addressing and a longer wait for the answer
/// (a pull that the device holds open until events arrive).
#[derive(Debug, Clone, Default)]
pub struct CallOptions {
    pub addressing: Option<Addressing>,
    pub timeout: Option<Duration>,
}

impl SoapClient {
    pub fn new() -> Result<Self, OnvifError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| OnvifError::Unreachable(e.to_string()))?;
        Ok(Self {
            http,
            clock_offset: time::Duration::ZERO,
            accepted: std::sync::Mutex::new(None),
        })
    }

    async fn post(
        &self,
        url: &str,
        version: SoapVersion,
        xml: String,
        action: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(u16, String), OnvifError> {
        let content_type = match (version, action) {
            (SoapVersion::V12, Some(action)) => {
                format!(r#"application/soap+xml; charset=utf-8; action="{action}""#)
            }
            (SoapVersion::V12, None) => "application/soap+xml; charset=utf-8".to_owned(),
            (SoapVersion::V11, _) => "text/xml; charset=utf-8".to_owned(),
        };
        let mut request = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(xml);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if version == SoapVersion::V11 {
            if let Some(action) = action {
                request = request.header("SOAPAction", format!("\"{action}\""));
            }
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| OnvifError::Unreachable(without_url(&e)))?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| OnvifError::Unreachable(without_url(&e)))?
        {
            if body.len() + chunk.len() > MAX_REPLY_BYTES {
                return Err(OnvifError::Invalid("answer too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }

    /// Call `body` at `url`, trying SOAP 1.2 then 1.1, and no authentication,
    /// then a password digest, then a plain password when credentials are
    /// given. Returns the reply XML.
    pub async fn call(
        &self,
        url: &str,
        body: &str,
        creds: Option<&Credentials>,
    ) -> Result<String, OnvifError> {
        self.call_with(url, body, creds, &CallOptions::default())
            .await
    }

    /// [`Self::call`] with WS-Addressing or a longer timeout.
    pub async fn call_with(
        &self,
        url: &str,
        body: &str,
        creds: Option<&Credentials>,
        options: &CallOptions,
    ) -> Result<String, OnvifError> {
        self.call_strategy(url, body, creds, options, true).await
    }

    /// Mutating camera actions may be repeated only after an explicit auth or
    /// SOAP-version refusal. Never replay an ambiguously failed move/preset.
    pub async fn call_command(
        &self,
        url: &str,
        body: &str,
        creds: Option<&Credentials>,
    ) -> Result<String, OnvifError> {
        self.call_strategy(url, body, creds, &CallOptions::default(), false)
            .await
    }

    async fn call_strategy(
        &self,
        url: &str,
        body: &str,
        creds: Option<&Credentials>,
        options: &CallOptions,
        retry_refusals: bool,
    ) -> Result<String, OnvifError> {
        let auths: &[Auth] = match creds {
            Some(c) if !c.username.is_empty() => &[Auth::None, Auth::Digest, Auth::Text],
            _ => &[Auth::None],
        };
        let mut attempts: Vec<(SoapVersion, Auth)> = [SoapVersion::V12, SoapVersion::V11]
            .into_iter()
            .flat_map(|version| auths.iter().map(move |auth| (version, *auth)))
            .collect();
        let accepted = self.accepted.lock().ok().and_then(|accepted| *accepted);
        if let Some(index) = accepted.and_then(|a| attempts.iter().position(|b| *b == a)) {
            let first = attempts.remove(index);
            attempts.insert(0, first);
        }
        let action = soap_action(body);
        let mut last = OnvifError::Unreachable("no answer".into());
        let mut unauthorized = false;
        // A refusal the device gave a request carrying the credentials (or to
        // any request, without credentials): the real answer, which an
        // earlier "not authorized" to a bare request must not hide.
        let mut refusal: Option<OnvifError> = None;
        for (version, auth) in attempts {
            let mut header = match (creds, auth) {
                (Some(creds), Auth::Digest | Auth::Text) => {
                    security_header(creds, auth, self.clock_offset)
                }
                _ => String::new(),
            };
            if let Some(addressing) = &options.addressing {
                header.push_str(&addressing_header(addressing, action));
            }
            let xml = envelope(version, &header, body);
            match self.post(url, version, xml, action, options.timeout).await {
                Ok((status, reply))
                    if (200..300).contains(&status) && !reply.contains(":Fault>") =>
                {
                    if let Ok(mut remembered) = self.accepted.lock() {
                        *remembered = Some((version, auth));
                    }
                    return Ok(reply);
                }
                Ok((_, reply)) if reply.contains("ONVIF integrate function is disabled") => {
                    return Err(OnvifError::Disabled);
                }
                Ok((status, reply)) if is_auth_failure(status, &reply) => {
                    unauthorized = true;
                    last = OnvifError::Unauthorized;
                }
                Ok((status, ref reply))
                    if !retry_refusals && (status == 415 || reply.contains("VersionMismatch")) =>
                {
                    last = OnvifError::Invalid(format!("HTTP {status}"));
                }
                Ok((status, _)) => {
                    let error = OnvifError::Invalid(format!("HTTP {status}"));
                    if !retry_refusals {
                        return Err(error);
                    }
                    if auth != Auth::None || auths.len() == 1 {
                        refusal = Some(error.clone());
                    }
                    last = error;
                }
                // Unreachable is the same for every variant.
                Err(error @ OnvifError::Unreachable(_)) => return Err(error),
                Err(error) => {
                    if !retry_refusals {
                        return Err(error);
                    }
                    last = error;
                }
            }
        }
        Err(refusal.unwrap_or(if unauthorized {
            OnvifError::Unauthorized
        } else {
            last
        }))
    }

    /// Learn the device clock, so digests carry a time the device accepts
    /// even if its clock is wrong. Best effort.
    pub async fn sync_clock(&mut self, device_url: &str) {
        let Ok(reply) = self
            .call(device_url, "<tds:GetSystemDateAndTime/>", None)
            .await
        else {
            return;
        };
        if let Some(device_now) = parse_device_time(&reply) {
            self.clock_offset = device_now - time::OffsetDateTime::now_utc();
        }
    }
}

fn without_url(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "connection refused or unreachable".into()
    } else {
        "request failed".into()
    }
}

fn parse_device_time(reply: &str) -> Option<time::OffsetDateTime> {
    let doc = roxmltree::Document::parse(reply).ok()?;
    let utc = doc
        .descendants()
        .find(|n| n.is_element() && local(n) == "UTCDateTime")?;
    let number = |tag: &str| text_of(utc, tag)?.parse::<i32>().ok();
    let date = time::Date::from_calendar_date(
        number("Year")?,
        time::Month::try_from(u8::try_from(number("Month")?).ok()?).ok()?,
        u8::try_from(number("Day")?).ok()?,
    )
    .ok()?;
    let clock = time::Time::from_hms(
        u8::try_from(number("Hour")?).ok()?,
        u8::try_from(number("Minute")?).ok()?,
        u8::try_from(number("Second")?).ok()?,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, clock).assume_utc())
}

// ---------------------------------------------------------------------------
// Device and media calls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
}

pub fn parse_device_information(reply: &str) -> Result<DeviceInfo, OnvifError> {
    let doc = roxmltree::Document::parse(reply).map_err(|e| OnvifError::Invalid(e.to_string()))?;
    let root = doc.root_element();
    let field = |tag: &str| text_of(root, tag).map(|t| bounded(&t, 64));
    Ok(DeviceInfo {
        manufacturer: field("Manufacturer"),
        model: field("Model"),
        firmware: field("FirmwareVersion"),
    })
}

/// The media service URL from GetCapabilities, pinned to `device`.
pub fn parse_media_url(reply: &str, device: Ipv4Addr) -> Option<String> {
    let doc = roxmltree::Document::parse(reply).ok()?;
    let media = doc
        .descendants()
        .find(|n| n.is_element() && local(n) == "Media")?;
    pin_http_url(&text_of(media, "XAddr")?, device)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaProfile {
    pub token: String,
    pub name: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub encoding: Option<String>,
}

pub fn parse_profiles(reply: &str) -> Result<Vec<MediaProfile>, OnvifError> {
    let doc = roxmltree::Document::parse(reply).map_err(|e| OnvifError::Invalid(e.to_string()))?;
    let profiles: Vec<MediaProfile> = doc
        .descendants()
        .filter(|n| n.is_element() && local(n) == "Profiles")
        .filter_map(|profile| {
            let token = bounded(profile.attribute("token")?, 64);
            let video = profile
                .descendants()
                .find(|n| n.is_element() && local(n) == "VideoEncoderConfiguration")
                .unwrap_or(profile);
            let number = |tag: &str| {
                text_of(video, tag)
                    .and_then(|t| t.parse::<u32>().ok())
                    .filter(|n| *n > 0)
            };
            Some(MediaProfile {
                name: text_of(profile, "Name")
                    .map(|n| bounded(&n, 64))
                    .unwrap_or_else(|| token.clone()),
                width: number("Width"),
                height: number("Height"),
                encoding: text_of(video, "Encoding").map(|e| bounded(&e, 16)),
                token,
            })
        })
        .take(32)
        .collect();
    if profiles.is_empty() {
        Err(OnvifError::Invalid(
            "the camera reported no media profiles".into(),
        ))
    } else {
        Ok(profiles)
    }
}

pub fn stream_uri_body(token: &str) -> String {
    format!(
        concat!(
            "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream>",
            "<tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup>",
            "<trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>"
        ),
        xml_escape(token)
    )
}

/// The camera's JPEG snapshot for one media profile.
pub fn snapshot_uri_body(token: &str) -> String {
    format!(
        "<trt:GetSnapshotUri><trt:ProfileToken>{}</trt:ProfileToken></trt:GetSnapshotUri>",
        xml_escape(token)
    )
}

/// The snapshot URL from GetSnapshotUri, pinned to `device` (HTTP(S) only).
pub fn parse_snapshot_uri(reply: &str, device: Ipv4Addr) -> Option<String> {
    let doc = roxmltree::Document::parse(reply).ok()?;
    pin_http_url(&text_of(doc.root_element(), "Uri")?, device)
}

pub fn parse_stream_uri(reply: &str, device: Ipv4Addr) -> Option<String> {
    let doc = roxmltree::Document::parse(reply).ok()?;
    pin_stream_url(&text_of(doc.root_element(), "Uri")?, device)
}

/// Hikvision stream paths, for a Hikvision camera whose ONVIF is turned off.
pub fn hikvision_profiles(device: Ipv4Addr) -> Vec<(MediaProfile, String)> {
    [("101", "Main stream"), ("102", "Sub stream")]
        .into_iter()
        .map(|(channel, name)| {
            (
                MediaProfile {
                    token: format!("hikvision-{channel}"),
                    name: name.to_owned(),
                    width: None,
                    height: None,
                    encoding: None,
                },
                format!("rtsp://{device}:554/Streaming/Channels/{channel}"),
            )
        })
        .collect()
}

/// Dahua stream paths (also many OEM brands built on Dahua), for a Dahua
/// camera whose ONVIF is turned off.
pub fn dahua_profiles(device: Ipv4Addr) -> Vec<(MediaProfile, String)> {
    [("0", "Main stream"), ("1", "Sub stream")]
        .into_iter()
        .map(|(subtype, name)| {
            (
                MediaProfile {
                    token: format!("dahua-{subtype}"),
                    name: name.to_owned(),
                    width: None,
                    height: None,
                    encoding: None,
                },
                format!("rtsp://{device}:554/cam/realmonitor?channel=1&subtype={subtype}"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: Ipv4Addr = Ipv4Addr::new(10, 0, 10, 23);

    fn probe_match(relates: &str, xaddrs: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<SOAP-ENV:Header><wsa:RelatesTo>uuid:{relates}</wsa:RelatesTo></SOAP-ENV:Header>
<SOAP-ENV:Body><d:ProbeMatches><d:ProbeMatch>
<wsa:EndpointReference><wsa:Address>urn:uuid:4a4f-cam</wsa:Address></wsa:EndpointReference>
<d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>
<d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/HIKVISION%20DS-2CD2143G2 onvif://www.onvif.org/hardware/DS-2CD2143G2-I onvif://www.onvif.org/location/city/hangzhou</d:Scopes>
<d:XAddrs>{xaddrs}</d:XAddrs><d:MetadataVersion>10</d:MetadataVersion>
</d:ProbeMatch></d:ProbeMatches></SOAP-ENV:Body></SOAP-ENV:Envelope>"#
        )
    }

    #[test]
    fn a_probe_match_gives_identity_scopes_and_pinned_device_urls() {
        let ids = vec!["abc".to_owned()];
        let found = parse_probe_match(
            probe_match("abc", "http://192.168.254.1/onvif/device_service http://10.0.10.23:8080/onvif/device_service").as_bytes(),
            DEVICE,
            &ids,
        )
        .expect("probe match");
        assert_eq!(
            found.endpoint_reference.as_deref(),
            Some("urn:uuid:4a4f-cam")
        );
        assert_eq!(found.name.as_deref(), Some("HIKVISION DS-2CD2143G2"));
        assert_eq!(found.hardware.as_deref(), Some("DS-2CD2143G2-I"));
        assert!(found.video);
        // The advertised 192.168.254.1 is never contacted.
        assert_eq!(
            found.device_urls,
            [
                "http://10.0.10.23/onvif/device_service",
                "http://10.0.10.23:8080/onvif/device_service"
            ]
        );
    }

    #[test]
    fn replies_to_other_probes_and_junk_are_ignored() {
        let ids = vec!["abc".to_owned()];
        assert!(
            parse_probe_match(probe_match("other", "http://x/").as_bytes(), DEVICE, &ids).is_none()
        );
        assert!(parse_probe_match(b"<not-xml", DEVICE, &ids).is_none());
        assert!(parse_probe_match(&vec![b' '; MAX_REPLY_BYTES + 1], DEVICE, &ids).is_none());
        assert!(
            parse_probe_match(b"<Envelope><Body><Hello/></Body></Envelope>", DEVICE, &ids)
                .is_none()
        );
    }

    #[test]
    fn stream_urls_are_pinned_and_lose_device_supplied_credentials() {
        assert_eq!(
            pin_stream_url(
                "rtsp://admin:pw@0.0.0.0:554/Streaming/Channels/101?transportmode=unicast",
                DEVICE
            )
            .as_deref(),
            Some("rtsp://10.0.10.23:554/Streaming/Channels/101?transportmode=unicast")
        );
        assert_eq!(pin_stream_url("file:///etc/passwd", DEVICE), None);
        assert_eq!(pin_stream_url("rtsp://cam/\nlive", DEVICE), None);
        assert_eq!(pin_http_url("rtsp://cam/", DEVICE), None);
    }

    #[test]
    fn the_digest_matches_the_ws_security_example() {
        // Values from the OASIS UsernameToken profile style: SHA1 over nonce, created, password.
        let digest = password_digest(b"0123456789abcdef", "2026-10-02T10:00:00Z", "secret");
        let mut hasher = Sha1::new();
        hasher.update(b"0123456789abcdef2026-10-02T10:00:00Zsecret");
        assert_eq!(digest, STANDARD.encode(hasher.finalize()));
    }

    #[test]
    fn device_media_and_profile_replies_parse() {
        let info = parse_device_information(
            r#"<Envelope><Body><GetDeviceInformationResponse><Manufacturer>HIKVISION</Manufacturer><Model>DS-2CD2143G2-I</Model><FirmwareVersion>V5.7.3</FirmwareVersion><SerialNumber>X</SerialNumber></GetDeviceInformationResponse></Body></Envelope>"#,
        )
        .expect("info");
        assert_eq!(info.manufacturer.as_deref(), Some("HIKVISION"));
        assert_eq!(info.model.as_deref(), Some("DS-2CD2143G2-I"));
        assert_eq!(
            parse_media_url(
                r#"<Envelope><Body><Capabilities><Device><XAddr>http://10.0.10.23/onvif/device_service</XAddr></Device><Media><XAddr>http://172.16.0.9/onvif/Media</XAddr></Media></Capabilities></Body></Envelope>"#,
                DEVICE
            )
            .as_deref(),
            Some("http://10.0.10.23/onvif/Media")
        );
        let profiles = parse_profiles(
            r#"<Envelope><Body><GetProfilesResponse>
<Profiles token="Profile_1"><Name>mainStream</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>2560</Width><Height>1440</Height></Resolution></VideoEncoderConfiguration></Profiles>
<Profiles token="Profile_2"><Name>subStream</Name><VideoEncoderConfiguration><Encoding>H265</Encoding><Resolution><Width>640</Width><Height>360</Height></Resolution></VideoEncoderConfiguration></Profiles>
</GetProfilesResponse></Body></Envelope>"#,
        )
        .expect("profiles");
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            (profiles[0].width, profiles[0].height),
            (Some(2560), Some(1440))
        );
        assert_eq!(profiles[1].encoding.as_deref(), Some("H265"));
        assert!(parse_profiles("<Envelope/>").is_err());
        let time = parse_device_time(
            "<E><UTCDateTime><Time><Hour>9</Hour><Minute>5</Minute><Second>7</Second></Time><Date><Year>2026</Year><Month>10</Month><Day>2</Day></Date></UTCDateTime></E>",
        )
        .expect("time");
        assert_eq!((time.hour(), time.minute(), time.day()), (9, 5, 2));
    }

    #[test]
    fn an_address_alone_tries_the_usual_device_service_locations() {
        let urls = device_url_candidates(DEVICE, Some(8899));
        assert_eq!(urls[0], "http://10.0.10.23:8899/onvif/device_service");
        assert!(urls.contains(&"http://10.0.10.23/onvif/device_service".to_owned()));
        // 8899 first, then the four other usual ports; two paths each.
        assert_eq!(urls.len(), 10);
    }
}
