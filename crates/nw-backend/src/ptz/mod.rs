//! On-demand ONVIF PTZ sessions. Short camera timeouts plus expiring backend
//! leases stop continuous movement even if the desktop or connection vanishes.
mod parse;
#[cfg(test)]
mod tests;
use crate::{
    discovery::onvif::{self, OnvifError, SoapClient},
    events::EventHub,
};
use nw_core::camera::CameraConfig;
use parse::{escape, Options, Profile};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
const SETTLE: Duration = Duration::from_secs(2);
/// How long a held move runs without a renewal. A client on a slow link (the
/// web portal's relay) may ask for up to `MAX_LEASE`, never more than the
/// camera's own stop timeout allows.
const DEFAULT_LEASE: Duration = Duration::from_millis(900);
const MAX_LEASE: Duration = Duration::from_millis(3000);

struct Remote {
    client: SoapClient,
    credentials: Option<onvif::Credentials>,
    url: String,
    profile: Profile,
    options: Options,
    home: bool,
    presets: bool,
    owner: String,
    until: Instant,
}
impl Remote {
    async fn connect(camera: &CameraConfig) -> Result<Self, OnvifError> {
        let found =
            tokio::time::timeout(Duration::from_secs(12), crate::onvif_events::detect(camera))
                .await
                .map_err(|_| OnvifError::Unreachable("ONVIF detection timed out".into()))??;
        let url = found.endpoint.ptz_url.clone().ok_or_else(|| {
            OnvifError::Invalid("This camera does not advertise a PTZ service".into())
        })?;
        let credentials = crate::onvif_events::credentials(camera);
        let capabilities = found
            .client
            .call(
                &found.endpoint.device_url,
                "<tds:GetCapabilities><tds:Category>Media</tds:Category></tds:GetCapabilities>",
                credentials.as_ref(),
            )
            .await?;
        let media = onvif::parse_media_url(&capabilities, found.address)
            .ok_or_else(|| OnvifError::Invalid("Camera has no media service".into()))?;
        let profiles = found
            .client
            .call(&media, "<trt:GetProfiles/>", credentials.as_ref())
            .await?;
        let profile = parse::profile(&profiles)?;
        let options_body=format!("<tptz:GetConfigurationOptions><tptz:ConfigurationToken>{}</tptz:ConfigurationToken></tptz:GetConfigurationOptions>",escape(&profile.config));
        let options = parse::options(
            &found
                .client
                .call(&url, &options_body, credentials.as_ref())
                .await?,
        )?;
        let (home, presets) = if let Some(node) = &profile.node {
            let body = format!(
                "<tptz:GetNode><tptz:NodeToken>{}</tptz:NodeToken></tptz:GetNode>",
                escape(node)
            );
            found
                .client
                .call(&url, &body, credentials.as_ref())
                .await
                .map(|r| parse::node(&r))
                .unwrap_or((false, true))
        } else {
            (false, true)
        };
        Ok(Self {
            client: found.client,
            credentials,
            url,
            profile,
            options,
            home,
            presets,
            owner: String::new(),
            until: Instant::now(),
        })
    }
    fn body(&self, operation: &str, extra: &str) -> String {
        format!(
            "<tptz:{operation}><tptz:ProfileToken>{}</tptz:ProfileToken>{extra}</tptz:{operation}>",
            escape(&self.profile.token)
        )
    }
    async fn call(&self, operation: &str, extra: &str) -> Result<String, OnvifError> {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.client.call_command(
                &self.url,
                &self.body(operation, extra),
                self.credentials.as_ref(),
            ),
        )
        .await
        .map_err(|_| {
            OnvifError::Unreachable("PTZ command timed out; release the control and retry".into())
        })?
    }
    async fn stop(&mut self, hub: &EventHub, id: &str) -> Result<(), OnvifError> {
        self.owner.clear();
        self.until = Instant::now();
        let result = self.call("Stop", "").await.map(|_| ());
        hub.pause_activity(
            id,
            if result.is_ok() {
                SETTLE
            } else {
                Duration::from_secs(30)
            },
        );
        result
    }
}
struct Entry {
    fingerprint: String,
    remote: tokio::sync::Mutex<Option<Remote>>,
    generation: AtomicU64,
}
#[derive(Default)]
pub struct PtzService {
    sessions: Mutex<HashMap<String, Arc<Entry>>>,
}
impl PtzService {
    pub fn forget(&self, id: &str, hub: Arc<EventHub>) {
        let entry = self.sessions.lock().ok().and_then(|mut s| s.remove(id));
        if let Some(entry) = entry {
            entry.generation.fetch_add(1, Ordering::SeqCst);
            let id = id.to_owned();
            tokio::spawn(async move {
                if let Some(remote) = entry.remote.lock().await.as_mut() {
                    if !remote.owner.is_empty() {
                        let _ = remote.stop(&hub, &id).await;
                    }
                }
            });
        }
    }
    pub fn shutdown(&self, hub: Arc<EventHub>) {
        let ids = self
            .sessions
            .lock()
            .map(|s| s.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for id in ids {
            self.forget(&id, Arc::clone(&hub));
        }
    }
    pub async fn execute(
        &self,
        camera: &CameraConfig,
        hub: Arc<EventHub>,
        method: &str,
        params: &Value,
    ) -> Result<Value, OnvifError> {
        if !matches!(
            method,
            "ptz.capabilities"
                | "ptz.move"
                | "ptz.relative"
                | "ptz.stop"
                | "ptz.presets"
                | "ptz.goto_preset"
                | "ptz.save_preset"
                | "ptz.delete_preset"
                | "ptz.home"
        ) {
            return Err(OnvifError::Invalid("Unknown PTZ command".into()));
        }
        // Validate motion before any camera request; never accept NaN, infinity
        // or an out-of-range velocity from another client.
        let vector = if matches!(method, "ptz.move" | "ptz.relative") {
            Some(vector(params)?)
        } else {
            None
        };
        let requested_lease = if method == "ptz.move" {
            lease(params)?
        } else {
            DEFAULT_LEASE
        };
        if matches!(method, "ptz.goto_preset" | "ptz.delete_preset")
            && !params["token"].as_str().is_some_and(parse::token)
        {
            return Err(OnvifError::Invalid("Choose a valid preset".into()));
        }
        if method == "ptz.save_preset" {
            if !params["name"].as_str().is_some_and(|s| {
                !s.trim().is_empty()
                    && s.trim().chars().count() <= 64
                    && !s.chars().any(char::is_control)
            }) {
                return Err(OnvifError::Invalid(
                    "Use a preset name of 1–64 characters".into(),
                ));
            }
            if params
                .get("token")
                .is_some_and(|t| !t.as_str().is_some_and(parse::token))
            {
                return Err(OnvifError::Invalid("Invalid preset token".into()));
            }
        }
        let owner = params["client_id"].as_str().unwrap_or("local");
        if !parse::token(owner) {
            return Err(OnvifError::Invalid("Invalid PTZ client id".into()));
        }
        let fingerprint = format!(
            "{}|{:?}|{:?}|{:?}",
            camera.url, camera.username, camera.password_b64, camera.onvif
        );
        let entry = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| OnvifError::Invalid("PTZ sessions unavailable".into()))?;
            let entry = sessions.entry(camera.id.clone()).or_insert_with(|| {
                Arc::new(Entry {
                    fingerprint: fingerprint.clone(),
                    remote: tokio::sync::Mutex::new(None),
                    generation: AtomicU64::new(0),
                })
            });
            if entry.fingerprint != fingerprint
                || (method == "ptz.capabilities" && params["refresh"] == true)
            {
                entry.generation.fetch_add(1, Ordering::SeqCst);
                let old = Arc::clone(entry);
                let old_hub = Arc::clone(&hub);
                let old_id = camera.id.clone();
                tokio::spawn(async move {
                    if let Some(remote) = old.remote.lock().await.as_mut() {
                        if !remote.owner.is_empty() {
                            let _ = remote.stop(&old_hub, &old_id).await;
                        }
                    }
                });
                *entry = Arc::new(Entry {
                    fingerprint,
                    remote: tokio::sync::Mutex::new(None),
                    generation: AtomicU64::new(0),
                });
            }
            Arc::clone(entry)
        };
        let mut session = entry.remote.lock().await;
        if method == "ptz.stop" && session.is_none() && params["force"] != true {
            return Ok(json!({"ok":true}));
        }
        if session.is_none() {
            *session = Some(
                tokio::time::timeout(Duration::from_secs(18), Remote::connect(camera))
                    .await
                    .map_err(|_| OnvifError::Unreachable("PTZ discovery timed out".into()))??,
            );
        }
        let remote = session
            .as_mut()
            .ok_or_else(|| OnvifError::Invalid("PTZ unavailable".into()))?;
        if method == "ptz.capabilities" {
            return Ok(
                json!({"pan_tilt":remote.options.pan.is_some(),"zoom":remote.options.zoom.is_some(),"relative_pan_tilt":remote.options.relative_pan.is_some(),"relative_zoom":remote.options.relative_zoom.is_some(),"home":remote.home,"presets":remote.presets}),
            );
        }
        if method == "ptz.presets" {
            return Ok(json!(parse::presets(
                &remote.call("GetPresets", "").await?
            )?));
        }
        let moving = matches!(
            method,
            "ptz.move" | "ptz.relative" | "ptz.goto_preset" | "ptz.home"
        );
        if (moving || method == "ptz.stop")
            && !(method == "ptz.stop" && params["force"] == true)
            && !remote.owner.is_empty()
            && remote.owner != owner
            && remote.until > Instant::now()
        {
            return Err(OnvifError::Invalid(
                "Another PTZ control is active; release it first".into(),
            ));
        }
        if method == "ptz.stop" {
            if remote.owner.is_empty() && params["force"] != true {
                return Ok(json!({"ok": true}));
            }
            entry.generation.fetch_add(1, Ordering::SeqCst);
            remote.stop(&hub, &camera.id).await?;
            return Ok(json!({"ok":true}));
        }
        // A longer lease only as far as the camera's stop timeout reaches.
        let lease =
            requested_lease.min(DEFAULT_LEASE.max(Duration::from_secs_f64(remote.options.longest)));
        let (operation, extra) = match method {
            "ptz.move" | "ptz.relative" => {
                let (pan, tilt, zoom) =
                    vector.ok_or_else(|| OnvifError::Invalid("Missing velocity".into()))?;
                let relative = method == "ptz.relative";
                let (xy, z) = if relative {
                    (&remote.options.relative_pan, &remote.options.relative_zoom)
                } else {
                    (&remote.options.pan, &remote.options.zoom)
                };
                if (pan != 0.0 || tilt != 0.0) && xy.is_none() || zoom != 0.0 && z.is_none() {
                    return Err(OnvifError::Invalid(
                        "This camera does not support that PTZ axis or movement mode".into(),
                    ));
                }
                let mut extra = String::new();
                if let Some(s) = xy {
                    extra.push_str(&format!(
                        "<tt:PanTilt x=\"{}\" y=\"{}\" space=\"{}\"/>",
                        s.x(pan),
                        s.y(tilt),
                        escape(&s.uri)
                    ));
                }
                if let Some(s) = z {
                    extra.push_str(&format!(
                        "<tt:Zoom x=\"{}\" space=\"{}\"/>",
                        s.x(zoom),
                        escape(&s.uri)
                    ));
                }
                if relative {
                    (
                        "RelativeMove",
                        format!("<tptz:Translation>{extra}</tptz:Translation>"),
                    )
                } else {
                    let timeout = remote
                        .options
                        .timeout
                        .max(lease.as_secs_f64().min(remote.options.longest));
                    ("ContinuousMove",format!("<tptz:Velocity>{extra}</tptz:Velocity><tptz:Timeout>PT{timeout:.3}S</tptz:Timeout>"))
                }
            }
            "ptz.goto_preset" | "ptz.delete_preset" => {
                if !remote.presets {
                    return Err(OnvifError::Invalid("Presets are not supported".into()));
                }
                let token = params["token"]
                    .as_str()
                    .filter(|s| parse::token(s))
                    .ok_or_else(|| OnvifError::Invalid("Choose a valid preset".into()))?;
                (
                    if method == "ptz.goto_preset" {
                        "GotoPreset"
                    } else {
                        "RemovePreset"
                    },
                    format!("<tptz:PresetToken>{}</tptz:PresetToken>", escape(token)),
                )
            }
            "ptz.save_preset" => {
                if !remote.presets {
                    return Err(OnvifError::Invalid("Presets are not supported".into()));
                }
                let name = params["name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| {
                        !s.is_empty() && s.chars().count() <= 64 && !s.chars().any(char::is_control)
                    })
                    .ok_or_else(|| {
                        OnvifError::Invalid("Use a preset name of 1–64 characters".into())
                    })?;
                let mut extra = format!("<tptz:PresetName>{}</tptz:PresetName>", escape(name));
                if let Some(token) = params.get("token") {
                    let token = token
                        .as_str()
                        .filter(|s| parse::token(s))
                        .ok_or_else(|| OnvifError::Invalid("Invalid preset token".into()))?;
                    extra.push_str(&format!(
                        "<tptz:PresetToken>{}</tptz:PresetToken>",
                        escape(token)
                    ));
                }
                ("SetPreset", extra)
            }
            "ptz.home" if remote.home => ("GotoHomePosition", String::new()),
            _ => {
                return Err(OnvifError::Invalid(
                    "This camera does not support Home".into(),
                ))
            }
        };
        let generation = if moving {
            remote.owner = owner.into();
            remote.until = Instant::now()
                + if method == "ptz.move" {
                    lease
                } else {
                    Duration::from_secs(30)
                };
            hub.pause_activity(
                &camera.id,
                if method == "ptz.move" {
                    Duration::from_secs(3).max(lease + SETTLE)
                } else {
                    Duration::from_secs(32)
                },
            );
            entry.generation.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            entry.generation.load(Ordering::SeqCst)
        };
        // A timed-out reply may still have moved the camera. Arm the stop
        // watcher before returning either success or failure.
        let result = remote.call(operation, &extra).await;
        if moving {
            watch(
                Arc::clone(&entry),
                Arc::clone(&hub),
                camera.id.clone(),
                generation,
                (method == "ptz.move").then_some(lease),
            );
        }
        result?;
        if method == "ptz.move" {
            return Ok(json!({"ok":true,"lease_ms":lease.as_millis() as u64}));
        }
        Ok(json!({"ok":true}))
    }
}
fn vector(params: &Value) -> Result<(f64, f64, f64), OnvifError> {
    let value = |key: &str| match params.get(key) {
        None => Ok(0.0),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite() && (-1.0..=1.0).contains(v))
            .ok_or_else(|| {
                OnvifError::Invalid("PTZ coordinates must be numbers between -1 and 1".into())
            }),
    };
    Ok((value("pan")?, value("tilt")?, value("zoom")?))
}
/// The lease a `ptz.move` asks for: 900 ms unless it names 900–3000 ms.
fn lease(params: &Value) -> Result<Duration, OnvifError> {
    match params.get("lease_ms") {
        None => Ok(DEFAULT_LEASE),
        Some(value) => value
            .as_u64()
            .map(Duration::from_millis)
            .filter(|lease| (DEFAULT_LEASE..=MAX_LEASE).contains(lease))
            .ok_or_else(|| OnvifError::Invalid("PTZ lease must be 900–3000 ms".into())),
    }
}
/// Stops a held move when its `lease` lapses unrenewed, or watches a one-off
/// move (`None`) until the camera is idle.
fn watch(
    entry: Arc<Entry>,
    hub: Arc<EventHub>,
    id: String,
    generation: u64,
    lease: Option<Duration>,
) {
    let continuous = lease.is_some();
    tokio::spawn(async move {
        let deadline = Instant::now() + lease.unwrap_or(Duration::from_secs(30));
        loop {
            tokio::time::sleep(lease.unwrap_or(Duration::from_millis(500))).await;
            if entry.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let mut guard = entry.remote.lock().await;
            if entry.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let Some(remote) = guard.as_mut() else {
                return;
            };
            if continuous || Instant::now() >= deadline {
                entry.generation.fetch_add(1, Ordering::SeqCst);
                let _ = remote.stop(&hub, &id).await;
                return;
            }
            if remote
                .call("GetStatus", "")
                .await
                .ok()
                .and_then(|r| parse::idle(&r))
                == Some(true)
            {
                remote.owner.clear();
                remote.until = Instant::now();
                hub.pause_activity(&id, SETTLE);
                return;
            }
        }
    });
}
