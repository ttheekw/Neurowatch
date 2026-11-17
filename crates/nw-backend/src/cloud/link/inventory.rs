//! Allowlisted camera inventory reported to NeuroWatch Cloud.
//!
//! Only an opaque camera id, a display name, a coarse status, whether local
//! recording is on, which of its streams browser live view can forward
//! (with their codecs once seen) and each stream's picture size and measured
//! frame rate and bitrate (which Cloud Backup qualities fit it) ever leave
//! the node. Stream URLs, credentials,
//! network addresses and storage paths are never read into this type; unsafe
//! display names are replaced rather than sent. Since 2026-11-17 also the
//! camera's hardware (MAC, serial, maker, model: `camera_identity`), so the
//! cloud notices another camera put in the same place.

use std::collections::{BTreeMap, HashMap, HashSet};

use nw_core::camera::CameraConfig;
use serde::Serialize;

/// Matches the cloud's `MAX_CAMERAS`.
pub const MAX_REPORTED_CAMERAS: usize = 256;
const MAX_NAME_CHARS: usize = 64;

/// The streams browser live view can forward (`"main"`, `"sub"`), each with
/// its codec (`"h264"`, `"h265"`) once seen, else `null`. A stream left out
/// cannot be watched in a browser.
pub type LiveStreams = BTreeMap<&'static str, Option<&'static str>>;

/// A stream's picture size and measured rates, as last seen (`"main"`,
/// `"sub"`); a stream not seen yet is left out.
pub type StreamVideos = BTreeMap<&'static str, StreamVideo>;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct StreamVideo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Pictures a second, to a tenth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
}

impl From<crate::streams::VideoFormat> for StreamVideo {
    fn from(format: crate::streams::VideoFormat) -> Self {
        Self {
            width: format.width.filter(|w| (1..=16_384).contains(w)),
            height: format.height.filter(|h| (1..=16_384).contains(h)),
            fps: format
                .fps
                .filter(|fps| fps.is_finite() && *fps > 0.0 && *fps <= 1000.0)
                .map(|fps| (fps * 10.0).round() / 10.0),
            bitrate_bps: format.video_bps.filter(|bps| *bps <= 10_000_000_000),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CameraSnapshot {
    pub camera_id: String,
    pub name: String,
    pub status: &'static str,
    pub recording: bool,
    /// Left out when not known, so the cloud keeps its earlier assumption.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveStreams>,
    /// Left out when no stream has been seen yet. (Not `video`: the cloud
    /// refuses that key anywhere, as a guard against media in reports.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formats: Option<StreamVideos>,
    /// Which physical camera this is, when known (`camera_identity`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<crate::camera_identity::CameraHardware>,
}

/// Build the cloud snapshot from local config and supervisor states.
#[cfg(test)]
pub fn camera_inventory(
    cameras: &[CameraConfig],
    states: &HashMap<String, String>,
) -> Vec<CameraSnapshot> {
    camera_inventory_with_live(cameras, states, |_| None, |_| None, |_| None)
}

/// [`camera_inventory`], also reporting each camera's live streams.
pub fn camera_inventory_with_live(
    cameras: &[CameraConfig],
    states: &HashMap<String, String>,
    live: impl Fn(&CameraConfig) -> Option<LiveStreams>,
    formats: impl Fn(&CameraConfig) -> Option<StreamVideos>,
    hardware: impl Fn(&CameraConfig) -> Option<crate::camera_identity::CameraHardware>,
) -> Vec<CameraSnapshot> {
    let cameras = &cameras[..cameras.len().min(MAX_REPORTED_CAMERAS)];
    // Ids and names come from configuration order (a replacement id such as
    // `camera-3` must not change); the list is then in wall order, so the
    // portal's plan, which serves the first cameras reported, serves the ones
    // a free build would run.
    let mut listed: Vec<Option<CameraSnapshot>> = cameras
        .iter()
        .zip(reported_ids(cameras))
        .enumerate()
        .map(|(index, (camera, camera_id))| {
            Some(CameraSnapshot {
                camera_id,
                name: safe_name(&camera.name, index),
                status: cloud_status(states.get(&camera.id).map(String::as_str)),
                // Recording now: a segment is open. The camera's own switch is
                // not enough — "record all cameras" records cameras with it off,
                // and an offline camera records nothing.
                recording: states.get(&camera.id).map(String::as_str) == Some("recording_ready"),
                live: live(camera),
                formats: formats(camera).filter(|streams| !streams.is_empty()),
                hardware: hardware(camera).filter(|found| !found.is_empty()),
            })
        })
        .collect();
    crate::edition::wall_order(cameras)
        .into_iter()
        .filter_map(|index| listed.get_mut(index).and_then(Option::take))
        .collect()
}

/// The id each camera is reported under, in order: its own id when that is
/// opaque, else a replacement such as `camera-3`.
fn reported_ids(cameras: &[CameraConfig]) -> Vec<String> {
    // Ids that are reported as-is; a replacement must not reuse one, because
    // the cloud rejects a whole snapshot that repeats a camera id.
    let mut used: HashSet<String> = cameras
        .iter()
        .filter(|camera| is_opaque_id(&camera.id))
        .map(|camera| camera.id.clone())
        .collect();
    cameras
        .iter()
        .enumerate()
        .map(|(index, camera)| {
            if is_opaque_id(&camera.id) {
                camera.id.clone()
            } else {
                replacement_id(index, &mut used)
            }
        })
        .collect()
}

/// Local camera id → the id the inventory reports it under, so other cloud
/// reports name cameras the way the portal knows them.
pub fn reported_camera_ids(cameras: &[CameraConfig]) -> HashMap<String, String> {
    let cameras = &cameras[..cameras.len().min(MAX_REPORTED_CAMERAS)];
    cameras
        .iter()
        .map(|camera| camera.id.clone())
        .zip(reported_ids(cameras))
        .collect()
}

fn cloud_status(state: Option<&str>) -> &'static str {
    match state {
        Some("streaming" | "recording_ready") => "streaming",
        Some("connecting") => "connecting",
        Some("reconnecting") => "reconnecting",
        // The portal knows no "unauthorized" (wrong password): an error.
        Some("error" | "teardown_failed" | crate::pipeline::supervisor::UNAUTHORIZED) => "error",
        Some("offline") => "offline",
        _ => "stopped",
    }
}

fn is_opaque_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn replacement_id(index: usize, used: &mut HashSet<String>) -> String {
    let base = format!("camera-{}", index + 1);
    let mut candidate = base.clone();
    let mut suffix = 2;
    while used.contains(&candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    used.insert(candidate.clone());
    candidate
}

fn safe_name(name: &str, index: usize) -> String {
    let name: String = name.trim().chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim_end();
    let unsafe_text = name.is_empty()
        || name.contains("://")
        || name.contains('@')
        || name.chars().any(char::is_control)
        || contains_ipv4(name);
    if unsafe_text {
        format!("Camera {}", index + 1)
    } else {
        name.to_owned()
    }
}

/// True if the text contains four dot-separated groups of 1–3 digits.
fn contains_ipv4(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .any(|token| {
            let groups: Vec<&str> = token.split('.').collect();
            groups.len() >= 4
                && groups
                    .windows(4)
                    .any(|w| w.iter().all(|g| (1..=3).contains(&g.len())))
        })
}

/// Most recorded ranges reported per camera; matches the cloud's limit.
pub const MAX_COVERAGE_RANGES: usize = 500;

/// When one camera has local recordings: whole-second UTC ranges only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CameraCoverage {
    pub camera_id: String,
    pub ranges: Vec<RecordedRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecordedRange {
    pub start_at: String,
    pub end_at: String,
}

/// Build the cloud coverage report from millisecond ranges per camera.
///
/// Ranges round inwards to whole seconds, so everything reported is really
/// recorded. Cameras whose local id is not an opaque id are left out: the
/// inventory reports them under a replacement id that coverage cannot match.
pub fn camera_coverage(cameras: Vec<(String, Vec<(i64, i64)>)>) -> Vec<CameraCoverage> {
    cameras
        .into_iter()
        .filter(|(camera_id, _)| is_opaque_id(camera_id))
        .take(MAX_REPORTED_CAMERAS)
        .filter_map(|(camera_id, ranges)| {
            let mut ranges: Vec<RecordedRange> = ranges
                .into_iter()
                .filter_map(|(start_ms, end_ms)| {
                    let start =
                        start_ms.div_euclid(1000) + i64::from(start_ms.rem_euclid(1000) != 0);
                    let end = end_ms.div_euclid(1000);
                    if end <= start {
                        return None;
                    }
                    Some(RecordedRange {
                        start_at: utc_second(start)?,
                        end_at: utc_second(end)?,
                    })
                })
                .collect();
            let excess = ranges.len().saturating_sub(MAX_COVERAGE_RANGES);
            ranges.drain(..excess);
            (!ranges.is_empty()).then_some(CameraCoverage { camera_id, ranges })
        })
        .collect()
}

fn utc_second(seconds: i64) -> Option<String> {
    let at = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    at.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

#[cfg(test)]
mod status_tests {
    use super::cloud_status;

    #[test]
    fn cameras_are_reported_in_wall_order_with_their_own_ids() {
        use super::{camera_inventory, reported_camera_ids, CameraConfig, HashMap};
        use nw_core::camera::WallPosition;
        let camera = |id: &str, place: Option<(u32, u32, u32)>| {
            let mut camera = CameraConfig::new(id, format!("rtsp://cam.invalid/{id}"));
            camera.id = id.into();
            camera.wall_position = place.map(|(page, row, col)| WallPosition {
                page,
                row,
                col,
                row_span: 1,
                col_span: 1,
            });
            camera
        };
        let cameras = vec![
            camera("6ba7b810-9dad-11d1-80b4-00c04fd430c1", None),
            camera("6ba7b810-9dad-11d1-80b4-00c04fd430c2", Some((1, 0, 0))),
            camera("6ba7b810-9dad-11d1-80b4-00c04fd430c3", Some((0, 1, 0))),
            camera("6ba7b810-9dad-11d1-80b4-00c04fd430c4", Some((0, 0, 1))),
            camera("cam 5", None),
        ];
        let ids: Vec<String> = camera_inventory(&cameras, &HashMap::new())
            .into_iter()
            .map(|c| c.camera_id)
            .collect();
        let reported = reported_camera_ids(&cameras);
        assert_eq!(
            ids,
            [
                "6ba7b810-9dad-11d1-80b4-00c04fd430c4",
                "6ba7b810-9dad-11d1-80b4-00c04fd430c3",
                "6ba7b810-9dad-11d1-80b4-00c04fd430c2",
                "6ba7b810-9dad-11d1-80b4-00c04fd430c1",
                reported["cam 5"].as_str(),
            ],
            "placed by page, row and column, then unplaced in configuration order"
        );
    }

    #[test]
    fn a_camera_refusing_its_password_is_an_error_to_the_portal() {
        // The portal accepts only its own states; an unknown one would refuse
        // the whole inventory.
        assert_eq!(cloud_status(Some("unauthorized")), "error");
        assert_eq!(cloud_status(Some("offline")), "offline");
        assert_eq!(cloud_status(Some("recording_ready")), "streaming");
    }
}
