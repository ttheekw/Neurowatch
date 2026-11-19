//! Small plugin-served pages and domain APIs. A service receives only the
//! node-signed user context and HTTP body, never the recorder's cookies.
use crate::identity;
use base64::{engine::general_purpose::STANDARD, Engine};
use nw_analytics_protocol::{
    rpc::{self, RpcError},
    web,
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub struct HttpRequest {
    pub path: String,
    pub method: String,
    pub content_type: String,
    pub body: Vec<u8>,
    pub user: web::UserContext,
}
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}
impl HttpResponse {
    pub fn html(html: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            body: html.into().into_bytes(),
        }
    }
    pub fn json(value: &serde_json::Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            body: value.to_string().into_bytes(),
        }
    }
}
struct Body {
    bytes: Vec<u8>,
    user: web::UserContext,
    expires: Instant,
}
#[derive(Default)]
pub struct Service {
    bodies: HashMap<String, Body>,
    bytes: usize,
}
fn invalid(message: &str) -> RpcError {
    RpcError::new(-32602, message)
}
fn same_user(a: &web::UserContext, b: &web::UserContext) -> bool {
    a.user == b.user && a.role == b.role && a.cameras == b.cameras && a.plugin_key == b.plugin_key
}
pub fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= 2048
        && !path.chars().any(|c| c.is_control() || c == '\\')
}
impl Service {
    pub fn dispatch(
        &mut self,
        method: &str,
        params: serde_json::Value,
        node_key: &str,
        plugin_key: &str,
        now: i64,
        serve: impl FnOnce(HttpRequest) -> Result<HttpResponse, RpcError>,
    ) -> Result<serde_json::Value, RpcError> {
        self.bodies.retain(|_, body| body.expires > Instant::now());
        self.bytes = self.bodies.values().map(|body| body.bytes.len()).sum();
        match method {
            "web.request" => {
                let request: web::Request = rpc::params(params)?;
                if !identity::verify_user(node_key, plugin_key, &request.user, now) {
                    return Err(invalid("Invalid or expired viewer context"));
                }
                if !valid_path(&request.path)
                    || !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"]
                        .contains(&request.method.as_str())
                    || request.body_b64.len() > web::CHUNK * 4 / 3 + 4
                {
                    return Err(invalid("Invalid HTTP request"));
                }
                let body = STANDARD
                    .decode(&request.body_b64)
                    .map_err(|_| invalid("Invalid request body"))?;
                let response = serve(HttpRequest {
                    path: request.path,
                    method: request.method,
                    content_type: request.content_type,
                    body,
                    user: request.user.context.clone(),
                })?;
                if response.body.len() > web::MAX_BODY
                    || !(200..=599).contains(&response.status)
                    || response.content_type.len() > 128
                    || !response.content_type.is_ascii()
                    || response.content_type.contains(['\r', '\n'])
                {
                    return Err(invalid("Plugin response exceeds ingress limits"));
                }
                while self.bytes + response.body.len() > 2 * web::MAX_BODY
                    || self.bodies.len() >= 16
                {
                    let Some(oldest) = self
                        .bodies
                        .iter()
                        .min_by_key(|(_, body)| body.expires)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    if let Some(body) = self.bodies.remove(&oldest) {
                        self.bytes -= body.bytes.len();
                    }
                }
                let token = identity::challenge().map_err(|_| invalid("Capability unavailable"))?;
                let result = web::Response {
                    token: token.clone(),
                    status: response.status,
                    content_type: response.content_type,
                    size: response.body.len(),
                };
                self.bytes += response.body.len();
                self.bodies.insert(
                    token,
                    Body {
                        bytes: response.body,
                        user: request.user.context,
                        expires: Instant::now() + Duration::from_secs(300),
                    },
                );
                serde_json::to_value(result).map_err(|_| invalid("Invalid response"))
            }
            "web.read" => {
                let request: web::Read = rpc::params(params)?;
                if !identity::verify_user(node_key, plugin_key, &request.user, now) {
                    return Err(invalid("Invalid or expired viewer context"));
                }
                let body = self
                    .bodies
                    .get(&request.token)
                    .filter(|body| same_user(&body.user, &request.user.context))
                    .ok_or_else(|| invalid("Page body expired or belongs to another viewer"))?;
                if request.offset > body.bytes.len() {
                    return Err(invalid("Invalid body offset"));
                }
                let end = request
                    .offset
                    .saturating_add(web::CHUNK)
                    .min(body.bytes.len());
                serde_json::to_value(web::Chunk {
                    offset: request.offset,
                    data_b64: STANDARD.encode(&body.bytes[request.offset..end]),
                    eof: end == body.bytes.len(),
                })
                .map_err(|_| invalid("Invalid response"))
            }
            _ => Err(RpcError::new(-32601, "Method not found")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::SigningKey;
    use nw_analytics_protocol::messages::CameraInfo;
    fn user(
        node: &SigningKey,
        plugin: &str,
        name: &str,
        cameras: Vec<CameraInfo>,
    ) -> web::SignedUser {
        identity::sign_user(
            node,
            web::UserContext {
                user: name.into(),
                role: "viewer".into(),
                cameras,
                plugin_key: plugin.into(),
                expires_at: 130,
                nonce: identity::challenge().unwrap(),
            },
        )
    }
    #[test]
    fn pages_are_chunked_bounded_and_scoped_to_the_signed_viewer_and_camera_set() {
        let node = SigningKey::from_bytes(&[1; 32]);
        let plugin = identity::public_key(&SigningKey::from_bytes(&[2; 32]));
        let public = identity::public_key(&node);
        let cameras = vec![CameraInfo {
            id: "a".into(),
            name: "Gate".into(),
        }];
        let user = user(&node, &plugin, "viewer-a", cameras.clone());
        let mut service = Service::default();
        let request = web::Request {
            path: "/".into(),
            method: "GET".into(),
            content_type: String::new(),
            body_b64: String::new(),
            user: user.clone(),
        };
        let response = service
            .dispatch(
                "web.request",
                serde_json::json!(request),
                &public,
                &plugin,
                100,
                |request| {
                    assert_eq!(request.user.cameras, cameras);
                    Ok(HttpResponse {
                        status: 200,
                        content_type: "text/plain".into(),
                        body: vec![7; web::CHUNK + 1],
                    })
                },
            )
            .unwrap();
        let response: web::Response = serde_json::from_value(response).unwrap();
        let read = web::Read {
            token: response.token.clone(),
            offset: 0,
            user: user.clone(),
        };
        let first = service
            .dispatch(
                "web.read",
                serde_json::json!(read),
                &public,
                &plugin,
                100,
                |_| unreachable!(),
            )
            .unwrap();
        assert_eq!(
            STANDARD
                .decode(first["data_b64"].as_str().unwrap())
                .unwrap()
                .len(),
            web::CHUNK
        );
        assert_eq!(first["eof"], false);
        let mut forged = read.clone();
        forged.user.context.role = "admin".into();
        assert!(service
            .dispatch(
                "web.read",
                serde_json::json!(forged),
                &public,
                &plugin,
                100,
                |_| unreachable!()
            )
            .is_err());
        let other = web::Read {
            user: super::tests::user(&node, &plugin, "viewer-b", cameras),
            ..read.clone()
        };
        assert!(service
            .dispatch(
                "web.read",
                serde_json::json!(other),
                &public,
                &plugin,
                100,
                |_| unreachable!()
            )
            .is_err());
        assert!(service
            .dispatch(
                "web.read",
                serde_json::json!(read),
                &public,
                &plugin,
                131,
                |_| unreachable!()
            )
            .is_err());
    }

    fn request_for(
        user: &web::SignedUser,
        path: &str,
        method: &str,
        body_b64: &str,
    ) -> web::Request {
        web::Request {
            path: path.into(),
            method: method.into(),
            content_type: String::new(),
            body_b64: body_b64.into(),
            user: user.clone(),
        }
    }

    fn ok(body: Vec<u8>) -> Result<HttpResponse, RpcError> {
        Ok(HttpResponse {
            status: 200,
            content_type: "text/plain".into(),
            body,
        })
    }

    #[test]
    fn responses_and_paths_have_helpers_and_limits() {
        let page = HttpResponse::html("<p>hi</p>");
        assert_eq!(page.status, 200);
        assert_eq!(page.content_type, "text/html; charset=utf-8");
        assert_eq!(page.body, b"<p>hi</p>");
        let data = HttpResponse::json(&serde_json::json!({"a": 1}));
        assert_eq!(data.content_type, "application/json");
        assert_eq!(data.body, br#"{"a":1}"#);
        assert!(valid_path("/ok?x=1"));
        for bad in [
            "",
            "ok",
            "//host",
            "/a\\b",
            "/a\nb",
            &format!("/{}", "a".repeat(2048)),
        ] {
            assert!(!valid_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn requests_are_refused_unless_signed_valid_and_within_limits() {
        let node = SigningKey::from_bytes(&[1; 32]);
        let plugin = identity::public_key(&SigningKey::from_bytes(&[2; 32]));
        let public = identity::public_key(&node);
        let signed = user(&node, &plugin, "viewer-a", vec![]);
        let mut service = Service::default();
        let call = |service: &mut Service,
                    method: &str,
                    params: serde_json::Value,
                    now: i64,
                    reply: Result<HttpResponse, RpcError>| {
            service.dispatch(method, params, &public, &plugin, now, move |_| reply)
        };
        let message = |result: Result<serde_json::Value, RpcError>| {
            let error = result.unwrap_err();
            (error.code, error.message)
        };
        // Expired viewer context.
        let (code, _) = message(call(
            &mut service,
            "web.request",
            serde_json::json!(request_for(&signed, "/", "GET", "")),
            500,
            ok(vec![]),
        ));
        assert_eq!(code, -32602);
        // Bad path, bad method, bad base64, oversized body.
        for request in [
            request_for(&signed, "//x", "GET", ""),
            request_for(&signed, "/", "TRACE", ""),
            request_for(&signed, "/", "POST", &"A".repeat(web::CHUNK * 4 / 3 + 5)),
        ] {
            let (_, text) = message(call(
                &mut service,
                "web.request",
                serde_json::json!(request),
                100,
                ok(vec![]),
            ));
            assert_eq!(text, "Invalid HTTP request");
        }
        let (_, text) = message(call(
            &mut service,
            "web.request",
            serde_json::json!(request_for(&signed, "/", "POST", "***")),
            100,
            ok(vec![]),
        ));
        assert_eq!(text, "Invalid request body");
        // The plugin's own errors pass through; its bad responses are refused.
        let (code, _) = message(call(
            &mut service,
            "web.request",
            serde_json::json!(request_for(&signed, "/", "GET", "")),
            100,
            Err(RpcError::new(-32000, "boom")),
        ));
        assert_eq!(code, -32000);
        for bad in [
            HttpResponse {
                status: 99,
                content_type: "text/plain".into(),
                body: vec![],
            },
            HttpResponse {
                status: 200,
                content_type: "text/\nplain".into(),
                body: vec![],
            },
            HttpResponse {
                status: 200,
                content_type: "x".repeat(129),
                body: vec![],
            },
            HttpResponse {
                status: 200,
                content_type: "t\u{e9}xt".into(),
                body: vec![],
            },
            HttpResponse {
                status: 200,
                content_type: "text/plain".into(),
                body: vec![0; web::MAX_BODY + 1],
            },
        ] {
            let (_, text) = message(call(
                &mut service,
                "web.request",
                serde_json::json!(request_for(&signed, "/", "GET", "")),
                100,
                Ok(bad),
            ));
            assert_eq!(text, "Plugin response exceeds ingress limits");
        }
        // Unknown methods and unreadable parameters.
        let (code, _) = message(call(
            &mut service,
            "web.nope",
            serde_json::json!({}),
            100,
            ok(vec![]),
        ));
        assert_eq!(code, -32601);
        let (code, _) = message(call(
            &mut service,
            "web.read",
            serde_json::json!({}),
            100,
            ok(vec![]),
        ));
        assert_eq!(code, -32602);
    }

    #[test]
    fn stored_pages_are_limited_in_number_and_read_within_bounds() {
        let node = SigningKey::from_bytes(&[1; 32]);
        let plugin = identity::public_key(&SigningKey::from_bytes(&[2; 32]));
        let public = identity::public_key(&node);
        let signed = user(&node, &plugin, "viewer-a", vec![]);
        let mut service = Service::default();
        let mut tokens = Vec::new();
        for _ in 0..20 {
            let reply = service
                .dispatch(
                    "web.request",
                    serde_json::json!(request_for(&signed, "/", "GET", "")),
                    &public,
                    &plugin,
                    100,
                    |_| ok(b"page".to_vec()),
                )
                .unwrap();
            let reply: web::Response = serde_json::from_value(reply).unwrap();
            assert_eq!(reply.size, 4);
            tokens.push(reply.token);
        }
        assert!(service.bodies.len() <= 16, "at most 16 pages are kept");
        let read = |service: &mut Service, token: &str, offset: usize| {
            service.dispatch(
                "web.read",
                serde_json::json!(web::Read {
                    token: token.into(),
                    offset,
                    user: signed.clone(),
                }),
                &public,
                &plugin,
                100,
                |_| unreachable!(),
            )
        };
        let last = tokens.last().unwrap();
        let whole = read(&mut service, last, 0).unwrap();
        assert_eq!(whole["eof"], true);
        assert_eq!(
            STANDARD
                .decode(whole["data_b64"].as_str().unwrap())
                .unwrap(),
            b"page"
        );
        let tail = read(&mut service, last, 4).unwrap();
        assert_eq!(tail["eof"], true);
        assert_eq!(tail["data_b64"], "");
        assert_eq!(
            read(&mut service, last, 5).unwrap_err().message,
            "Invalid body offset"
        );
        assert!(read(&mut service, "unknown", 0).is_err());
    }
}
