//! IPC handler for `events.*` and `motion.*` ([`crate::events`],
//! [`crate::onvif_events`]).
//!
//! Methods:
//!   events.list        — {camera_ids?, kinds?, from_ms?, to_ms?, before_ms?, before_id?,
//!                         unacknowledged?, ongoing?, search?, limit?} → {events: [EventRecord]}, newest first
//!   events.get         — {id} → EventRecord + current matching zone_names
//!   events.open        — {} → {events: [EventRecord]} still going on
//!   events.wait        — {after?, boot?, timeout_ms?} → {boot, seq, reset, notices: [EventNotice]}
//!   events.acknowledge — {ids, acknowledged?} → {changed}
//!   events.delete      — {ids} → {deleted}
//!   events.bookmark    — {camera_id?, time_ms?, end_ms?, note?, kept?} → EventRecord
//!                        (`end_ms`: a span up to a day; `kept`: never delete its footage)
//!   events.update_bookmark — {id, note?, kept?} → EventRecord (`note` null clears it)
//!   motion.configure   — {camera_id, source?, sensitivity?, cooldown_sec?, min_duration_sec?,
//!                         zones?} → the camera's motion settings
//!   motion.status      — {} → {cameras: {id: {source, onvif?, kinds: [kind], active: [kind]}}}
//!                         (`kinds`: what the camera has reported, from its topics)
//!   camera.onvif.detect — {id} → {device_url, events, pull_point, ptz} (stored on the camera)
//!
//! `events.wait` is a long poll: it answers as soon as there is a notice
//! after `after`, or empty after `timeout_ms` (at most 25 s). Without
//! `after` it only returns the cursor. `reset` means the cursor belongs to
//! another backend run or is too old: list again, then continue from `seq`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nw_core::camera::{CameraMotionConfig, MotionSource};
use nw_core::event::{EventKind, EventSource};
use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::events::{self, store, EventHub, NewEvent};
use crate::onvif_events;

pub struct EventsHandler {
    state: Arc<RwLock<AppState>>,
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(e.to_string()))
}

/// Longest span a bookmark covers.
const MAX_BOOKMARK_SPAN_MS: i64 = 24 * 60 * 60 * 1000;

/// A bookmark's note: printable, at most 500 characters; blank is none.
fn bookmark_note(value: &Value) -> Result<Option<String>, RpcError> {
    let text = match value {
        Value::Null => return Ok(None),
        Value::String(text) => text,
        _ => return Err(RpcError::invalid_params("'note' must be text")),
    };
    let note: String = text.chars().filter(|c| !c.is_control()).take(500).collect();
    let note = note.trim();
    Ok((!note.is_empty()).then(|| note.to_owned()))
}

fn ids(params: &Value) -> Result<Vec<i64>, RpcError> {
    let ids = params["ids"]
        .as_array()
        .ok_or_else(|| RpcError::invalid_params("Missing 'ids'"))?;
    ids.iter()
        .map(|id| {
            id.as_i64()
                .ok_or_else(|| RpcError::invalid_params("'ids' must be numbers"))
        })
        .collect()
}

fn strings(value: &Value, name: &str) -> Result<Vec<String>, RpcError> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::invalid_params(format!("'{name}' must be strings")))
            })
            .collect(),
        _ => Err(RpcError::invalid_params(format!("'{name}' must be a list"))),
    }
}

impl EventsHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self { state }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "events.list" => self.list(params).await,
            "events.get" => self.get(params).await,
            "events.open" => self.open().await,
            "events.wait" => self.wait(params).await,
            "events.acknowledge" => self.acknowledge(params).await,
            "events.delete" => self.delete(params).await,
            "events.bookmark" => self.bookmark(params).await,
            "events.update_bookmark" => self.update_bookmark(params).await,
            "events.thumbnail" => self.thumbnail(params).await,
            "snapshot.capture" => self.snapshot(params).await,
            "motion.configure" => self.motion_configure(params).await,
            "motion.status" => self.motion_status(params).await,
            "camera.onvif.detect" => self.onvif_detect(params).await,
            _ => Err(RpcError::method_not_found()),
        }
    }

    async fn hub(&self) -> Arc<EventHub> {
        Arc::clone(&self.state.read().await.events)
    }

    /// Resolve ids before dispatch: a thumbnail's internal get and mutations
    /// must not bypass the accounts gate. Check an entire batch before any
    /// acknowledgement or deletion can change the database.
    pub(crate) async fn authorize_ids(
        &self,
        session: &crate::accounts::Session,
        method: &str,
        params: &Value,
    ) -> Result<(), RpcError> {
        if session.cameras.is_empty() {
            return Ok(());
        }
        let event_ids = match method {
            "events.get" | "events.thumbnail" | "events.update_bookmark" => vec![params["id"]
                .as_i64()
                .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?],
            "events.acknowledge" | "events.delete" => ids(params)?,
            _ => return Ok(()),
        };
        self.flush_events().await?;
        let session = session.clone();
        let denied = self
            .with_db(move |conn| {
                for id in event_ids {
                    if let Some(event) = store::get(conn, id)? {
                        if event
                            .camera_id
                            .as_deref()
                            .is_some_and(|id| !session.sees(id))
                        {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            })
            .await?;
        if denied {
            return Err(RpcError {
                code: super::handler::NOT_ALLOWED as i32,
                message: "You may not access this camera's events".into(),
                data: None,
            });
        }
        Ok(())
    }

    async fn flush_events(&self) -> Result<(), RpcError> {
        self.hub()
            .await
            .flush_async()
            .await
            .map_err(RpcError::internal)
    }

    /// Run a database call off the async workers.
    async fn with_db<T: Send + 'static>(
        &self,
        work: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, RpcError> {
        let db = Arc::clone(&self.state.read().await.db);
        tokio::task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| RpcError::internal("The events database is unavailable"))?;
            work(&conn).map_err(|e| RpcError::internal(e.to_string()))
        })
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?
    }

    async fn list(&self, params: &Value) -> Result<Value, RpcError> {
        let search = match &params["search"] {
            Value::Null => None,
            Value::String(text) if text.chars().count() <= 200 => {
                let text = text.trim().to_lowercase();
                (!text.is_empty()).then_some(text)
            }
            _ => {
                return Err(RpcError::invalid_params(
                    "'search' must be text (at most 200 characters)",
                ))
            }
        };
        let ongoing = match &params["ongoing"] {
            Value::Null => None,
            Value::Bool(value) => Some(*value),
            _ => return Err(RpcError::invalid_params("'ongoing' must be a boolean")),
        };
        let search_cameras = if let Some(query) = &search {
            self.state
                .read()
                .await
                .config
                .cameras
                .iter()
                .filter(|c| c.name.to_lowercase().contains(query))
                .map(|c| c.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let kinds = strings(&params["kinds"], "kinds")?
            .iter()
            .map(|name| {
                EventKind::parse(name)
                    .ok_or_else(|| RpcError::invalid_params(format!("Unknown event kind '{name}'")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let before = match (params["before_ms"].as_i64(), params["before_id"].as_i64()) {
            (Some(ts), Some(id)) => Some((ts, id)),
            (None, None) => None,
            _ => {
                return Err(RpcError::invalid_params(
                    "'before_ms' and 'before_id' go together",
                ))
            }
        };
        let filter = store::Filter {
            cameras: strings(&params["camera_ids"], "camera_ids")?,
            kinds,
            from_ms: params["from_ms"].as_i64(),
            to_ms: params["to_ms"].as_i64(),
            before,
            unacknowledged_only: params["unacknowledged"].as_bool().unwrap_or(false),
            ongoing,
            search,
            search_cameras,
            limit: params["limit"]
                .as_u64()
                .map_or(100, |limit| limit.min(u64::from(store::MAX_LIST)) as u32),
        };
        // The writer may not have stored the newest changes yet.
        self.flush_events().await?;
        let events = self.with_db(move |conn| store::list(conn, &filter)).await?;
        Ok(json!({ "events": events }))
    }

    async fn get(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["id"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?;
        self.flush_events().await?;
        let event = self
            .with_db(move |conn| store::get(conn, id))
            .await?
            .ok_or_else(|| RpcError::invalid_params("Event not found"))?;
        let mut answer = to_value(&event)?;
        // Current names are selection-only metadata; never enlarge bulk history
        // or expose another camera's zones. No state guard survives a DB await.
        let names = {
            let state = self.state.read().await;
            state
                .config
                .cameras
                .iter()
                .find(|camera| Some(&camera.id) == event.camera_id.as_ref())
                .map(|camera| {
                    camera
                        .motion
                        .zones
                        .iter()
                        .filter(|zone| event.zones.contains(&zone.id))
                        .take(32)
                        .map(|zone| (zone.id.clone(), zone.name.clone()))
                        .collect::<std::collections::BTreeMap<_, _>>()
                })
                .unwrap_or_default()
        };
        answer["zone_names"] = to_value(&names)?;
        Ok(answer)
    }

    async fn snapshot(&self, params: &Value) -> Result<Value, RpcError> {
        use base64::Engine as _;
        let id = params["camera_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing camera_id"))?;
        let (camera, pictures) = {
            let state = self.state.read().await;
            (
                state
                    .config
                    .cameras
                    .iter()
                    .find(|c| c.id == id)
                    .cloned()
                    .ok_or_else(|| RpcError::invalid_params("Camera not found"))?,
                Arc::clone(&state.snapshots),
            )
        };
        let jpeg = pictures.capture(camera).await.map_err(RpcError::internal)?;
        Ok(
            json!({"jpeg_b64":base64::engine::general_purpose::STANDARD.encode(jpeg),
            "mime":"image/jpeg", "width":640, "height":360, "captured_ms":events::now_ms()}),
        )
    }

    async fn thumbnail(&self, params: &Value) -> Result<Value, RpcError> {
        use base64::Engine as _;
        use std::io::Read as _;
        let record = self.get(params).await?;
        let Some(relative) = record["thumbnail"].as_str() else {
            return Ok(json!({"available":false}));
        };
        let path = crate::snapshot::thumbnail_path(relative).map_err(RpcError::invalid_params)?;
        let jpeg = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
            if std::fs::symlink_metadata(&path)
                .map_err(|_| "Event picture is unavailable")?
                .file_type()
                .is_symlink()
            {
                return Err("Invalid event picture".into());
            }
            let file = std::fs::File::open(path).map_err(|_| "Event picture is unavailable")?;
            let mut data = Vec::new();
            file.take(crate::snapshot::MAX_JPEG_BYTES as u64 + 1)
                .read_to_end(&mut data)
                .map_err(|_| "Event picture is unreadable")?;
            if data.len() > crate::snapshot::MAX_JPEG_BYTES || !data.starts_with(&[255, 216]) {
                return Err("Invalid event picture".into());
            }
            Ok(data)
        })
        .await
        .map_err(|_| RpcError::internal("Event picture worker stopped"))?
        .map_err(RpcError::internal)?;
        Ok(
            json!({"available":true, "mime":"image/jpeg", "jpeg_b64":base64::engine::general_purpose::STANDARD.encode(jpeg)}),
        )
    }

    async fn open(&self) -> Result<Value, RpcError> {
        Ok(json!({ "events": self.hub().await.open_events() }))
    }

    async fn wait(&self, params: &Value) -> Result<Value, RpcError> {
        let timeout = Duration::from_millis(params["timeout_ms"].as_u64().unwrap_or(20_000))
            .min(events::MAX_WAIT);
        let hub = self.hub().await;
        let result = hub
            .wait(params["boot"].as_str(), params["after"].as_u64(), timeout)
            .await;
        Ok(json!({
            "boot": result.boot,
            "seq": result.seq,
            "reset": result.reset,
            "notices": result.notices,
        }))
    }

    async fn acknowledge(&self, params: &Value) -> Result<Value, RpcError> {
        let ids = ids(params)?;
        let open_ids = ids.clone();
        let acknowledged = params["acknowledged"].as_bool().unwrap_or(true);
        self.flush_events().await?;
        let changed = self
            .with_db(move |conn| store::acknowledge(conn, &ids, acknowledged))
            .await?;
        // Keep live snapshots and later end notices in step with the DB.
        let hub = self.hub().await;
        for id in open_ids {
            hub.update(id, |event| event.acknowledged = acknowledged);
        }
        Ok(json!({ "changed": changed }))
    }

    async fn delete(&self, params: &Value) -> Result<Value, RpcError> {
        let ids = ids(params)?;
        let count = ids.len();
        self.flush_events().await?;
        let thumbnails = self.with_db(move |conn| store::delete(conn, &ids)).await?;
        events::remove_thumbnails(thumbnails).await;
        Ok(json!({ "deleted": count }))
    }

    async fn bookmark(&self, params: &Value) -> Result<Value, RpcError> {
        let camera_id = match &params["camera_id"] {
            Value::Null => None,
            Value::String(id) => {
                if !self
                    .state
                    .read()
                    .await
                    .config
                    .cameras
                    .iter()
                    .any(|c| &c.id == id)
                {
                    return Err(RpcError::invalid_params("Camera not found"));
                }
                Some(id.clone())
            }
            _ => return Err(RpcError::invalid_params("'camera_id' must be a string")),
        };
        let note = bookmark_note(&params["note"])?;
        let kept = match &params["kept"] {
            Value::Null => false,
            value => value
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("'kept' must be a boolean"))?,
        };
        let mut event = NewEvent::new(camera_id, EventKind::Bookmark, EventSource::User).kept(kept);
        if let Some(note) = note {
            event = event.detail(note);
        }
        let at = match &params["time_ms"] {
            Value::Null => events::now_ms(),
            value => value
                .as_i64()
                .filter(|at| *at >= 0)
                .ok_or_else(|| RpcError::invalid_params("'time_ms' must be a time"))?,
        };
        let end = match &params["end_ms"] {
            Value::Null => None,
            value => Some(
                value
                    .as_i64()
                    .filter(|end| *end > at && *end - at <= MAX_BOOKMARK_SPAN_MS)
                    .ok_or_else(|| {
                        RpcError::invalid_params("A bookmark ends after it starts, within a day")
                    })?,
            ),
        };
        let hub = self.hub().await;
        let record = match end {
            None => hub.instant(event, at),
            Some(end) => {
                let started = hub.start_at(event, at);
                hub.end_at(started.id, end).unwrap_or(started)
            }
        };
        if kept {
            tracing::info!(id = record.id, camera_id = ?record.camera_id, "Bookmark keeps its footage");
        }
        to_value(&record)
    }

    async fn update_bookmark(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["id"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?;
        let note = match params.get("note") {
            None => None,
            Some(value) => Some(bookmark_note(value)?),
        };
        let kept = match params.get("kept") {
            None => None,
            Some(value) => Some(
                value
                    .as_bool()
                    .ok_or_else(|| RpcError::invalid_params("'kept' must be a boolean"))?,
            ),
        };
        self.flush_events().await?;
        let record = self
            .with_db(move |conn| {
                store::edit_bookmark(conn, id, note.as_ref().map(|n| n.as_deref()), kept)
            })
            .await?
            .ok_or_else(|| RpcError::invalid_params("Bookmark not found"))?;
        if let Some(kept) = kept {
            tracing::info!(id, kept, "Bookmark footage protection changed");
        }
        self.hub().await.changed(record.clone());
        to_value(&record)
    }

    async fn motion_configure(&self, params: &Value) -> Result<Value, RpcError> {
        let camera_id = params["camera_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'camera_id'"))?;
        let mut st = self.state.write().await;
        let cam = st
            .config
            .cameras
            .iter_mut()
            .find(|c| c.id == camera_id)
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;
        let mut motion = cam.motion.clone();
        apply_motion_params(&mut motion, params)?;
        cam.motion = motion.clone();
        st.save_config()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        st.onvif_events.kick();
        tracing::info!(cam_id = %camera_id, source = ?motion.effective_source(), "Motion settings changed");
        to_value(&motion)
    }

    async fn motion_status(&self, params: &Value) -> Result<Value, RpcError> {
        let camera_id = params
            .get("camera_id")
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| RpcError::invalid_params("'camera_id' must be a string"))
            })
            .transpose()?;
        let include_preview = params
            .get("include_preview")
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| RpcError::invalid_params("'include_preview' must be a boolean"))
            })
            .transpose()?
            .unwrap_or(false);
        if include_preview && camera_id.is_none() {
            return Err(RpcError::invalid_params(
                "Choose one camera for an analysis preview",
            ));
        }
        let (cameras, hub, onvif, builtin) = {
            let st = self.state.read().await;
            let cameras: Vec<(String, MotionSource)> = st
                .config
                .cameras
                .iter()
                .filter(|c| camera_id.map_or(true, |id| id == c.id))
                .map(|c| (c.id.clone(), c.motion.effective_source()))
                .collect();
            (
                cameras,
                Arc::clone(&st.events),
                Arc::clone(&st.onvif_events),
                Arc::clone(&st.builtin_motion),
            )
        };
        let mut active: HashMap<String, Vec<&'static str>> = HashMap::new();
        for event in hub.open_events() {
            if let Some(camera) = event.camera_id {
                active.entry(camera).or_default().push(event.kind.as_str());
            }
        }
        let cameras: serde_json::Map<String, Value> = cameras
            .into_iter()
            .map(|(id, source)| {
                let camera_status = source.uses_onvif().then(|| onvif.status(&id)).flatten();
                // What the camera reports, as event kinds (its topics name them).
                let mut kinds: Vec<&'static str> = camera_status
                    .iter()
                    .flat_map(|status| status.topics.iter())
                    .filter_map(|topic| onvif_events::tracker::classify(topic))
                    .map(EventKind::as_str)
                    .collect();
                if source.uses_builtin() {
                    kinds.push("motion");
                }
                kinds.sort_unstable();
                kinds.dedup();
                let (builtin_status, preview) =
                    builtin.snapshot(&id, include_preview && source.uses_builtin());
                let mut status = json!({
                    "source": source,
                    "onvif": camera_status,
                    "builtin": builtin_status,
                    "kinds": kinds,
                    "active": active.remove(&id).unwrap_or_default(),
                });
                if include_preview && source.uses_builtin() {
                    status["preview"] = preview.unwrap_or(Value::Null);
                }
                (id, status)
            })
            .collect();
        Ok(json!({ "cameras": cameras }))
    }

    async fn onvif_detect(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?;
        let cam = self
            .state
            .read()
            .await
            .config
            .cameras
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;
        let detected = onvif_events::detect(&cam)
            .await
            .map_err(|e| RpcError::internal(format!("ONVIF: {e}")))?;
        {
            let mut st = self.state.write().await;
            if let Some(cam) = st.config.cameras.iter_mut().find(|c| c.id == id) {
                if cam.onvif.as_ref() != Some(&detected.endpoint) {
                    cam.onvif = Some(detected.endpoint.clone());
                    st.save_config()
                        .map_err(|e| RpcError::internal(e.to_string()))?;
                }
            }
        }
        // Answers carry no URLs with credentials: the endpoint has none.
        Ok(json!({
            "device_url": detected.endpoint.device_url,
            "events": detected.services.events_url.is_some(),
            "pull_point": detected.services.pull_point,
            "ptz": detected.services.ptz_url.is_some(),
        }))
    }

    /// The event hub of the state (for tests of other handlers).
    #[cfg(test)]
    pub async fn hub_for_test(&self) -> Arc<EventHub> {
        self.hub().await
    }

    #[cfg(test)]
    pub async fn onvif_for_test(&self) -> Arc<onvif_events::OnvifEvents> {
        Arc::clone(&self.state.read().await.onvif_events)
    }
}

/// Change motion settings from request parameters; anything left out stays.
fn apply_motion_params(motion: &mut CameraMotionConfig, params: &Value) -> Result<(), RpcError> {
    let mut merged = serde_json::to_value(&*motion)
        .map_err(|_| RpcError::internal("Invalid saved motion settings"))?;
    for key in [
        "source",
        "sensitivity",
        "cooldown_sec",
        "min_duration_sec",
        "low_cpu",
        "zones",
    ] {
        if let Some(value) = params.get(key).filter(|v| !v.is_null()) {
            merged[key] = value.clone();
        }
    }
    if !params["source"].is_null() {
        merged["enabled"] = Value::Bool(false);
    }
    *motion = crate::motion::validate_settings(&merged).map_err(RpcError::invalid_params)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;
    use nw_core::camera::CameraConfig;

    fn handler() -> (EventsHandler, Arc<RwLock<AppState>>, tempfile_dir::Dir) {
        let dir = tempfile_dir::Dir::new();
        let mut config = AppConfig::default();
        let mut cam = CameraConfig::new("Front", "rtsp://127.0.0.1:9/none");
        cam.id = "cam-1".into();
        config.cameras.push(cam);
        let state = Arc::new(RwLock::new(AppState::new(
            config,
            dir.path().join("config.json"),
            HwAccelKind::Software,
        )));
        (EventsHandler::new(Arc::clone(&state)), state, dir)
    }

    #[tokio::test]
    async fn motion_preview_requires_one_camera_and_low_cpu_updates_preserve_other_fields() {
        let (handler, state, _dir) = handler();
        let first = handler
            .dispatch(
                "motion.configure",
                &json!({"camera_id":"cam-1", "source":"builtin", "sensitivity":8, "low_cpu":true}),
            )
            .await
            .unwrap();
        assert_eq!(first["low_cpu"], true);
        let second = handler
            .dispatch(
                "motion.configure",
                &json!({"camera_id":"cam-1", "cooldown_sec":3}),
            )
            .await
            .unwrap();
        assert_eq!(second["low_cpu"], true);
        assert_eq!(second["sensitivity"], 8);
        for params in [
            json!({"include_preview":true}),
            json!({"camera_id":4}),
            json!({"camera_id":"cam-1", "include_preview":"yes"}),
        ] {
            assert!(handler.dispatch("motion.status", &params).await.is_err());
        }
        for params in [
            json!({"low_cpu":"yes"}),
            json!({"sensitivity":-1}),
            json!({"cooldown_sec":"three"}),
        ] {
            let mut params = params;
            params["camera_id"] = json!("cam-1");
            assert!(handler.dispatch("motion.configure", &params).await.is_err());
        }
        assert!(state.read().await.config.cameras[0].motion.low_cpu);
        let result = handler
            .dispatch("motion.status", &json!({"camera_id":"cam-1"}))
            .await
            .unwrap();
        assert!(result["cameras"]["cam-1"].get("preview").is_none());
        let result = handler
            .dispatch(
                "motion.status",
                &json!({"camera_id":"cam-1", "include_preview":true}),
            )
            .await
            .unwrap();
        assert!(result["cameras"]["cam-1"]["preview"].is_null());
    }

    #[tokio::test]
    async fn snapshots_and_thumbnails_refuse_unknown_targets_before_any_media_access() {
        let (handler, _, _dir) = handler();
        let snapshot = handler
            .dispatch("snapshot.capture", &json!({"camera_id":"missing"}))
            .await
            .unwrap_err();
        let thumbnail = handler
            .dispatch("events.thumbnail", &json!({"id":-1}))
            .await
            .unwrap_err();
        assert_eq!(snapshot.code, -32602);
        assert_eq!(thumbnail.code, -32602);
    }

    /// Private DB contention must not occupy the current-thread async executor.
    #[tokio::test(flavor = "current_thread")]
    async fn history_writer_contention_does_not_block_async_controls() {
        let (handler, state, _dir) = handler();
        let db = Arc::clone(&state.read().await.db);
        let (locked, acquired) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = db.lock().unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        acquired.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = std::time::Instant::now();
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let query = tokio::spawn(async move {
            entered.send(()).unwrap();
            handler.dispatch("events.list", &json!({})).await
        });
        entered_rx.await.unwrap();
        let scheduling_delay = started.elapsed();
        query.await.unwrap().unwrap();
        holder.join().unwrap();
        assert!(
            scheduling_delay < Duration::from_millis(300),
            "event history blocked the async worker for {scheduling_delay:?}"
        );
    }

    /// A private temporary directory, removed when dropped.
    mod tempfile_dir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let mut name = [0u8; 8];
                let _ = getrandom::getrandom(&mut name);
                let suffix: String = name.iter().map(|b| format!("{b:02x}")).collect();
                let path = std::env::temp_dir().join(format!("nw-events-test-{suffix}"));
                std::fs::create_dir_all(&path).expect("temp dir");
                Self(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[tokio::test]
    async fn acknowledging_motion_updates_open_snapshot_and_end_notice_without_deleting_history() {
        let (handler, state, _dir) = handler();
        let hub = Arc::clone(&state.read().await.events);
        let event = hub.start_at(
            NewEvent::new(Some("cam-1".into()), EventKind::Motion, EventSource::Onvif),
            1000,
        );
        handler
            .dispatch("events.acknowledge", &json!({"ids": [event.id]}))
            .await
            .expect("acknowledge");
        assert!(hub.open_events()[0].acknowledged);
        let ended = hub.end_at(event.id, 2000).expect("end motion");
        assert!(ended.acknowledged);
        let unread = handler
            .dispatch(
                "events.list",
                &json!({"kinds": ["motion"], "unacknowledged": true}),
            )
            .await
            .expect("unacknowledged history");
        assert_eq!(unread["events"], json!([]));
        let history = handler
            .dispatch(
                "events.list",
                &json!({"kinds": ["motion"], "from_ms": 0, "to_ms": 3000}),
            )
            .await
            .expect("playback history");
        assert_eq!(history["events"][0]["id"], event.id);
        assert_eq!(history["events"][0]["end_ms"], 2000);
        assert_eq!(history["events"][0]["acknowledged"], true);
        let notices = hub.wait(None, Some(0), Duration::ZERO).await;
        assert!(
            notices
                .notices
                .last()
                .expect("end notice")
                .event
                .acknowledged
        );
    }

    #[tokio::test]
    async fn history_search_matches_current_camera_names_and_validates_filters() {
        let (handler, state, _dir) = handler();
        let hub = Arc::clone(&state.read().await.events);
        let event = hub.start_at(
            NewEvent::new(Some("cam-1".into()), EventKind::Motion, EventSource::Onvif),
            1000,
        );
        let result = handler
            .dispatch(
                "events.list",
                &json!({"search": "  FRONT  ", "ongoing": true}),
            )
            .await
            .unwrap();
        assert_eq!(result["events"][0]["id"], event.id);
        let result = handler
            .dispatch("events.list", &json!({"search": "front", "ongoing": false}))
            .await
            .unwrap();
        assert_eq!(result["events"], json!([]));
        for params in [
            json!({"search": 42}),
            json!({"search": "x".repeat(201)}),
            json!({"ongoing": "yes"}),
        ] {
            assert!(handler.dispatch("events.list", &params).await.is_err());
        }
    }

    #[tokio::test]
    async fn selected_details_resolve_only_the_events_camera_zones() {
        use crate::events::NewEvent;
        use nw_core::event::{EventKind, EventSource};
        let (handler, state, _dir) = handler();
        let hub = {
            let mut state = state.write().await;
            state.config.cameras[0].motion.zones = serde_json::from_value(json!([
                {"id":"gate","name":"Front gate","points":[[0,0],[1,0],[1,1]]}
            ]))
            .unwrap();
            Arc::clone(&state.events)
        };
        let event = hub.start_at(
            NewEvent::new(
                Some("cam-1".into()),
                EventKind::Motion,
                EventSource::Builtin,
            ),
            1000,
        );
        hub.update(event.id, |event| {
            event.score = Some(0.25);
            event.zones = vec!["gate".into(), "removed".into()];
            event.bbox = Some([0.1, 0.2, 0.3, 0.4]);
        });
        let got = handler
            .dispatch("events.get", &json!({"id":event.id}))
            .await
            .unwrap();
        assert_eq!(got["zone_names"], json!({"gate":"Front gate"}));
        assert_eq!(got["score"], 0.25);
        assert_eq!(got["zones"], json!(["gate", "removed"]));
    }

    #[tokio::test]
    async fn a_bookmark_may_span_time_keep_its_footage_and_be_edited() {
        let (handler, _state, _dir) = handler();
        let mark = handler
            .dispatch(
                "events.bookmark",
                &json!({"camera_id": "cam-1", "time_ms": 1_000, "end_ms": 61_000,
                        "note": "break-in", "kept": true}),
            )
            .await
            .expect("bookmark");
        assert_eq!(
            (mark["start_ms"].as_i64(), mark["end_ms"].as_i64()),
            (Some(1_000), Some(61_000))
        );
        assert_eq!(mark["kept"], true);
        for bad in [
            json!({"time_ms": 1_000, "end_ms": 1_000}),
            json!({"time_ms": 1_000, "end_ms": 1_000 + 86_400_001}),
            json!({"kept": "yes"}),
            json!({"note": 4}),
        ] {
            assert!(
                handler.dispatch("events.bookmark", &bad).await.is_err(),
                "{bad}"
            );
        }
        let id = mark["id"].as_i64().unwrap();
        let edited = handler
            .dispatch(
                "events.update_bookmark",
                &json!({"id": id, "note": "  police report 42 ", "kept": false}),
            )
            .await
            .expect("edit");
        assert_eq!(edited["detail"], "police report 42");
        assert!(edited.get("kept").is_none(), "not kept any more");
        let cleared = handler
            .dispatch("events.update_bookmark", &json!({"id": id, "note": null}))
            .await
            .unwrap();
        assert!(cleared.get("detail").is_none());
        let motion = handler.hub_for_test().await.instant(
            NewEvent::new(None, EventKind::Motion, EventSource::Builtin),
            5,
        );
        assert!(
            handler
                .dispatch(
                    "events.update_bookmark",
                    &json!({"id": motion.id, "kept": true})
                )
                .await
                .is_err(),
            "only bookmarks keep footage"
        );
    }

    #[tokio::test]
    async fn bookmarks_are_listed_acknowledged_and_deleted() {
        let (handler, _state, _dir) = handler();
        let mark = handler
            .dispatch(
                "events.bookmark",
                &json!({"camera_id": "cam-1", "time_ms": 1_000, "note": "  delivery  "}),
            )
            .await
            .expect("bookmark");
        assert_eq!(mark["kind"], "bookmark");
        assert_eq!(mark["detail"], "delivery");
        assert_eq!(mark["end_ms"], 1_000);
        let unknown = handler
            .dispatch("events.bookmark", &json!({"camera_id": "nope"}))
            .await;
        assert!(unknown.is_err());

        let listed = handler
            .dispatch(
                "events.list",
                &json!({"kinds": ["bookmark"], "camera_ids": ["cam-1"]}),
            )
            .await
            .expect("list");
        assert_eq!(listed["events"].as_array().map(Vec::len), Some(1));
        let id = mark["id"].as_i64().unwrap();
        handler
            .dispatch("events.acknowledge", &json!({"ids": [id]}))
            .await
            .expect("acknowledge");
        let got = handler
            .dispatch("events.get", &json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(got["acknowledged"], true);
        handler
            .dispatch("events.delete", &json!({"ids": [id]}))
            .await
            .expect("delete");
        assert!(handler
            .dispatch("events.get", &json!({"id": id}))
            .await
            .is_err());
        assert!(handler
            .dispatch("events.list", &json!({"kinds": ["nope"]}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn wait_reports_new_events_after_the_cursor() {
        let (handler, _state, _dir) = handler();
        let cursor = handler
            .dispatch("events.wait", &json!({}))
            .await
            .expect("cursor");
        let hub = handler.hub_for_test().await;
        hub.start(NewEvent::new(
            Some("cam-1".into()),
            EventKind::Motion,
            EventSource::Onvif,
        ));
        let got = handler
            .dispatch(
                "events.wait",
                &json!({"after": cursor["seq"], "boot": cursor["boot"], "timeout_ms": 1000}),
            )
            .await
            .expect("wait");
        assert_eq!(got["notices"][0]["phase"], "start");
        assert_eq!(got["notices"][0]["event"]["kind"], "motion");
        let open = handler.dispatch("events.open", &json!({})).await.unwrap();
        assert_eq!(open["events"].as_array().map(Vec::len), Some(1));
        let status = handler.dispatch("motion.status", &json!({})).await.unwrap();
        assert_eq!(status["cameras"]["cam-1"]["active"][0], "motion");
        assert_eq!(status["cameras"]["cam-1"]["kinds"], json!([]));
    }

    #[tokio::test]
    async fn motion_configure_validates_and_keeps_what_is_left_out() {
        let (handler, state, _dir) = handler();
        let saved = handler
            .dispatch(
                "motion.configure",
                &json!({"camera_id": "cam-1", "source": "onvif", "sensitivity": 7}),
            )
            .await
            .expect("configure");
        assert_eq!(saved["source"], "onvif");
        assert_eq!(saved["sensitivity"], 7);
        let saved = handler
            .dispatch(
                "motion.configure",
                &json!({"camera_id": "cam-1", "zones": [
                    {"id": "z1", "kind": "exclude", "points": [[0,0],[1,0],[1,1]]}
                ]}),
            )
            .await
            .expect("zones");
        assert_eq!(saved["source"], "onvif", "left out, so kept");
        assert_eq!(saved["zones"][0]["kind"], "exclude");
        for bad in [
            json!({"camera_id": "cam-1", "source": "sometimes"}),
            json!({"camera_id": "cam-1", "sensitivity": 11}),
            json!({"camera_id": "cam-1", "zones": [{"id": "z", "points": [[0,0],[1,1]]}]}),
            json!({"camera_id": "nope", "source": "off"}),
        ] {
            assert!(
                handler.dispatch("motion.configure", &bad).await.is_err(),
                "{bad}"
            );
        }
        let st = state.read().await;
        assert_eq!(st.config.cameras[0].motion.source, MotionSource::Onvif);
        let on_disk = std::fs::read_to_string(&st.config_path).expect("saved");
        assert!(on_disk.contains("\"source\": \"onvif\""), "{on_disk}");
        let status = handler.dispatch("motion.status", &json!({})).await.unwrap();
        assert_eq!(status["cameras"]["cam-1"]["source"], "onvif");
        drop(st);
        assert!(handler.onvif_for_test().await.status("cam-1").is_none());
    }
}
