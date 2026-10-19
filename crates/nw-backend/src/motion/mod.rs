//! Opt-in computer-side motion analysis, independent of the recording pipeline.
mod analysis;
pub mod detector;
#[cfg(test)]
mod qualification;
pub mod runtime;

/// Validate user-supplied settings before the legacy config deserializer can
/// discard invalid zones. Config-file compatibility remains separate.
pub fn validate_settings(
    value: &serde_json::Value,
) -> Result<nw_core::camera::CameraMotionConfig, String> {
    use nw_core::camera::{CameraMotionConfig, MotionZone};
    if !value.is_object() {
        return Err("Motion settings must be an object".into());
    }
    let settings: CameraMotionConfig =
        serde_json::from_value(value.clone()).map_err(|_| "Invalid motion settings".to_owned())?;
    if !(1..=10).contains(&settings.sensitivity)
        || !settings.cooldown_sec.is_finite()
        || !(0.0..=600.0).contains(&settings.cooldown_sec)
        || !settings.min_duration_sec.is_finite()
        || !(0.0..=60.0).contains(&settings.min_duration_sec)
    {
        return Err("Motion sensitivity must be 1–10, cooldown 0–600 seconds and minimum duration 0–60 seconds".into());
    }
    if let Some(raw) = value.get("zones").or_else(|| value.get("exclusion_zones")) {
        let zones: Vec<MotionZone> =
            serde_json::from_value(raw.clone()).map_err(|_| "Invalid motion zones".to_owned())?;
        let mut ids = std::collections::HashSet::new();
        if zones.len() > 32
            || zones
                .iter()
                .any(|zone| !zone.is_valid() || zone.points.len() > 64 || !ids.insert(&zone.id))
        {
            return Err(
                "Use at most 32 uniquely named zones, each with 3–64 points inside the picture"
                    .into(),
            );
        }
    }
    Ok(settings)
}
