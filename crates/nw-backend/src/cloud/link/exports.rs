//! Clip export for the web portal.
//!
//! A recorder manager asks the portal for a clip; the portal queues a job and
//! wakes this node (`export_pull` event). The node takes the job with a
//! signed `export_pull`, cuts the clip with the same engine as the desktop
//! (`crate::export`: the video is copied, never re-encoded), and reports with
//! signed `export_report`s every [`REPORT_INTERVAL`]:
//!
//! 1. `running` with its progress;
//! 2. `ready` with the file's size — the reply carries a presigned PUT URL
//!    for exactly that many bytes (object storage; the node holds no
//!    storage credentials and the portal never relays the video);
//! 3. `uploading` with the upload's progress, then `uploaded`;
//! 4. or `failed` / `cancelled`. Any reply may say `cancel`: the node stops.
//!
//! The clip file is removed from this computer once uploaded (or abandoned).
//! An upload URL must be HTTPS on a public host, resolved to public addresses
//! and pinned like every cloud connection; plain HTTP on 127.0.0.1 only under
//! the development policy ([`super::EndpointPolicy`]).

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use time::{Duration as TimeDuration, OffsetDateTime};

use super::{
    fmt_time, sign_value, validate_host, validate_resolved_ips, without_signature, CloudError,
    CloudNode, CloudStatus, LinkPhase, Result, NODE_AUDIENCE,
};
use crate::export::{ExportRequest, ExportService, ExportStatus, TimelapseOptions};
use crate::playback::session::PlaybackSegment;

pub const EXPORT_PULL_OPERATION: &str = "export_pull";
pub const EXPORT_REPORT_OPERATION: &str = "export_report";
/// Longest the cloud holds an export pull open; below the cloud client's
/// 20 s request timeout.
pub const EXPORT_PULL_WAIT: Duration = Duration::from_secs(15);
/// A pull that came back at once is not repeated sooner than this.
const EXPORT_PULL_MIN_CYCLE: Duration = Duration::from_secs(2);
/// How often a running job reports (the portal fails one silent for 3 min).
pub const REPORT_INTERVAL: Duration = Duration::from_secs(2);
/// Clips cut for the portal at once (the portal hands out no more).
const MAX_RUNNING: usize = 3;
const MAX_ERROR_CHARS: usize = 300;
/// Terminal reports are retried this often before the job is let go.
const FINAL_REPORT_ATTEMPTS: u32 = 5;

/// A job the portal queued for this node.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ExportJob {
    pub export_id: String,
    pub camera_id: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub max_bytes: u64,
    /// A timelapse (keyframes sampled at an interval) instead of a clip.
    #[serde(default)]
    pub timelapse: Option<TimelapseOptions>,
}

/// What the export pull tells the portal this node can cut besides plain
/// clips (in the pull's `known` option, which portals without timelapse
/// accept and ignore): a portal hands out timelapse jobs only to nodes that
/// say so, so an older node never turns one into a month-long clip.
pub const EXPORT_FORMATS: &str = "timelapse";

/// What a job reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportProgress {
    /// `running`, `ready`, `uploading`, `uploaded`, `failed` or `cancelled`.
    pub state: &'static str,
    /// 0–1, rounded to thousandths (the signature covers its JSON text).
    pub progress: f64,
    pub size_bytes: Option<u64>,
    pub duration_ms: Option<i64>,
    pub first_ms: Option<i64>,
    pub audio: Option<bool>,
    pub error: Option<String>,
}

/// The portal's answer to a report.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ExportReply {
    #[serde(default)]
    pub cancel: bool,
    #[serde(default)]
    pub upload_url: Option<String>,
}

/// What the node needs to cut clips: the export engine, and the camera's
/// recordings overlapping a range.
pub type SegmentLookup = Arc<
    dyn Fn(
            String,
            i64,
            i64,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = std::result::Result<Vec<PlaybackSegment>, String>>
                    + Send,
            >,
        > + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ExportSource {
    pub service: Arc<ExportService>,
    pub segments: SegmentLookup,
}

fn valid_job(job: &ExportJob) -> bool {
    job.export_id.len() == 32
        && job
            .export_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !job.camera_id.is_empty()
        && job.camera_id.len() <= 64
        && job
            .camera_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && job.end_ms > job.start_ms
        && job.max_bytes > 0
        && job.timelapse.map_or(true, |options| {
            options.validate(job.start_ms, job.end_ms).is_ok()
        })
}

/// The signed `export-reports` body. Every field is present (null when
/// unknown) so the signature covers exactly what the cloud's model renders.
pub(super) fn export_report_body(
    key: &ed25519_dalek::SigningKey,
    status: &CloudStatus,
    challenge: &str,
    export_id: &str,
    report: &ExportProgress,
    issued: OffsetDateTime,
) -> Result<Value> {
    let first_at = report
        .first_ms
        .map(|ms| {
            OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
                .map_err(|e| CloudError::Invalid(e.to_string()))
                .and_then(fmt_time)
        })
        .transpose()?;
    let progress = (report.progress.clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
    let error = report.error.as_ref().map(|text| -> String {
        text.chars()
            .filter(|c| !c.is_control())
            .take(MAX_ERROR_CHARS)
            .collect()
    });
    let mut body = json!({
        "protocol_version":1,"tenant_id":status.tenant_id,"node_id":status.node_id,
        "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
        "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
        "challenge":challenge,"export_id":export_id,"state":report.state,"progress":progress,
        "size_bytes":report.size_bytes,"duration_ms":report.duration_ms,"first_at":first_at,
        "audio":report.audio,"error":error,"signature":""
    });
    body["signature"] = Value::String(sign_value(key, &without_signature(&body))?);
    Ok(body)
}

impl CloudNode {
    /// Export jobs waiting for this node, held open by the cloud for up to
    /// `wait` until one arrives. Taking a job starts it on the cloud's side.
    pub async fn pull_exports(&self, wait: Duration) -> Result<Vec<ExportJob>> {
        let outcome = self.request_exports(wait).await;
        self.note(&outcome);
        outcome
    }

    async fn request_exports(&self, wait: Duration) -> Result<Vec<ExportJob>> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, EXPORT_PULL_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = OffsetDateTime::now_utc();
        let mut proof = json!({
            "protocol_version":1,"tenant_id":status.tenant_id,"node_id":node_id,
            "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
            "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
            "challenge":challenge,"signature":""
        });
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        let mut url = endpoint.join(&format!("v1/nodes/{node_id}/export-pulls"))?;
        url.query_pairs_mut()
            .append_pair("wait", &wait.as_secs().to_string())
            .append_pair("known", EXPORT_FORMATS);
        let response = self
            .control_post_json(&endpoint, &client, url, &proof)
            .await?;
        self.mark_contact();
        let jobs: Vec<ExportJob> = serde_json::from_value(response["exports"].clone())
            .map_err(|_| CloudError::Network("invalid export jobs".into()))?;
        if jobs.len() > MAX_RUNNING || !jobs.iter().all(valid_job) {
            return Err(CloudError::Network("invalid export jobs".into()));
        }
        Ok(jobs)
    }

    /// Report on one job; the reply says whether to stop and, for `ready`,
    /// where to upload.
    pub async fn report_export(
        &self,
        export_id: &str,
        report: &ExportProgress,
    ) -> Result<ExportReply> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, EXPORT_REPORT_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let body = export_report_body(
            &key,
            &status,
            &challenge,
            export_id,
            report,
            OffsetDateTime::now_utc(),
        )?;
        let url = endpoint.join(&format!("v1/nodes/{node_id}/export-reports"))?;
        let response = self
            .control_post_json(&endpoint, &client, url, &body)
            .await?;
        self.mark_contact();
        serde_json::from_value(response)
            .map_err(|_| CloudError::Network("invalid export reply".into()))
    }

    fn allows_loopback_http(&self) -> bool {
        self.lock()
            .map(|inner| inner.policy.allow_loopback_http)
            .unwrap_or(false)
    }
}

/// Check an upload URL and build a client pinned to its (public) addresses.
/// No overall timeout: a clip of gigabytes takes as long as the uplink needs.
pub(crate) async fn upload_client(
    raw: &str,
    allow_loopback_http: bool,
) -> Result<(reqwest::Url, reqwest::Client)> {
    let url =
        reqwest::Url::parse(raw).map_err(|_| CloudError::Invalid("invalid upload URL".into()))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(CloudError::Invalid("invalid upload URL".into()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| CloudError::Invalid("invalid upload URL".into()))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| CloudError::Invalid("invalid upload URL".into()))?;
    let addresses: Vec<SocketAddr> = match url.scheme() {
        "http" if allow_loopback_http && host == "127.0.0.1" => {
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)]
        }
        "https" => {
            validate_host(&url)?;
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|e| CloudError::Network(format!("upload DNS resolution failed: {e}")))?
                .collect();
            validate_resolved_ips(&resolved.iter().map(SocketAddr::ip).collect::<Vec<_>>())?;
            resolved
        }
        _ => {
            return Err(CloudError::Invalid(
                "the upload URL must be HTTPS on a public host".into(),
            ))
        }
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .resolve_to_addrs(&host, &addresses)
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))?;
    Ok((url, client))
}

/// PUT `path` (exactly `size` bytes) to the presigned `url`, counting the
/// bytes sent into `sent`.
pub(crate) async fn upload_file(
    client: &reqwest::Client,
    url: reqwest::Url,
    path: &Path,
    size: u64,
    sent: Arc<AtomicU64>,
) -> Result<()> {
    use futures_util::StreamExt as _;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| CloudError::Store(format!("the clip could not be read: {e}")))?;
    let stream = tokio_util::io::ReaderStream::new(file).inspect(move |chunk| {
        if let Ok(bytes) = chunk {
            sent.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
    });
    let response = client
        .put(url)
        // The URL's signature covers the length: exactly the reported size.
        .header(reqwest::header::CONTENT_LENGTH, size)
        .header(reqwest::header::CONTENT_TYPE, "video/mp4")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .map_err(|e| CloudError::Network(format!("upload failed: {}", e.without_url())))?;
    if !response.status().is_success() {
        return Err(CloudError::Network(format!(
            "upload refused: HTTP {}",
            response.status().as_u16()
        )));
    }
    Ok(())
}

/// Jobs this node is working on, so a job taken twice runs once.
#[derive(Default, Clone)]
pub(crate) struct RunningExports(Arc<Mutex<HashSet<String>>>);

impl RunningExports {
    pub(crate) fn len(&self) -> usize {
        self.0.lock().map(|set| set.len()).unwrap_or(0)
    }

    fn claim(&self, export_id: &str) -> bool {
        self.0
            .lock()
            .map(|mut set| set.insert(export_id.to_owned()))
            .unwrap_or(false)
    }

    fn release(&self, export_id: &str) {
        if let Ok(mut set) = self.0.lock() {
            set.remove(export_id);
        }
    }
}

/// Start `job` unless it already runs (or too many do).
pub(crate) fn start_job(
    node: CloudNode,
    source: ExportSource,
    running: RunningExports,
    job: ExportJob,
) {
    if running.len() >= MAX_RUNNING || !running.claim(&job.export_id) {
        return;
    }
    tokio::spawn(async move {
        let export_id = job.export_id.clone();
        run_job(&node, &source, job).await;
        running.release(&export_id);
    });
}

async fn report(
    node: &CloudNode,
    export_id: &str,
    progress: &ExportProgress,
) -> Option<ExportReply> {
    match node.report_export(export_id, progress).await {
        Ok(reply) => Some(reply),
        Err(error) => {
            tracing::warn!(export_id = %export_id, %error, "Export report not delivered");
            None
        }
    }
}

/// A final report: retried, since the portal otherwise waits minutes to
/// learn the job ended.
async fn report_final(node: &CloudNode, export_id: &str, progress: ExportProgress) {
    for _ in 0..FINAL_REPORT_ATTEMPTS {
        if report(node, export_id, &progress).await.is_some() {
            return;
        }
        tokio::time::sleep(REPORT_INTERVAL).await;
    }
}

fn failed(error: impl Into<String>) -> ExportProgress {
    ExportProgress {
        state: "failed",
        error: Some(error.into()),
        ..ExportProgress::default()
    }
}

fn cancelled() -> ExportProgress {
    ExportProgress {
        state: "cancelled",
        ..ExportProgress::default()
    }
}

async fn run_job(node: &CloudNode, source: &ExportSource, job: ExportJob) {
    let export_id = job.export_id.clone();
    tracing::info!(export_id = %export_id, camera_id = %job.camera_id, "Web portal export started");
    let segments = match (source.segments)(job.camera_id.clone(), job.start_ms, job.end_ms).await {
        Ok(segments) => segments,
        Err(error) => {
            report_final(node, &export_id, failed(error)).await;
            return;
        }
    };
    let request = ExportRequest {
        camera_id: job.camera_id.clone(),
        camera_name: job.camera_id.clone(),
        start_ms: job.start_ms,
        end_ms: job.end_ms,
        output: None,
        overwrite: false,
    };
    let service = Arc::clone(&source.service);
    let timelapse = job.timelapse;
    let started = match tokio::task::spawn_blocking(move || match timelapse {
        Some(options) => service.start_timelapse(&request, segments, options),
        None => service.start(&request, segments),
    })
    .await
    {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            report_final(node, &export_id, failed(error.to_string())).await;
            return;
        }
        Err(_) => {
            report_final(node, &export_id, failed("The export could not start.")).await;
            return;
        }
    };
    let local_id = started.job_id.clone();
    let done = match follow_local_job(node, source, &export_id, &local_id).await {
        Some(done) => done,
        None => return,
    };
    let path = PathBuf::from(&done.path);
    deliver(node, &export_id, &job, &done, &path).await;
    let _ = tokio::fs::remove_file(&path).await;
}

/// Report the local job's progress until it ends; `Some` with its status
/// when the clip is written, `None` when it failed or was cancelled (already
/// reported).
async fn follow_local_job(
    node: &CloudNode,
    source: &ExportSource,
    export_id: &str,
    local_id: &str,
) -> Option<ExportStatus> {
    let mut cancelling = false;
    loop {
        tokio::time::sleep(REPORT_INTERVAL).await;
        let Some(status) = source.service.status(local_id) else {
            report_final(node, export_id, failed("The export was lost.")).await;
            return None;
        };
        match status.state {
            "done" => return Some(status),
            "failed" => {
                let error = status.error.unwrap_or_else(|| "The export failed.".into());
                report_final(node, export_id, failed(error)).await;
                return None;
            }
            "cancelled" => {
                report_final(node, export_id, cancelled()).await;
                return None;
            }
            _ if cancelling => {}
            _ => {
                let progress = ExportProgress {
                    state: "running",
                    progress: status.progress,
                    ..ExportProgress::default()
                };
                if report(node, export_id, &progress)
                    .await
                    .is_some_and(|reply| reply.cancel)
                {
                    tracing::info!(export_id = %export_id, "Web portal export cancelled");
                    source.service.cancel(local_id);
                    cancelling = true;
                }
            }
        }
    }
}

/// Hand a finished clip to object storage.
async fn deliver(
    node: &CloudNode,
    export_id: &str,
    job: &ExportJob,
    done: &ExportStatus,
    path: &Path,
) {
    let size = done.size_bytes.unwrap_or(0);
    if size == 0 {
        report_final(node, export_id, failed("The clip is empty.")).await;
        return;
    }
    if size > job.max_bytes {
        report_final(
            node,
            export_id,
            failed(format!(
                "The clip is larger than {} MB; export a shorter one.",
                job.max_bytes / 1_000_000
            )),
        )
        .await;
        return;
    }
    let ready = ExportProgress {
        state: "ready",
        progress: 1.0,
        size_bytes: Some(size),
        duration_ms: done.duration_ms,
        first_ms: done.first_ms,
        audio: done.audio,
        error: None,
    };
    let reply = report(node, export_id, &ready).await;
    let Some(url) = reply.as_ref().and_then(|reply| reply.upload_url.clone()) else {
        if reply.is_some_and(|reply| reply.cancel) {
            return;
        }
        report_final(node, export_id, failed("No upload address was given.")).await;
        return;
    };
    let (url, client) = match upload_client(&url, node.allows_loopback_http()).await {
        Ok(checked) => checked,
        Err(error) => {
            report_final(node, export_id, failed(error.to_string())).await;
            return;
        }
    };
    let sent = Arc::new(AtomicU64::new(0));
    let upload = upload_file(&client, url, path, size, Arc::clone(&sent));
    tokio::pin!(upload);
    let outcome = loop {
        tokio::select! {
            outcome = &mut upload => break outcome,
            _ = tokio::time::sleep(REPORT_INTERVAL) => {
                let progress = ExportProgress {
                    state: "uploading",
                    progress: sent.load(Ordering::Relaxed) as f64 / size as f64,
                    ..ExportProgress::default()
                };
                if report(node, export_id, &progress).await.is_some_and(|reply| reply.cancel) {
                    tracing::info!(export_id = %export_id, "Web portal export cancelled while uploading");
                    report_final(node, export_id, cancelled()).await;
                    return;
                }
            }
        }
    };
    match outcome {
        Ok(()) => {
            tracing::info!(export_id = %export_id, size, "Web portal export uploaded");
            report_final(
                node,
                export_id,
                ExportProgress {
                    state: "uploaded",
                    progress: 1.0,
                    ..ExportProgress::default()
                },
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(export_id = %export_id, %error, "Web portal export upload failed");
            report_final(node, export_id, failed(error.to_string())).await;
        }
    }
}

/// The scheduler's export loop: pull jobs (long-polling, or on an event when
/// the portal sends them) and start each.
pub(crate) async fn run_loop(
    node: CloudNode,
    source: ExportSource,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let running = RunningExports::default();
    let signals = Arc::clone(&node.signals);
    let mut unsupported_logged = false;
    loop {
        let started = tokio::time::Instant::now();
        let mut pause = EXPORT_PULL_MIN_CYCLE;
        let pushed = node.events_pushed();
        let mut wake = pushed;
        if node.status().phase == LinkPhase::Linked {
            let wait = if pushed {
                Duration::ZERO
            } else {
                EXPORT_PULL_WAIT
            };
            let pull = tokio::time::timeout(wait + Duration::from_secs(5), node.pull_exports(wait));
            let pulled = tokio::select! {
                pulled = pull => pulled,
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break; }
                    continue;
                }
            };
            match pulled {
                Ok(Ok(jobs)) => {
                    unsupported_logged = false;
                    for job in jobs {
                        start_job(node.shared(), source.clone(), running.clone(), job);
                    }
                }
                Ok(Err(CloudError::Network(message))) if message == "HTTP 404 Not Found" => {
                    if !unsupported_logged {
                        tracing::info!("This cloud does not offer clip export");
                        unsupported_logged = true;
                    }
                    pause = Duration::from_secs(300);
                    wake = false;
                }
                Ok(Err(error)) => tracing::warn!(error = %error, "Cloud export poll failed"),
                Err(_) => tracing::warn!("Cloud export poll timed out"),
            }
        }
        let earliest = started + pause;
        let deadline = if wake {
            tokio::time::Instant::now() + super::EVENT_FALLBACK
        } else {
            earliest
        };
        if !super::rest(
            earliest,
            deadline,
            wake.then_some(&signals.exports),
            &mut stop,
        )
        .await
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upload_urls_must_be_public_https_or_development_loopback() {
        for raw in [
            "http://objects.example.com/x?sig=1",
            "https://user:pw@objects.example.com/x",
            "https://10.0.0.8/x",
            "https://localhost/x",
            "ftp://objects.example.com/x",
            "not a url",
        ] {
            assert!(upload_client(raw, true).await.is_err(), "{raw}");
        }
        assert!(
            upload_client("http://127.0.0.1:9000/b/k?X-Amz-Signature=1", false)
                .await
                .is_err()
        );
        let (url, _) = upload_client("http://127.0.0.1:9000/b/k?X-Amz-Signature=1", true)
            .await
            .unwrap();
        assert_eq!(
            url.query(),
            Some("X-Amz-Signature=1"),
            "the signature is kept"
        );
    }

    #[test]
    fn jobs_are_checked_before_anything_runs() {
        let job = ExportJob {
            export_id: "0123456789abcdef0123456789abcdef".into(),
            camera_id: "cam-1".into(),
            start_ms: 1_000,
            end_ms: 2_000,
            max_bytes: 10,
            timelapse: None,
        };
        assert!(valid_job(&job));
        let timelapse = ExportJob {
            end_ms: 3_600_000,
            timelapse: Some(TimelapseOptions {
                interval_sec: 10,
                fps: 30,
            }),
            ..job.clone()
        };
        assert!(valid_job(&timelapse));
        let parsed: ExportJob = serde_json::from_value(serde_json::json!({
            "export_id": job.export_id, "camera_id": "cam-1", "start_ms": 1_000,
            "end_ms": 3_600_000, "max_bytes": 10, "timelapse": {"interval_sec": 10, "fps": 30}
        }))
        .unwrap();
        assert_eq!(parsed, timelapse);
        for bad in [
            ExportJob {
                export_id: "ABC".into(),
                ..job.clone()
            },
            ExportJob {
                camera_id: "../x".into(),
                ..job.clone()
            },
            ExportJob {
                end_ms: 1_000,
                ..job.clone()
            },
            ExportJob {
                max_bytes: 0,
                ..job.clone()
            },
            ExportJob {
                timelapse: Some(TimelapseOptions {
                    interval_sec: 0,
                    fps: 30,
                }),
                ..timelapse.clone()
            },
            ExportJob {
                end_ms: 40 * 24 * 3_600_000,
                ..timelapse.clone()
            },
        ] {
            assert!(!valid_job(&bad), "{bad:?}");
        }
    }

    /// A one-request HTTP server: what the upload sent (head, body).
    async fn receive_one(listener: tokio::net::TcpListener) -> (String, Vec<u8>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut buffer = [0u8; 65_536];
        let (head, length) = loop {
            let n = socket.read(&mut buffer).await.unwrap();
            received.extend_from_slice(&buffer[..n]);
            if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&received[..end]).to_string();
                let length: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .expect("content-length sent");
                received.drain(..end + 4);
                break (head, length);
            }
        };
        while received.len() < length {
            let n = socket.read(&mut buffer).await.unwrap();
            received.extend_from_slice(&buffer[..n]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        (head, received)
    }

    #[tokio::test]
    async fn a_clip_is_put_whole_with_its_exact_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(receive_one(listener));
        let path =
            std::env::temp_dir().join(format!("nw-upload-{}.mp4", uuid::Uuid::new_v4().simple()));
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();
        let (url, client) = upload_client(
            &format!("http://127.0.0.1:{port}/exports/t/e.mp4?X-Amz-Signature=abc"),
            true,
        )
        .await
        .unwrap();
        let sent = Arc::new(AtomicU64::new(0));
        upload_file(&client, url, &path, content.len() as u64, Arc::clone(&sent))
            .await
            .unwrap();
        let (head, body) = server.await.unwrap();
        assert!(
            head.starts_with("PUT /exports/t/e.mp4?X-Amz-Signature=abc HTTP/1.1"),
            "{head}"
        );
        assert!(
            !head.to_ascii_lowercase().contains("transfer-encoding"),
            "not chunked: {head}"
        );
        assert!(head
            .to_ascii_lowercase()
            .contains("content-type: video/mp4"));
        assert_eq!(body, content);
        assert_eq!(sent.load(Ordering::Relaxed), content.len() as u64);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_refused_upload_is_an_error() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
        });
        let path =
            std::env::temp_dir().join(format!("nw-upload-{}.mp4", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, b"clip").unwrap();
        let (url, client) = upload_client(&format!("http://127.0.0.1:{port}/k"), true)
            .await
            .unwrap();
        let error = upload_file(&client, url, &path, 4, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("403"), "{error}");
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod export_report_contract {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;

    const FIXTURE: &str = "src/cloud/fixtures/export-report-v1.json";

    /// Deterministic signed reports. The web portal verifies this exact
    /// fixture (`tests/test_export_contract_fixture.py`), so both sides agree
    /// on the signed bytes: a running report, a ready one with every field,
    /// and a failure with non-ASCII text.
    fn signed_bodies() -> Value {
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let status = CloudStatus {
            phase: LinkPhase::Linked,
            endpoint: None,
            tenant_id: Some("6ba7b811-9dad-11d1-80b4-00c04fd430c8".into()),
            node_id: Some("6ba7b810-9dad-11d1-80b4-00c04fd430c8".into()),
            auth_epoch: Some(3),
            last_check_in: None,
        };
        let issued = OffsetDateTime::from_unix_timestamp(1_790_000_000).expect("valid time");
        let id = "0123456789abcdef0123456789abcdef";
        let sign = |challenge: &str, report: ExportProgress| {
            export_report_body(&key, &status, challenge, id, &report, issued)
                .expect("signed report")
        };
        json!({
            "public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
            "running": sign("challenge-1", ExportProgress { state: "running", progress: 0.4567, ..ExportProgress::default() }),
            "ready": sign("challenge-2", ExportProgress {
                state: "ready", progress: 1.0, size_bytes: Some(5_368_709_120), duration_ms: Some(600_400),
                first_ms: Some(1_789_808_998_500), audio: Some(true), error: None }),
            "failed": sign("challenge-3", ExportProgress {
                state: "failed", error: Some("Les enregistrements sont « image/jpeg »\n".into()),
                ..ExportProgress::default() }),
        })
    }

    #[test]
    fn signed_export_reports_match_the_shared_fixture() {
        let bodies = signed_bodies();
        assert_eq!(
            bodies["running"]["progress"],
            json!(0.457),
            "rounded to thousandths"
        );
        assert_eq!(
            bodies["ready"]["first_at"],
            json!("2026-09-19T09:09:58Z"),
            "whole seconds"
        );
        assert_eq!(
            bodies["failed"]["error"],
            json!("Les enregistrements sont « image/jpeg »"),
            "no control characters"
        );
        let fixture: Value = serde_json::from_str(include_str!("fixtures/export-report-v1.json"))
            .expect("fixture is JSON");
        assert_eq!(
            bodies, fixture,
            "regenerate with: cargo test -- --ignored write_export_report_fixture"
        );
    }

    #[test]
    #[ignore = "writes the shared fixture"]
    fn write_export_report_fixture() {
        let text = serde_json::to_string_pretty(&signed_bodies()).expect("serialize") + "\n";
        std::fs::write(FIXTURE, text).expect("write fixture");
    }
}
