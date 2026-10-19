//! What the web portal may do on this recorder, desktop or headless alike.
//!
//! The portal can manage cameras, recording settings and the recording drive.
//! A desktop recorder's window follows those changes (it reloads when the
//! configuration file changes).
//! Requests arrive over the node's signed cloud channel and run through the
//! same JSON-RPC methods as the desktop UI, but only an allowlisted subset,
//! with these extra rules:
//!
//! * a recording location is only ever one of this node's drives
//!   (`storage.select`), never a path from the request;
//! * stored camera passwords are never sent back (`has_password` instead), and
//!   an update that leaves the password out keeps it;
//! * cloud, playback, viewer and debug settings are not reachable;
//! * every `camera.*` request, and anything carrying a stream address or
//!   credentials, must arrive sealed (`cloud::sealed`), so the cloud relays
//!   only ciphertext; answers to sealed requests are sealed too;
//! * camera searches (`camera.discovery.*`) stay inside private networks, and
//!   their answers carry no stream URLs or interface addresses;
//! * PTZ (`ptz.*`, sealed) takes only each command's own fields and a
//!   `web-…` owner id, so it shares the desktop's leases and can never force
//!   another viewer's control to stop;
//! * `talk.probe` (sealed) only asks whether a camera has a speaker; the
//!   sound itself arrives over the live view room, never through this relay;
//! * event lists say whether a picture was saved (`has_image`), never its file.

use std::sync::Arc;

use serde_json::{json, Map, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::ipc::handler::RpcHandler;

pub struct RemoteConfig {
    rpc: RpcHandler,
    state: Arc<RwLock<AppState>>,
}

type Outcome = Result<Value, String>;

impl RemoteConfig {
    /// `rpc` must share `state` and must not have the cloud handler attached.
    pub fn new(rpc: RpcHandler, state: Arc<RwLock<AppState>>) -> Self {
        Self { rpc, state }
    }

    /// A request as it came from the web portal; `sealed` says whether it
    /// arrived encrypted for this node.
    pub async fn execute_from_portal(&self, method: &str, params: &Value, sealed: bool) -> Outcome {
        if !sealed
            && (method.starts_with("camera.")
                || method.starts_with("events.")
                || method.starts_with("ptz.")
                || method.starts_with("talk.")
                || method.starts_with("alerts.")
                || carries_secret(params))
        {
            return Err(
                "camera requests from the web portal must be encrypted for this recorder; reload the portal page and try again"
                    .to_owned(),
            );
        }
        self.execute(method, params).await
    }

    pub async fn execute(&self, method: &str, params: &Value) -> Outcome {
        let params = params
            .as_object()
            .ok_or_else(|| "params must be an object".to_owned())?;
        match method {
            "events.list" => {
                // Take the cursor first: notices arriving during the DB read
                // are replayed rather than lost between snapshot and polling.
                let cursor = self.call("events.wait", json!({"timeout_ms":0})).await?;
                let mut request = params.clone();
                let limit = request
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(200)
                    .clamp(1, 200);
                request.insert("limit".into(), limit.into());
                let mut answer = self.call("events.list", Value::Object(request)).await?;
                if let Some(events) = answer["events"].as_array_mut() {
                    for event in events {
                        trim_event(event);
                    }
                }
                answer["boot"] = cursor["boot"].clone();
                answer["seq"] = cursor["seq"].clone();
                Ok(answer)
            }
            "events.wait" => {
                let mut answer = self.call("events.wait", json!({"boot":params.get("boot"), "after":params.get("after"), "timeout_ms":0})).await?;
                if let Some(notices) = answer["notices"].as_array_mut() {
                    notices.truncate(200);
                    for notice in notices.iter_mut() {
                        trim_event(&mut notice["event"]);
                    }
                    if let Some(last) = notices.last() {
                        answer["seq"] = last["seq"].clone();
                    }
                }
                Ok(answer)
            }
            "events.get" => {
                let mut answer = self
                    .call("events.get", json!({"id":params.get("id")}))
                    .await?;
                trim_event_detail(&mut answer);
                Ok(answer)
            }
            "events.activity" => self.event_activity().await,
            "events.thumbnail" => {
                self.call("events.thumbnail", json!({"id":params.get("id")}))
                    .await
            }
            m if m.starts_with("ptz.") => self.call(m, Value::Object(ptz_params(m, params)?)).await,
            // Whether the camera has a speaker the browser can talk to.
            "talk.probe" => {
                self.call("talk.probe", json!({"camera_id": id_of_camera(params)?}))
                    .await
            }
            "camera.snapshot" => {
                self.call("snapshot.capture", json!({"camera_id":id_of(params)?}))
                    .await
            }
            "events.acknowledge" => self.acknowledge_activity(params).await,
            // Bookmarks: a camera's moment or span with a note, optionally
            // keeping its footage. The portal may delete bookmarks only.
            "events.bookmark" => {
                let mut request = pick(params, &["time_ms", "end_ms", "note", "kept"]);
                request.insert("camera_id".into(), Value::String(id_of_camera(params)?));
                let mut answer = self.call("events.bookmark", Value::Object(request)).await?;
                trim_event(&mut answer);
                Ok(answer)
            }
            "events.update_bookmark" => {
                let mut answer = self
                    .call(
                        "events.update_bookmark",
                        Value::Object(pick(params, &["id", "note", "kept"])),
                    )
                    .await?;
                trim_event(&mut answer);
                Ok(answer)
            }
            "events.delete_bookmark" => {
                let id = params
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or("Missing 'id'")?;
                let event = self.call("events.get", json!({"id": id})).await?;
                if event["kind"] != "bookmark" {
                    return Err("only bookmarks can be deleted from the web portal".into());
                }
                self.call("events.delete", json!({"ids": [id]})).await
            }
            // Alert notifications (sealed: channels hold secrets,
            // which the recorder masks in what it returns).
            "alerts.get" => self.call("alerts.get", json!({})).await,
            "alerts.set" => {
                self.call(
                    "alerts.set",
                    Value::Object(pick(params, &["channels", "rules"])),
                )
                .await
            }
            "alerts.test" => {
                self.call(
                    "alerts.test",
                    Value::Object(pick(params, &["channel_id", "channel"])),
                )
                .await
            }
            "alerts.history" => {
                self.call("alerts.history", Value::Object(pick(params, &["limit"])))
                    .await
            }
            "config.get" => self.config_get().await,
            "config.set" => self.config_set(params).await,
            "camera.list" => self.camera_list().await,
            "camera.add" => self.camera_add(params).await,
            "camera.update" => self.camera_update(params).await,
            "camera.remove" => {
                self.call("camera.remove", json!({ "id": id_of(params)? }))
                    .await
            }
            "recording.status_all" => self.call("recording.status_all", json!({})).await,
            "recording.set_config" => {
                if params.contains_key("local_storage_path") {
                    return Err(not_remote("a per-camera recording folder"));
                }
                self.call("recording.set_config", Value::Object(params.clone()))
                    .await
            }
            m if m.starts_with("camera.discovery.") => self.discovery(m, params).await,
            "storage.list" => {
                let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                self.call("storage.list", json!({ "all": all })).await
            }
            // A drive from the node's own scan, never a path.
            "storage.select" => {
                let mount_point = params.get("mount_point").cloned().unwrap_or(Value::Null);
                self.call("storage.select", json!({ "mount_point": mount_point }))
                    .await
            }
            _ => Err("this request is not allowed from the web portal".to_owned()),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Outcome {
        let response = self
            .rpc
            .dispatch(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .await;
        match response.get("error") {
            Some(error) => Err(error["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_owned()),
            None => Ok(response["result"].clone()),
        }
    }

    async fn camera_list(&self) -> Outcome {
        let mut cameras = redact_all(self.call("camera.list", json!({})).await?);
        let policies = self.call("recording.policy_status", json!({})).await?;
        if let Some(cameras) = cameras.as_array_mut() {
            for camera in cameras {
                let id = camera["id"].as_str().unwrap_or_default().to_owned();
                if let Some(policy) = policies.get(&id) {
                    camera["recording_policy_status"] = policy.clone();
                }
            }
        }
        Ok(cameras)
    }

    async fn event_activity(&self) -> Outcome {
        let (db, hub) = {
            let state = self.state.read().await;
            (Arc::clone(&state.db), Arc::clone(&state.events))
        };
        tokio::task::spawn_blocking(move || {
            hub.flush();
            let conn = db.lock().map_err(|_| "event database unavailable".to_owned())?;
            let mut statement = conn.prepare("SELECT camera_id, SUM(acknowledged = 0), COALESCE(MAX(CASE WHEN acknowledged = 0 THEN id END),0), SUM(end_ts IS NULL) FROM events WHERE event_type IN ('motion','intrusion','line_crossing') AND camera_id IS NOT NULL GROUP BY camera_id").map_err(|e| e.to_string())?;
            let rows = statement.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,i64>(3)?))).map_err(|e| e.to_string())?;
            let mut cameras = Map::new();
            for row in rows { let (id,pending,through_id,active) = row.map_err(|e| e.to_string())?; cameras.insert(id,json!({"pending":pending,"through_id":through_id,"active":active>0})); }
            Ok(json!({"cameras":cameras}))
        }).await.map_err(|e| e.to_string())?
    }

    async fn acknowledge_activity(&self, params: &Map<String, Value>) -> Outcome {
        if let Some(ids) = params.get("ids").and_then(Value::as_array) {
            if ids.is_empty()
                || ids.len() > 200
                || ids.iter().any(|id| id.as_i64().map_or(true, |id| id <= 0))
            {
                return Err("invalid event ids".into());
            }
            return self
                .call("events.acknowledge", json!({"ids":ids,"acknowledged":true}))
                .await;
        }
        let camera = params
            .get("camera_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or("camera id required")?
            .to_owned();
        let through = params
            .get("through_id")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or("event cutoff required")?;
        let mut changed = 0;
        loop {
            let (db, hub) = {
                let state = self.state.read().await;
                (Arc::clone(&state.db), Arc::clone(&state.events))
            };
            let camera = camera.clone();
            let ids: Vec<i64> = tokio::task::spawn_blocking(move || -> Result<Vec<i64>,String> {
                hub.flush();
                let conn=db.lock().map_err(|_|"event database unavailable".to_owned())?;
                let mut statement=conn.prepare("SELECT id FROM events WHERE camera_id=?1 AND id<=?2 AND acknowledged=0 AND event_type IN ('motion','intrusion','line_crossing') LIMIT 500").map_err(|e|e.to_string())?;
                let rows=statement.query_map(rusqlite::params![camera,through],|row|row.get(0)).map_err(|e|e.to_string())?;
                rows.collect::<rusqlite::Result<Vec<i64>>>().map_err(|e|e.to_string())
            }).await.map_err(|e|e.to_string())??;
            if ids.is_empty() {
                break;
            }
            let answer = self
                .call("events.acknowledge", json!({"ids":ids,"acknowledged":true}))
                .await?;
            changed += answer["changed"].as_u64().unwrap_or(0);
        }
        Ok(json!({"changed":changed}))
    }

    /// A camera search on the portal's behalf: only the fields each call
    /// takes, always as a portal request, and no addresses or URLs back.
    async fn discovery(&self, method: &str, params: &Map<String, Value>) -> Outcome {
        let fields: &[&str] = match method {
            "camera.discovery.interfaces" => &[],
            "camera.discovery.start" => &["scope", "interface", "cidr"],
            "camera.discovery.results" | "camera.discovery.cancel" => &["scan_id"],
            "camera.discovery.lookup" => &["address", "port"],
            "camera.discovery.profiles" => &["candidate_id", "username", "password_b64"],
            "camera.discovery.test" => &["candidate_id", "stream_id"],
            "camera.discovery.add" => &[
                "candidate_id",
                "main_stream_id",
                "sub_stream_id",
                "name",
                "record",
            ],
            _ => return Err("this request is not allowed from the web portal".to_owned()),
        };
        let mut request: Map<String, Value> = params
            .iter()
            .filter(|(key, _)| fields.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        request.insert("_portal".into(), Value::Bool(true));
        let mut answer = self.call(method, Value::Object(request)).await?;
        match method {
            "camera.discovery.interfaces" => {
                for iface in answer["interfaces"].as_array_mut().into_iter().flatten() {
                    if let Some(fields) = iface.as_object_mut() {
                        fields.remove("address");
                    }
                }
            }
            "camera.discovery.profiles" => {
                for stream in answer["streams"].as_array_mut().into_iter().flatten() {
                    if let Some(fields) = stream.as_object_mut() {
                        fields.remove("url");
                    }
                }
            }
            "camera.discovery.add" => answer = redact(answer),
            _ => {}
        }
        Ok(answer)
    }

    async fn config_get(&self) -> Outcome {
        let config = self.call("config.get", json!({})).await?;
        // Desktop or headless: the portal offers desktop notifications only
        // where there is a desktop to show them on.
        let headless = self.state.read().await.headless();
        Ok(json!({
            "site_name": config["site_name"],
            "recording": config["recording"],
            "events": config["events"],
            "headless": headless,
        }))
    }

    async fn config_set(&self, params: &Map<String, Value>) -> Outcome {
        for (key, value) in params {
            match key.as_str() {
                "site_name" => {
                    let name = value.as_str().unwrap_or_default().trim();
                    if name.is_empty() || name.chars().count() > 64 {
                        return Err("site name must be 1 to 64 characters".to_owned());
                    }
                }
                "recording" => check_recording_settings(value)?,
                // The core bounds the values; only these two are reachable.
                "events" => {
                    let fields = value
                        .as_object()
                        .ok_or_else(|| "event settings must be an object".to_owned())?;
                    if let Some(other) = fields
                        .keys()
                        .find(|key| !["retention_days", "push_port"].contains(&key.as_str()))
                    {
                        return Err(not_remote(other));
                    }
                }
                other => return Err(not_remote(other)),
            }
        }
        self.call("config.set", Value::Object(params.clone())).await
    }

    async fn camera_add(&self, params: &Map<String, Value>) -> Outcome {
        if let Some(motion) = params.get("motion") {
            crate::motion::validate_settings(motion)?;
        }
        let mut camera = params.clone();
        if !camera.contains_key("id") {
            camera.insert("id".into(), uuid::Uuid::new_v4().to_string().into());
        }
        check_camera_id(&camera["id"])?;
        if local_storage_path(&camera).is_some() {
            return Err(not_remote("a per-camera recording folder"));
        }
        for (key, default) in [
            ("notes", json!("")),
            ("transport", json!("tcp")),
            ("sub_url", Value::Null),
            ("username", Value::Null),
            ("password_b64", Value::Null),
            ("wall_position", Value::Null),
        ] {
            camera.entry(key).or_insert(default);
        }
        Ok(redact(
            self.call("camera.add", Value::Object(camera)).await?,
        ))
    }

    /// Changes only the fields sent; a password left out is kept.
    async fn camera_update(&self, params: &Map<String, Value>) -> Outcome {
        if let Some(motion) = params.get("motion") {
            crate::motion::validate_settings(motion)?;
        }
        let id = id_of(params)?;
        let current = {
            let st = self.state.read().await;
            let camera = st
                .config
                .cameras
                .iter()
                .find(|camera| camera.id == id)
                .ok_or_else(|| "Camera not found".to_owned())?;
            serde_json::to_value(camera).map_err(|e| e.to_string())?
        };
        let mut camera = current.as_object().cloned().unwrap_or_default();
        let mut changes = Value::Object(params.clone());
        crate::camera_credentials::preserve_url_credentials(&current, &mut changes);
        let changes = changes
            .as_object()
            .ok_or("camera changes must be an object")?;
        for (key, value) in changes {
            if key == "recording" {
                if let (Some(existing), Some(changes)) = (
                    camera.get_mut(key).and_then(Value::as_object_mut),
                    value.as_object(),
                ) {
                    for (field, new_value) in changes {
                        if field == "local" {
                            if let (Some(local), Some(local_changes)) = (
                                existing.get_mut(field).and_then(Value::as_object_mut),
                                new_value.as_object(),
                            ) {
                                local.extend(local_changes.clone());
                                continue;
                            }
                        }
                        existing.insert(field.clone(), new_value.clone());
                    }
                    continue;
                }
            }
            camera.insert(key.clone(), value.clone());
        }
        // The recording folder of a camera is local configuration only.
        if local_storage_path(&camera)
            != local_storage_path(current.as_object().unwrap_or(&Map::new()))
        {
            return Err(not_remote("a per-camera recording folder"));
        }
        Ok(redact(
            self.call("camera.update", Value::Object(camera)).await?,
        ))
    }
}

/// Bulk history stays small; analysis metadata is fetched for a selection only.
fn trim_event(event: &mut Value) {
    if let Some(fields) = event.as_object_mut() {
        // Whether a picture was saved, never its file name.
        let has_image = fields
            .get("thumbnail")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty());
        fields.retain(|key, _| {
            [
                "id",
                "kind",
                "camera_id",
                "start_ms",
                "end_ms",
                "source",
                "detail",
                "topic",
                "acknowledged",
                "kept",
            ]
            .contains(&key.as_str())
        });
        if has_image {
            fields.insert("has_image".into(), Value::Bool(true));
        }
        for key in ["detail", "topic"] {
            if let Some(Value::String(text)) = fields.get_mut(key) {
                let mut encoded_bytes = 0;
                *text = text
                    .chars()
                    .take_while(|ch| {
                        encoded_bytes += match ch {
                            '\u{0}'..='\u{1f}' => 6,
                            '"' | '\\' => 2,
                            _ => ch.len_utf8(),
                        };
                        encoded_bytes <= 256
                    })
                    .collect();
            }
        }
    }
}

fn trim_event_detail(event: &mut Value) {
    let score = event["score"]
        .as_f64()
        .filter(|score| (0.0..=1.0).contains(score));
    let zones: Vec<String> = event["zones"]
        .as_array()
        .map(|zones| {
            zones
                .iter()
                .take(32)
                .filter_map(Value::as_str)
                .filter(|zone| !zone.is_empty() && zone.len() <= 64)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let names: serde_json::Map<String, Value> = zones
        .iter()
        .filter_map(|id| {
            event["zone_names"][id]
                .as_str()
                .map(|name| (id.clone(), Value::String(name.chars().take(128).collect())))
        })
        .collect();
    let bbox = event["bbox"]
        .as_array()
        .filter(|bbox| {
            bbox.len() == 4
                && bbox.iter().all(|value| {
                    value
                        .as_f64()
                        .is_some_and(|value| (0.0..=1.0).contains(&value))
                })
        })
        .cloned();
    trim_event(event);
    event["score"] = json!(score);
    event["zones"] = json!(zones);
    event["zone_names"] = Value::Object(names);
    event["bbox"] = json!(bbox);
}

/// A PTZ request from the portal: only the fields its method takes, and an
/// owner id of the portal's own (`web-…`), so a browser can never take over
/// or force-stop the desktop's control.
fn ptz_params(method: &str, params: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let fields: &[&str] = match method {
        "ptz.capabilities" => &["refresh"],
        "ptz.presets" | "ptz.stop" | "ptz.home" => &[],
        // The relay is slower than a desktop: a held move may keep a longer
        // lease (the recorder bounds it to 3 s and the camera's own timeout).
        "ptz.move" => &["pan", "tilt", "zoom", "lease_ms"],
        "ptz.relative" => &["pan", "tilt", "zoom"],
        "ptz.goto_preset" | "ptz.delete_preset" => &["token"],
        "ptz.save_preset" => &["name", "token"],
        _ => return Err("this request is not allowed from the web portal".to_owned()),
    };
    let client = params
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("web-") && id.len() <= 64)
        .ok_or_else(|| "PTZ client id must be the web portal's own".to_owned())?;
    let mut request = Map::new();
    request.insert("camera_id".into(), Value::String(id_of_camera(params)?));
    request.insert("client_id".into(), Value::String(client.to_owned()));
    for field in fields {
        if let Some(value) = params.get(*field) {
            request.insert((*field).to_owned(), value.clone());
        }
    }
    Ok(request)
}

/// Only these fields of a portal request (the recorder checks their values).
fn pick(params: &Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|field| {
            params
                .get(*field)
                .map(|value| ((*field).to_owned(), value.clone()))
        })
        .collect()
}

fn id_of_camera(params: &Map<String, Value>) -> Result<String, String> {
    params
        .get("camera_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .map(str::to_owned)
        .ok_or_else(|| "Missing 'camera_id'".to_owned())
}

/// Field names that carry a stream address or credentials, at any depth.
fn carries_secret(value: &Value) -> bool {
    const SECRET: &[&str] = &[
        "url",
        "sub_url",
        "username",
        "password",
        "password_b64",
        "streams",
    ];
    match value {
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| SECRET.contains(&key.as_str()) || carries_secret(value)),
        Value::Array(items) => items.iter().any(carries_secret),
        _ => false,
    }
}

fn not_remote(what: &str) -> String {
    format!("{what} cannot be changed from the web portal")
}

fn id_of(params: &Map<String, Value>) -> Result<String, String> {
    params
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Missing 'id'".to_owned())
}

fn check_camera_id(id: &Value) -> Result<(), String> {
    let valid = id.as_str().is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    if valid {
        Ok(())
    } else {
        Err("camera id must be 1 to 64 letters, digits, '-' or '_'".to_owned())
    }
}

fn check_recording_settings(value: &Value) -> Result<(), String> {
    let settings = value
        .as_object()
        .ok_or_else(|| "recording settings must be an object".to_owned())?;
    for (key, value) in settings {
        let fits = |min: u64, max: u64| value.as_u64().is_some_and(|v| (min..=max).contains(&v));
        match key.as_str() {
            "segment_minutes" if !fits(1, 60) => {
                return Err("segment length must be 1 to 60 minutes".to_owned())
            }
            "pre_buffer_sec" if !fits(0, 60) => {
                return Err("pre-record buffer must be 0 to 60 seconds".to_owned())
            }
            "local_all_cameras" if !value.is_boolean() => {
                return Err("record all cameras must be true or false".to_owned())
            }
            "disk_threshold_percent"
                if !fits(0, u64::from(crate::config::MAX_DISK_THRESHOLD_PERCENT)) =>
            {
                return Err("disk threshold must be 0 (off) to 99 percent".to_owned())
            }
            "segment_minutes"
            | "pre_buffer_sec"
            | "local_all_cameras"
            | "disk_threshold_percent" => {}
            "storage_path" => {
                return Err(not_remote(
                    "the recording location as a path (choose a drive)",
                ))
            }
            other => return Err(not_remote(other)),
        }
    }
    Ok(())
}

fn local_storage_path(camera: &Map<String, Value>) -> Option<&Value> {
    camera
        .get("recording")
        .and_then(|recording| recording.get("local"))
        .and_then(|local| local.get("storage_path"))
        .filter(|path| !path.is_null())
}

/// Never send a stored camera password to the portal.
fn redact(mut camera: Value) -> Value {
    let has_password = crate::camera_credentials::redact_passwords(&mut camera);
    if let Some(fields) = camera.as_object_mut() {
        fields.insert("has_password".into(), has_password.into());
    }
    camera
}

fn redact_all(cameras: Value) -> Value {
    match cameras {
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;
    use crate::stats::test_support::{StaticCameras, StaticGpu, StaticIpc, StaticSystem};
    use crate::stats::PerformanceAggregator;
    use crate::viewer::capabilities::ViewerCapabilities;
    use crate::viewer::ViewerManager;
    use nw_core::stats::{GpuStatus, IpcMetrics, ProcessMetrics};

    fn remote(name: &str) -> (RemoteConfig, Arc<RwLock<AppState>>, std::path::PathBuf) {
        let _ = gstreamer::init();
        let path =
            std::env::temp_dir().join(format!("nw-remote-{name}-{}.json", uuid::Uuid::new_v4()));
        // A private in-memory recording index, never the user's real one.
        let state = Arc::new(RwLock::new(AppState::with_database(
            AppConfig::default(),
            path.clone(),
            HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )));
        let viewer = Arc::new(ViewerManager::new(
            HwAccelKind::Software,
            ViewerCapabilities::unavailable("no_overlay_sink"),
        ));
        let stats = Arc::new(PerformanceAggregator::new(
            Arc::new(StaticSystem(ProcessMetrics {
                cpu_percent: 0.0,
                rss_bytes: 0,
                memory_percent: 0.0,
                system_cpu_percent: None,
                system_memory_percent: None,
                network: None,
                siblings: vec![],
            })),
            Arc::new(StaticGpu(GpuStatus::unavailable("test"))),
            Arc::new(StaticIpc(IpcMetrics::default())),
            Arc::new(StaticCameras(std::sync::Mutex::new(vec![]))),
        ));
        let rpc = RpcHandler::new(Arc::clone(&state), viewer, stats);
        (RemoteConfig::new(rpc, Arc::clone(&state)), state, path)
    }

    async fn done(state: &Arc<RwLock<AppState>>, path: &std::path::Path) {
        state.write().await.stop_all();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn camera_requests_and_secrets_must_arrive_sealed() {
        let (remote, state, path) = remote("sealed-only");
        let unsealed_list = remote
            .execute_from_portal("camera.list", &json!({}), false)
            .await;
        let unsealed_secret = remote
            .execute_from_portal(
                "config.set",
                &json!({"recording": {"url": "rtsp://x"}}),
                false,
            )
            .await;
        let plain_settings = remote
            .execute_from_portal("config.get", &json!({}), false)
            .await;
        let sealed_list = remote
            .execute_from_portal("camera.list", &json!({}), true)
            .await;
        done(&state, &path).await;

        assert!(unsealed_list.unwrap_err().contains("encrypted"));
        assert!(unsealed_secret.unwrap_err().contains("encrypted"));
        let settings = plain_settings.unwrap();
        assert_eq!(
            settings["headless"], false,
            "a desktop recorder is configurable too"
        );
        assert!(settings["recording"].is_object());
        assert!(sealed_list.is_ok());
    }

    #[tokio::test]
    async fn event_reads_are_sealed_and_event_history_cannot_be_deleted() {
        use crate::events::NewEvent;
        use nw_core::event::{EventKind, EventSource};
        let (remote, state, path) = remote("portal-events");
        let hub = Arc::clone(&state.read().await.events);
        let first = hub.start_at(
            NewEvent::new(Some("a".into()), EventKind::Motion, EventSource::Onvif),
            1000,
        );
        hub.end_at(first.id, 2000);
        let history = remote
            .execute_from_portal("events.list", &json!({"limit": 200}), true)
            .await
            .unwrap();
        assert_eq!(history["events"][0]["id"], first.id);
        let selected = remote
            .execute_from_portal("events.get", &json!({"id":first.id}), true)
            .await
            .unwrap();
        assert_eq!(selected["id"], first.id);
        assert!(selected.get("thumbnail").is_none());
        assert!(history["boot"].is_string());
        let activity = remote
            .execute_from_portal("events.activity", &json!({}), true)
            .await
            .unwrap();
        assert_eq!(activity["cameras"]["a"]["pending"], 1);
        let second = hub.start_at(
            NewEvent::new(Some("a".into()), EventKind::Motion, EventSource::Onvif),
            3000,
        );
        remote
            .execute_from_portal(
                "events.acknowledge",
                &json!({"camera_id":"a", "through_id":first.id}),
                true,
            )
            .await
            .unwrap();
        let activity = remote
            .execute_from_portal("events.activity", &json!({}), true)
            .await
            .unwrap();
        assert_eq!(activity["cameras"]["a"]["pending"], 1);
        assert_eq!(activity["cameras"]["a"]["through_id"], second.id);
        assert_eq!(activity["cameras"]["a"]["active"], true);
        assert!(remote
            .execute_from_portal("events.list", &json!({}), false)
            .await
            .is_err());
        assert!(remote
            .execute_from_portal("events.delete", &json!({"ids":[first.id]}), true)
            .await
            .is_err());
        done(&state, &path).await;
    }

    #[tokio::test]
    async fn bookmarks_are_made_kept_edited_and_only_bookmarks_deleted_from_the_portal() {
        let (remote, state, path) = remote("bookmarks");
        let mut camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://cam.invalid/1");
        camera.id = "a".into();
        state.write().await.config.cameras.push(camera);
        let mark = remote
            .execute_from_portal(
                "events.bookmark",
                &json!({"camera_id":"a","time_ms":1_000,"end_ms":61_000,"note":"van",
                        "kept":true,"source":"system","force":true}),
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            (mark["kind"].as_str(), mark["kept"].as_bool()),
            (Some("bookmark"), Some(true))
        );
        assert_eq!(
            mark["source"], "user",
            "the portal cannot pose as the recorder"
        );
        assert!(
            remote
                .execute_from_portal("events.bookmark", &json!({"time_ms":1}), true)
                .await
                .is_err(),
            "a web bookmark names its camera"
        );
        let id = mark["id"].as_i64().unwrap();
        let edited = remote
            .execute_from_portal(
                "events.update_bookmark",
                &json!({"id":id,"kept":false}),
                true,
            )
            .await
            .unwrap();
        assert!(edited.get("kept").is_none());
        let hub = Arc::clone(&state.read().await.events);
        let motion = hub.instant(
            crate::events::NewEvent::new(
                Some("a".into()),
                nw_core::event::EventKind::Motion,
                nw_core::event::EventSource::Builtin,
            ),
            5,
        );
        hub.flush_async().await.unwrap();
        assert!(remote
            .execute_from_portal("events.delete_bookmark", &json!({"id":motion.id}), true)
            .await
            .is_err());
        remote
            .execute_from_portal("events.delete_bookmark", &json!({"id":id}), true)
            .await
            .unwrap();
        done(&state, &path).await;
    }

    #[tokio::test]
    async fn alerts_are_managed_sealed_and_never_reveal_secrets() {
        let (remote, state, path) = remote("alerts");
        let settings = json!({
            "channels": [{"id": "bot", "name": "Family", "kind": "telegram",
                          "bot_token": "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZ", "chat_id": "-100"}],
            "rules": [{"id": "r", "name": "Motion", "kinds": ["motion"], "channels": ["bot"]}]
        });
        assert!(
            remote
                .execute_from_portal("alerts.set", &settings, false)
                .await
                .is_err(),
            "secrets travel sealed only"
        );
        let saved = remote
            .execute_from_portal("alerts.set", &settings, true)
            .await
            .unwrap();
        assert!(!saved.to_string().contains("ABCDEFGHIJ"));
        let read = remote
            .execute_from_portal("alerts.get", &json!({}), true)
            .await
            .unwrap();
        assert_eq!(read, saved);
        let history = remote
            .execute_from_portal("alerts.history", &json!({"limit": 5}), true)
            .await
            .unwrap();
        assert!(history["deliveries"].is_array());
        done(&state, &path).await;
    }

    #[test]
    fn selected_metadata_keeps_bounded_details_but_never_private_paths() {
        let mut event = json!({"id":1,"score":0.25,"zones":["gate"],"zone_names":{"gate":"Front gate"},"bbox":[0.1,0.2,0.3,0.4],"thumbnail":"private/local/path","password":"secret"});
        trim_event_detail(&mut event);
        assert_eq!(event["score"], 0.25);
        assert_eq!(event["zones"], json!(["gate"]));
        assert_eq!(event["zone_names"], json!({"gate":"Front gate"}));
        assert!(event.get("thumbnail").is_none());
        assert!(event.get("password").is_none());
        let mut bulk = event.clone();
        trim_event(&mut bulk);
        assert!(bulk.get("zones").is_none());
    }

    #[test]
    fn event_projection_bounds_escaped_text_without_changing_identity() {
        let mut event = json!({"id":7,"camera_id":"camera-a","detail":"🙂".repeat(500),"topic":"\0".repeat(500),"thumbnail":"private/local/path"});
        trim_event(&mut event);
        assert!(serde_json::to_vec(&event["detail"]).unwrap().len() <= 258);
        assert!(serde_json::to_vec(&event["topic"]).unwrap().len() <= 258);
        assert_eq!(event["id"], 7);
        assert_eq!(event["camera_id"], "camera-a");
        assert!(event.get("thumbnail").is_none());
    }

    #[tokio::test]
    async fn portal_searches_stay_private_and_cannot_pass_hidden_fields() {
        let (remote, state, path) = remote("discovery");
        let public = remote
            .execute_from_portal(
                "camera.discovery.start",
                &json!({"scope": "cidr", "cidr": "8.8.8.0/24", "_portal": false}),
                true,
            )
            .await;
        let interfaces = remote
            .execute_from_portal("camera.discovery.interfaces", &json!({}), true)
            .await
            .expect("interfaces");
        let unknown = remote
            .execute_from_portal("camera.discovery.wipe", &json!({}), true)
            .await;
        done(&state, &path).await;

        assert!(
            public.unwrap_err().contains("private"),
            "the portal flag cannot be turned off"
        );
        assert!(interfaces["interfaces"]
            .as_array()
            .expect("list")
            .iter()
            .all(|iface| iface.get("address").is_none()));
        assert!(unknown.is_err());
    }

    #[tokio::test]
    async fn portal_motion_validation_refuses_silently_dropped_zones() {
        let (remote, state, path) = remote("invalid-motion");
        let result = remote
            .execute_from_portal(
                "camera.add",
                &json!({
                    "name":"Gate", "url":"rtsp://cam.invalid/1", "motion":{
                        "source":"onvif", "sensitivity":5,
                        "zones":[{"id":"bad", "kind":"include", "points":[[0,0],[2,0],[0,1]]}]
                    }
                }),
                true,
            )
            .await;
        assert!(
            result.is_err(),
            "invalid polygons must not silently become whole-picture detection"
        );
        assert!(state.read().await.config.cameras.is_empty());
        done(&state, &path).await;
    }

    #[tokio::test]
    async fn portal_images_use_the_existing_core_services_without_accepting_raw_urls() {
        let (remote, state, path) = remote("image-targets");
        let snapshot = remote
            .execute_from_portal("camera.snapshot", &json!({"id":"missing"}), true)
            .await
            .unwrap_err();
        let thumbnail = remote
            .execute_from_portal("events.thumbnail", &json!({"id":-1}), true)
            .await
            .unwrap_err();
        assert!(snapshot.contains("Camera not found"));
        assert!(thumbnail.contains("Event not found"));
        done(&state, &path).await;
    }

    #[tokio::test]
    async fn a_camera_added_from_the_portal_is_complete_and_its_password_stays_local() {
        let (remote, state, path) = remote("add");
        let added = remote
            .execute(
                "camera.add",
                &json!({"name": "Gate", "url": "rtsp://cam.invalid/1", "username": "admin", "password_b64": "c2VjcmV0"}),
            )
            .await;
        let listed = remote.execute("camera.list", &json!({})).await;
        let stored = state.read().await.config.cameras.clone();
        done(&state, &path).await;

        let added = added.expect("camera added");
        assert_eq!(added["has_password"], json!(true));
        assert!(added.get("password_b64").is_none());
        assert!(uuid::Uuid::parse_str(added["id"].as_str().unwrap_or_default()).is_ok());
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].password_b64.as_deref(), Some("c2VjcmV0"));
        assert!(!listed.expect("listed").to_string().contains("c2VjcmV0"));
    }

    #[tokio::test]
    async fn an_update_without_a_password_keeps_it_and_cannot_move_recordings() {
        let (remote, state, path) = remote("update");
        let id = remote
            .execute(
                "camera.add",
                &json!({"name": "Gate", "url": "rtsp://cam.invalid/1", "password_b64": "c2VjcmV0"}),
            )
            .await
            .expect("camera added")["id"]
            .clone();
        let renamed = remote
            .execute("camera.update", &json!({"id": id, "name": "Front gate"}))
            .await;
        let repath = remote
            .execute(
                "camera.update",
                &json!({"id": id, "recording": {"local": {"enabled": true, "storage_path": "/etc", "quota_gb": null}}}),
            )
            .await;
        let stored = state.read().await.config.cameras[0].clone();
        done(&state, &path).await;

        assert_eq!(renamed.expect("renamed")["name"], "Front gate");
        assert_eq!(stored.name, "Front gate");
        assert_eq!(stored.password_b64.as_deref(), Some("c2VjcmV0"));
        assert!(repath.is_err());
        assert_eq!(stored.recording.local.storage_path, None);
    }

    #[tokio::test]
    async fn security_portal_redacts_embedded_credentials_and_keeps_them_on_round_trip_updates() {
        use nw_core::camera::{CameraConfig, CameraStream};
        let (remote, state, path) = remote("embedded-passwords");
        let mut camera = CameraConfig::new("Gate", "rtsp://main-user:main-secret@cam.invalid/main");
        camera.id = "embedded".into();
        camera.sub_url = Some("rtsp://sub-user:sub-secret@cam.invalid/sub".into());
        camera.streams.push(CameraStream {
            id: "1".into(),
            name: "Main".into(),
            url: "rtsp://stream-user:stream-secret@cam.invalid/stream".into(),
            width: None,
            height: None,
        });
        state.write().await.config.cameras.push(camera.clone());
        let shown = remote.execute("camera.list", &json!({})).await.unwrap()[0].clone();
        assert_eq!(shown["has_password"], true);
        assert_eq!(shown["url"], "rtsp://cam.invalid/main");
        assert_eq!(shown["sub_url"], "rtsp://cam.invalid/sub");
        assert_eq!(shown["streams"][0]["url"], "rtsp://cam.invalid/stream");
        assert!(!shown.to_string().contains("secret"));
        let mut update = shown;
        update["name"] = json!("Renamed gate");
        let updated = remote.execute("camera.update", &update).await.unwrap();
        assert!(!updated.to_string().contains("secret"));
        assert_eq!(updated["has_password"], true);
        let stored = state.read().await.config.cameras[0].clone();
        assert_eq!(stored.url, camera.url);
        assert_eq!(stored.sub_url, camera.sub_url);
        assert_eq!(stored.streams, camera.streams);
        assert_eq!(stored.name, "Renamed gate");
        done(&state, &path).await;
    }

    #[tokio::test]
    async fn partial_recording_policy_updates_preserve_local_destination_and_validate_windows() {
        let (remote, state, path) = remote("policy-preservation");
        let added = remote
            .execute(
                "camera.add",
                &json!({"name":"Gate", "url":"rtsp://cam.invalid/1"}),
            )
            .await
            .unwrap();
        let id = added["id"].clone();
        state.write().await.config.cameras[0]
            .recording
            .local
            .storage_path = Some("/local-only".into());
        let changed = remote
            .execute_from_portal(
                "camera.update",
                &json!({"id":id,"recording":{"mode":"events","event_pre_sec":12}}),
                true,
            )
            .await;
        assert!(changed.is_ok(), "{changed:?}");
        let stored = state.read().await.config.cameras[0].recording.clone();
        assert_eq!(stored.local.storage_path.as_deref(), Some("/local-only"));
        assert_eq!(stored.event_pre_sec, 12);
        let listed = remote.execute("camera.list", &json!({})).await.unwrap();
        assert_eq!(
            listed[0]["recording_policy_status"]["active_mode"],
            "events"
        );
        assert_eq!(
            listed[0]["recording_policy_status"]["cleanup_active"],
            false
        );
        let invalid = remote.execute_from_portal("camera.update",&json!({"id":id,"recording":{"weekly_rules":[{"days":[0],"start":"24:00","end":"08:00","mode":"off"}]}}),true).await;
        assert!(invalid.is_err());
        assert_eq!(state.read().await.config.cameras[0].recording, stored);
        done(&state, &path).await;
    }

    #[tokio::test]
    async fn settings_change_within_bounds_but_never_by_path_or_outside_the_allowlist() {
        let (remote, state, path) = remote("settings");
        let changed = remote
            .execute("config.set", &json!({"recording": {"segment_minutes": 5}}))
            .await;
        let by_path = remote
            .execute(
                "config.set",
                &json!({"recording": {"storage_path": "/etc"}}),
            )
            .await;
        let debug = remote
            .execute("config.set", &json!({"debug": {"gst_log": "trace"}}))
            .await;
        let unlink = remote.execute("cloud.unlink", &json!({})).await;
        let playback = remote.execute("playback.open", &json!({})).await;
        let config = state.read().await.config.clone();
        done(&state, &path).await;

        assert!(changed.is_ok());
        assert_eq!(config.recording.segment_minutes, 5);
        assert!(by_path.is_err() && debug.is_err() && unlink.is_err() && playback.is_err());
        assert_ne!(config.recording.storage_path, "/etc");
        assert_eq!(config.debug.gst_log, "warn");
    }

    #[tokio::test]
    async fn ptz_reaches_a_desktop_recorder_sealed_with_only_its_own_fields() {
        let (remote, state, path) = remote("portal-ptz");
        // Unsealed PTZ is refused like any camera request.
        let unsealed = remote
            .execute_from_portal("ptz.stop", &json!({"camera_id":"a"}), false)
            .await;
        // Unknown PTZ methods and hidden fields never reach the camera code.
        let unknown = remote
            .execute_from_portal("ptz.reboot", &json!({"camera_id":"a"}), true)
            .await;
        // A desktop recorder takes PTZ (camera operation, not configuration);
        // the camera does not exist here, so the core answers, not the relay.
        let missing = remote
            .execute_from_portal(
                "ptz.move",
                &json!({"camera_id":"nope","pan":0.5,"client_id":"web-tab1","url":"rtsp://x"}),
                true,
            )
            .await;
        let bad_owner = remote
            .execute_from_portal(
                "ptz.move",
                &json!({"camera_id":"nope","pan":0.5,"client_id":"local"}),
                true,
            )
            .await;
        done(&state, &path).await;

        assert!(unsealed.unwrap_err().contains("encrypted"));
        assert!(unknown.unwrap_err().contains("not allowed"));
        assert_eq!(missing.unwrap_err(), "Camera not found");
        assert!(bad_owner.unwrap_err().contains("client"));
    }

    #[tokio::test]
    async fn talk_probe_reaches_a_desktop_recorder_sealed_and_nothing_else_of_talk_does() {
        let (remote, state, path) = remote("portal-talk");
        let unsealed = remote
            .execute_from_portal("talk.probe", &json!({"camera_id":"a"}), false)
            .await;
        // The camera does not exist here, so the core answers, not the relay.
        let missing = remote
            .execute_from_portal(
                "talk.probe",
                &json!({"camera_id":"nope","url":"rtsp://x"}),
                true,
            )
            .await;
        // Sound goes through the live view room; the relay cannot start it.
        let start = remote
            .execute_from_portal(
                "talk.start",
                &json!({"camera_id":"nope","client_id":"web-1","source":"tone"}),
                true,
            )
            .await;
        done(&state, &path).await;

        assert!(unsealed.unwrap_err().contains("encrypted"));
        assert_eq!(missing.unwrap_err(), "Camera not found");
        assert!(start.unwrap_err().contains("not allowed"));
    }

    #[test]
    fn ptz_requests_keep_only_movement_and_preset_fields() {
        let request = ptz_params(
            "ptz.save_preset",
            json!({"camera_id":"a","name":"Gate","token":"p1","client_id":"web-1",
                   "pan":0.2,"password_b64":"x","force":true})
            .as_object()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            Value::Object(request),
            json!({"camera_id":"a","name":"Gate","token":"p1","client_id":"web-1"})
        );
        let stop = ptz_params(
            "ptz.stop",
            json!({"camera_id":"a","force":true,"client_id":"web-1"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(
            stop.get("force").is_none(),
            "the portal cannot break another viewer's control"
        );
        let held = ptz_params(
            "ptz.move",
            json!({"camera_id":"a","client_id":"web-1","pan":0.5,"lease_ms":2500})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(held["lease_ms"], 2500);
        let step = ptz_params(
            "ptz.relative",
            json!({"camera_id":"a","client_id":"web-1","pan":0.1,"lease_ms":2500})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(step.get("lease_ms").is_none());
    }

    #[test]
    fn event_lists_say_whether_a_picture_exists_but_never_where() {
        let mut event = json!({"id":4,"kind":"motion","start_ms":1,"thumbnail":"4.jpg"});
        trim_event(&mut event);
        assert_eq!(event["has_image"], true);
        assert!(event.get("thumbnail").is_none());
        let mut plain = json!({"id":5,"kind":"disk_low","start_ms":1});
        trim_event(&mut plain);
        assert!(plain.get("has_image").is_none());
    }

    #[tokio::test]
    async fn event_settings_are_readable_and_bounded_from_the_portal() {
        let (remote, state, path) = remote("event-settings");
        let read = remote.execute("config.get", &json!({})).await.unwrap();
        let changed = remote
            .execute(
                "config.set",
                &json!({"events": {"retention_days": 14, "push_port": 0}}),
            )
            .await;
        let hidden = remote
            .execute(
                "config.set",
                &json!({"events": {"listen_address": "0.0.0.0"}}),
            )
            .await;
        let low_port = remote
            .execute("config.set", &json!({"events": {"push_port": 22}}))
            .await;
        let events = state.read().await.config.events.clone();
        done(&state, &path).await;

        assert_eq!(read["events"]["retention_days"], 30);
        assert_eq!(read["events"]["push_port"], 7681);
        assert!(changed.is_ok(), "{changed:?}");
        assert!(hidden.is_err() && low_port.is_err());
        assert_eq!((events.retention_days, events.push_port), (14, 0));
    }

    #[test]
    fn passwords_are_replaced_by_a_flag() {
        let shown = redact(json!({"id": "a", "password_b64": "c2VjcmV0"}));
        assert_eq!(shown, json!({"id": "a", "has_password": true}));
        let none = redact(json!({"id": "a", "password_b64": null}));
        assert_eq!(none, json!({"id": "a", "has_password": false}));
    }

    #[test]
    fn only_bounded_recording_settings_can_be_changed() {
        assert!(check_recording_settings(
            &json!({"segment_minutes": 5, "pre_buffer_sec": 0, "local_all_cameras": true, "disk_threshold_percent": 80})
        )
        .is_ok());
        for bad in [
            json!({"segment_minutes": 0}),
            json!({"pre_buffer_sec": 61}),
            json!({"local_all_cameras": "yes"}),
            json!({"disk_threshold_percent": 100}),
            json!({"storage_path": "/etc"}),
            json!({"gst_log": "debug"}),
            json!("not an object"),
        ] {
            assert!(check_recording_settings(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn camera_ids_are_plain_identifiers() {
        assert!(check_camera_id(&json!("6f1c2a9e-0d5b-4e2f-9c1a-1b2c3d4e5f60")).is_ok());
        for bad in [
            json!(""),
            json!("../x"),
            json!("a b"),
            json!(7),
            json!("x".repeat(65)),
        ] {
            assert!(check_camera_id(&bad).is_err(), "{bad}");
        }
    }
}
