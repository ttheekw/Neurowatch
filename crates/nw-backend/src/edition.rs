//! Free and unlimited builds.
//!
//! A free build runs up to [`FREE_CAMERA_LIMIT`] cameras; a build with the
//! `unlimited` Cargo feature (sold at [`DOWNLOADS_URL`]) runs any number.
//! Cameras beyond the limit are never deleted: [`AppConfig::cameras`] holds
//! the ones that run (the first ones in wall order) and
//! [`AppConfig::cameras_over_limit`] the rest, saved in the same file, so an
//! unlimited build (or removing a camera) brings them back as they were.

use nw_core::camera::CameraConfig;
use nw_core::ipc::RpcError;
use serde_json::json;

use crate::config::AppConfig;

/// Cameras a free build runs.
pub const FREE_CAMERA_LIMIT: usize = 16;

/// The most cameras this build runs; `None`: any number.
pub const CAMERA_LIMIT: Option<usize> = if cfg!(feature = "unlimited") {
    None
} else {
    Some(FREE_CAMERA_LIMIT)
};

/// `"free"` or `"unlimited"`, as `system.status` reports it.
pub const EDITION: &str = if cfg!(feature = "unlimited") {
    "unlimited"
} else {
    "free"
};

/// Where NeuroWatch (and NeuroWatch Unlimited) is downloaded.
pub const DOWNLOADS_URL: &str = "https://neurowatch.app/downloads";

/// JSON-RPC error code of an add refused by the camera limit.
pub const CAMERA_LIMIT_REACHED: i32 = -32010;

/// The refusal of a camera beyond the limit; `data` carries the limit and
/// where to get the unlimited build.
pub fn limit_reached(limit: usize) -> RpcError {
    RpcError {
        code: CAMERA_LIMIT_REACHED,
        message: format!(
            "This free version of NeuroWatch runs up to {limit} cameras. \
             NeuroWatch Unlimited has no software camera limit; capacity depends on \
             your hardware: {DOWNLOADS_URL}"
        ),
        data: Some(json!({ "camera_limit": limit, "downloads_url": DOWNLOADS_URL })),
    }
}

/// Bring `config` within this build's limit (see [`apply_limit`]). Returns the
/// cameras that now run and did not before (taken from the held ones).
pub fn apply(config: &mut AppConfig) -> Vec<CameraConfig> {
    apply_limit(config, CAMERA_LIMIT)
}

/// Keep at most `limit` cameras in `config.cameras`. With too many, the first
/// ones in wall order run and the rest are held in `cameras_over_limit`; with
/// room, held cameras run again (first in wall order first). Configuration
/// order is kept within each list. Returns the cameras moved from held to
/// running.
pub fn apply_limit(config: &mut AppConfig, limit: Option<usize>) -> Vec<CameraConfig> {
    let Some(limit) = limit else {
        let held = std::mem::take(&mut config.cameras_over_limit);
        config.cameras.extend(held.iter().cloned());
        return held;
    };
    if config.cameras.len() > limit {
        let running = std::mem::take(&mut config.cameras);
        let (keep, hold) = split_by_wall_order(running, limit);
        config.cameras = keep;
        let mut held = hold;
        held.append(&mut config.cameras_over_limit);
        config.cameras_over_limit = held;
        return Vec::new();
    }
    let room = limit - config.cameras.len();
    if room == 0 || config.cameras_over_limit.is_empty() {
        return Vec::new();
    }
    let held = std::mem::take(&mut config.cameras_over_limit);
    let (promoted, still_held) = split_by_wall_order(held, room);
    config.cameras.extend(promoted.iter().cloned());
    config.cameras_over_limit = still_held;
    promoted
}

/// The first `count` cameras in wall order (placed cameras by page, row and
/// column, then unplaced ones in configuration order), and the rest; each part
/// in configuration order.
fn split_by_wall_order(
    cameras: Vec<CameraConfig>,
    count: usize,
) -> (Vec<CameraConfig>, Vec<CameraConfig>) {
    let mut order: Vec<usize> = (0..cameras.len()).collect();
    order.sort_by_key(|&index| {
        let place = cameras[index]
            .wall_position
            .as_ref()
            .map(|p| (p.page, p.row, p.col));
        (place.is_none(), place.unwrap_or_default(), index)
    });
    let chosen: std::collections::HashSet<usize> = order.into_iter().take(count).collect();
    let mut keep = Vec::new();
    let mut rest = Vec::new();
    for (index, camera) in cameras.into_iter().enumerate() {
        if chosen.contains(&index) {
            keep.push(camera);
        } else {
            rest.push(camera);
        }
    }
    (keep, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::camera::WallPosition;

    fn camera(id: &str, place: Option<(u32, u32, u32)>) -> CameraConfig {
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
    }

    fn ids(cameras: &[CameraConfig]) -> Vec<&str> {
        cameras.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn a_free_build_runs_the_first_cameras_in_wall_order_and_keeps_the_rest() {
        let mut config = AppConfig {
            cameras: vec![
                camera("unplaced", None),
                camera("page1", Some((1, 0, 0))),
                camera("page0-b", Some((0, 0, 1))),
                camera("page0-a", Some((0, 0, 0))),
            ],
            ..AppConfig::default()
        };
        assert!(apply_limit(&mut config, Some(2)).is_empty());
        assert_eq!(ids(&config.cameras), ["page0-b", "page0-a"]);
        assert_eq!(ids(&config.cameras_over_limit), ["unplaced", "page1"]);

        // Removing one makes room: the next in wall order runs again.
        config.cameras.retain(|c| c.id != "page0-a");
        let promoted = apply_limit(&mut config, Some(2));
        assert_eq!(ids(&promoted), ["page1"]);
        assert_eq!(ids(&config.cameras), ["page0-b", "page1"]);
        assert_eq!(ids(&config.cameras_over_limit), ["unplaced"]);
    }

    #[test]
    fn an_unlimited_build_runs_every_held_camera() {
        let mut config = AppConfig {
            cameras: vec![camera("a", None)],
            cameras_over_limit: vec![camera("b", None), camera("c", None)],
            ..AppConfig::default()
        };
        let promoted = apply_limit(&mut config, None);
        assert_eq!(ids(&promoted), ["b", "c"]);
        assert_eq!(ids(&config.cameras), ["a", "b", "c"]);
        assert!(config.cameras_over_limit.is_empty());
    }

    #[test]
    fn held_cameras_survive_a_save_and_load() {
        let path = std::env::temp_dir().join(format!("nw-edition-{}.json", uuid::Uuid::new_v4()));
        let config = AppConfig {
            cameras: (0..FREE_CAMERA_LIMIT + 2)
                .map(|i| camera(&format!("cam-{i:02}"), None))
                .collect(),
            ..AppConfig::default()
        };
        config.save(&path).unwrap();
        let loaded = AppConfig::load_or_default(&path).unwrap();
        let total = loaded.cameras.len() + loaded.cameras_over_limit.len();
        assert_eq!(total, FREE_CAMERA_LIMIT + 2, "nothing is lost");
        if CAMERA_LIMIT.is_some() {
            assert_eq!(loaded.cameras.len(), FREE_CAMERA_LIMIT);
            assert_eq!(ids(&loaded.cameras_over_limit), ["cam-16", "cam-17"]);
        }
        loaded.save(&path).unwrap();
        let again = AppConfig::load_or_default(&path).unwrap();
        assert_eq!(again.cameras.len() + again.cameras_over_limit.len(), total);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_refusal_says_where_to_get_unlimited() {
        let error = limit_reached(16);
        assert_eq!(error.code, CAMERA_LIMIT_REACHED);
        assert!(error.message.contains("16 cameras"));
        assert_eq!(error.data.unwrap()["downloads_url"], DOWNLOADS_URL);
    }
}
