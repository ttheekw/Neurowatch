//! What each role may call, and what a camera-limited user sees.
//!
//! With accounts on, the IPC server asks [`required_role`] for each request
//! (methods not listed need an admin), refuses requests that name a camera
//! the user may not use ([`check_cameras`]), and removes other cameras from
//! what lists return ([`filter_result`]).

use serde_json::Value;

use super::{Role, Session};

/// Methods anyone may call without signing in: the sign-in itself, and
/// status reads the tray shows (no pictures, settings or secrets).
pub fn open(method: &str) -> bool {
    matches!(
        method,
        "system.status"
            | "system.capabilities"
            | "auth.login"
            | "auth.logout"
            | "auth.status"
            | "camera.status_all"
            | "recording.status_all"
            | "stats.get_performance"
            | "cloud.status"
    )
}

/// The least role that may call `method`.
pub fn required_role(method: &str) -> Role {
    const VIEWER: &[&str] = &[
        "config.get",
        "camera.list",
        "camera.codec_all",
        "camera.stream_info_all",
        "camera.set_stream",
        "view.update",
        "view.release",
        "events.list",
        "events.get",
        "events.open",
        "events.wait",
        "events.thumbnail",
        "events.acknowledge",
        "snapshot.capture",
        "motion.status",
        "plugins.cameras",
        "plugins.overlays",
        "plugins.history",
        "plugins.pages",
        "plugins.web.request",
        "plugins.web.read",
        "recording.list_segments",
        "recording.policy_status",
        "alerts.desktop",
        "accounts.me",
        "auth.change_password",
    ];
    const OPERATOR: &[&str] = &[
        "events.bookmark",
        "events.update_bookmark",
        "recording.start",
        "recording.stop",
        "camera.set_positions",
    ];
    if VIEWER.contains(&method) || method.starts_with("playback.") || method.starts_with("viewer.")
    {
        Role::Viewer
    } else if OPERATOR.contains(&method)
        || method.starts_with("ptz.")
        || method.starts_with("talk.")
        || method.starts_with("export.")
    {
        Role::Operator
    } else {
        Role::Admin
    }
}

/// Camera ids a request names.
fn named_cameras(method: &str, params: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for key in ["camera_id", "audio_camera_id"] {
        if let Some(id) = params.get(key).and_then(Value::as_str) {
            ids.push(id.to_owned());
        }
    }
    for key in ["camera_ids", "cameras"] {
        if let Some(list) = params.get(key).and_then(Value::as_array) {
            ids.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    // camera.* methods name their camera as `id`.
    if method.starts_with("camera.") {
        if let Some(id) = params.get("id").and_then(Value::as_str) {
            ids.push(id.to_owned());
        }
    }
    ids
}

/// Refuse a request naming a camera the user may not use; a camera-limited
/// user's event queries are limited to their cameras.
pub fn check_cameras(session: &Session, method: &str, params: &mut Value) -> Result<(), String> {
    if session.cameras.is_empty() {
        return Ok(());
    }
    if let Some(denied) = named_cameras(method, params)
        .into_iter()
        .find(|id| !session.sees(id))
    {
        return Err(format!("you may not use camera '{denied}'"));
    }
    if method == "events.list"
        && params
            .get("camera_ids")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        if let Some(object) = params.as_object_mut() {
            object.insert("camera_ids".into(), serde_json::json!(session.cameras));
        }
    }
    Ok(())
}

fn keep_event(session: &Session, event: &Value) -> bool {
    match event.get("camera_id").and_then(Value::as_str) {
        Some(id) => session.sees(id),
        // Recorder-wide events (drive full, recorder started) are for all.
        None => true,
    }
}

/// A result with other cameras removed, or `Err` for an event of a camera
/// the user may not see.
pub fn filter_result(session: &Session, method: &str, mut result: Value) -> Result<Value, String> {
    // Camera access does not grant access to the camera's own credentials.
    // Apply this even to non-admin users who may see every camera.
    if method == "camera.list" && session.role != Role::Admin {
        if let Some(cameras) = result.as_array_mut() {
            for camera in cameras {
                crate::camera_credentials::redact_passwords(camera);
                if let Some(fields) = camera.as_object_mut() {
                    fields.remove("username");
                }
            }
        }
    }
    if session.cameras.is_empty() {
        return Ok(result);
    }
    match method {
        "camera.list" => {
            if let Some(list) = result.as_array_mut() {
                list.retain(|camera| {
                    camera
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| session.sees(id))
                });
            }
        }
        "camera.status_all"
        | "camera.codec_all"
        | "camera.stream_info_all"
        | "recording.status_all"
        | "recording.policy_status" => {
            if let Some(map) = result.as_object_mut() {
                map.retain(|id, _| session.sees(id));
            }
        }
        "motion.status" | "plugins.cameras" => {
            if let Some(map) = result.get_mut("cameras").and_then(Value::as_object_mut) {
                map.retain(|id, _| session.sees(id));
            }
        }
        "plugins.overlays" | "plugins.history" => {
            let key = if method == "plugins.history" {
                "drawings"
            } else {
                "overlays"
            };
            if let Some(list) = result.get_mut(key).and_then(Value::as_array_mut) {
                list.retain(|overlay| {
                    overlay
                        .get("camera_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| session.sees(id))
                });
            }
            if let Some(map) = result.get_mut("figures").and_then(Value::as_object_mut) {
                map.retain(|id, _| session.sees(id));
            }
        }
        "events.list" | "events.open" => {
            if let Some(list) = result.get_mut("events").and_then(Value::as_array_mut) {
                list.retain(|event| keep_event(session, event));
            }
        }
        "events.wait" => {
            if let Some(list) = result.get_mut("notices").and_then(Value::as_array_mut) {
                list.retain(|notice| {
                    notice
                        .get("event")
                        .is_none_or(|event| keep_event(session, event))
                });
            }
        }
        "events.get" if !keep_event(session, &result) => {
            return Err("you may not see this camera's events".into());
        }
        "export.list" => {
            if let Some(jobs) = result.as_array_mut() {
                jobs.retain(|job| {
                    job.get("camera_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| session.sees(id))
                });
            }
        }
        "alerts.desktop" => {
            if let Some(list) = result.get_mut("alerts").and_then(Value::as_array_mut) {
                list.retain(|alert| keep_event(session, alert));
            }
        }
        _ => {}
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limited() -> Session {
        Session {
            username: "ana".into(),
            role: Role::Operator,
            cameras: vec!["cam-1".into()],
        }
    }

    #[test]
    fn roles_grow_from_watching_to_operating_to_managing() {
        assert_eq!(required_role("playback.open"), Role::Viewer);
        assert_eq!(required_role("events.list"), Role::Viewer);
        assert_eq!(required_role("ptz.move"), Role::Operator);
        assert_eq!(required_role("export.start"), Role::Operator);
        assert_eq!(required_role("events.bookmark"), Role::Operator);
        assert_eq!(required_role("talk.start"), Role::Operator);
        for admin in [
            "camera.add",
            "config.set",
            "alerts.set",
            "accounts.set_user",
            "system.shutdown",
            "events.delete",
            "storage.select",
            "camera.discovery.start",
            "some.future_method",
        ] {
            assert_eq!(required_role(admin), Role::Admin, "{admin}");
        }
        assert!(open("auth.login") && open("system.status") && !open("camera.list"));
        // A live view's plugin marker and drawings; managing plugins is admin.
        assert_eq!(required_role("plugins.cameras"), Role::Viewer);
        assert_eq!(required_role("plugins.overlays"), Role::Viewer);
        assert_eq!(required_role("plugins.history"), Role::Viewer);
        for admin in ["plugins.get", "plugins.set", "plugins.status"] {
            assert_eq!(required_role(admin), Role::Admin, "{admin}");
        }
    }

    #[test]
    fn plugin_markers_and_drawings_show_only_the_users_cameras() {
        let session = limited();
        let cameras = json!({"cameras": {"cam-1": [{"plugin": "m"}], "cam-2": [{"plugin": "m"}]}});
        let shown = filter_result(&session, "plugins.cameras", cameras).unwrap();
        assert_eq!(shown["cameras"].as_object().unwrap().len(), 1);
        let overlays = json!({"seq": 4, "overlays": [{"camera_id": "cam-1"}, {"camera_id": "cam-2"}],
            "figures": {"cam-1": [{"label": "Cars"}], "cam-2": [{"label": "Cars"}]}});
        let shown = filter_result(&session, "plugins.overlays", overlays).unwrap();
        assert_eq!(shown["overlays"], json!([{"camera_id": "cam-1"}]));
        assert_eq!(shown["figures"], json!({"cam-1": [{"label": "Cars"}]}));
        let history = json!({"drawings": [{"camera_id":"cam-1"},{"camera_id":"cam-2"}]});
        assert_eq!(
            filter_result(&session, "plugins.history", history).unwrap()["drawings"],
            json!([{"camera_id":"cam-1"}])
        );
        assert!(check_cameras(
            &session,
            "plugins.history",
            &mut json!({"camera_id":"cam-2"})
        )
        .is_err());
    }

    #[test]
    fn security_non_admin_camera_lists_never_expose_credentials_even_without_camera_limits() {
        let original = json!([{
            "id": "cam-1", "username": "camera-user", "password_b64": "c2VjcmV0",
            "url": "rtsp://camera-user:main-secret@cam.invalid/main",
            "sub_url": "rtsp://sub-user:sub-secret@cam.invalid/sub",
            "streams": [{"id": "1", "url": "rtsp://stream-user:stream-secret@cam.invalid/stream"}],
            "onvif": {"device_url": "http://onvif-user:onvif-secret@cam.invalid/onvif"}
        }]);
        for role in [Role::Viewer, Role::Operator] {
            for cameras in [vec![], vec!["cam-1".into()]] {
                let session = Session {
                    role,
                    cameras,
                    ..limited()
                };
                let shown = filter_result(&session, "camera.list", original.clone()).unwrap();
                assert!(shown[0].get("username").is_none());
                assert!(shown[0].get("password_b64").is_none());
                assert_eq!(shown[0]["url"], "rtsp://cam.invalid/main");
                assert_eq!(shown[0]["sub_url"], "rtsp://cam.invalid/sub");
                assert_eq!(shown[0]["streams"][0]["url"], "rtsp://cam.invalid/stream");
                assert_eq!(shown[0]["onvif"]["device_url"], "http://cam.invalid/onvif");
                assert!(!shown.to_string().contains("secret"));
            }
        }
        let admin = Session {
            role: Role::Admin,
            cameras: vec![],
            ..limited()
        };
        assert_eq!(
            filter_result(&admin, "camera.list", original.clone()).unwrap(),
            original
        );
    }

    #[test]
    fn a_camera_limited_user_names_and_sees_only_their_cameras() {
        let session = limited();
        let mut ok = json!({"camera_id": "cam-1"});
        assert!(check_cameras(&session, "playback.open", &mut ok).is_ok());
        let mut other = json!({"camera_id": "cam-2"});
        assert!(check_cameras(&session, "playback.open", &mut other).is_err());
        let mut view = json!({"cameras": ["cam-1", "cam-2"]});
        assert!(check_cameras(&session, "view.update", &mut view).is_err());
        let mut by_id = json!({"id": "cam-2"});
        assert!(check_cameras(&session, "camera.update", &mut by_id).is_err());
        let mut query = json!({"limit": 10});
        check_cameras(&session, "events.list", &mut query).unwrap();
        assert_eq!(query["camera_ids"], json!(["cam-1"]));

        let cameras = filter_result(
            &session,
            "camera.list",
            json!([{"id": "cam-1"}, {"id": "cam-2"}]),
        )
        .unwrap();
        assert_eq!(cameras, json!([{"id": "cam-1"}]));
        let status = filter_result(
            &session,
            "camera.status_all",
            json!({"cam-1": "ok", "cam-2": "ok"}),
        )
        .unwrap();
        assert_eq!(status, json!({"cam-1": "ok"}));
        let events = filter_result(&session, "events.wait", json!({"notices": [
            {"event": {"camera_id": "cam-2"}}, {"event": {"camera_id": "cam-1"}}, {"event": {"camera_id": null}}
        ]}))
        .unwrap();
        assert_eq!(events["notices"].as_array().unwrap().len(), 2);
        assert!(filter_result(&session, "events.get", json!({"camera_id": "cam-2"})).is_err());
        let everyone = Session {
            cameras: vec![],
            ..limited()
        };
        assert_eq!(
            filter_result(&everyone, "camera.list", json!([{"id": "cam-2"}])).unwrap(),
            json!([{"id": "cam-2"}])
        );
    }
}
