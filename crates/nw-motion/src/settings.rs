//! A binding's configuration document. Same field names as a NeuroWatch
//! camera's `motion` settings, so existing settings migrate unchanged.
use serde::{Deserialize, Serialize};

/// Whether a zone is watched or ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ZoneKind {
    #[default]
    Include,
    Exclude,
}

/// A polygon on the picture, in coordinates normalised to 0..1, so it
/// survives a change of stream resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: ZoneKind,
    pub points: Vec<[f32; 2]>,
}

impl Zone {
    /// At least three points, every one inside the picture.
    pub fn is_valid(&self) -> bool {
        !self.id.is_empty()
            && self.points.len() >= 3
            && self
                .points
                .iter()
                .all(|[x, y]| (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y))
    }
}

/// Fields left out take their defaults, so a client may send only what it sets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotionSettings {
    pub sensitivity: u8,
    pub cooldown_sec: f32,
    pub min_duration_sec: f32,
    /// Decode independently decodable pictures only. Short movements between
    /// keyframes may be missed; normal analysis remains the default.
    pub low_cpu: bool,
    /// What decodes the camera's video (Automatic: the first hardware decoder
    /// that works here, else the CPU).
    pub decoder: crate::decoding::DecoderChoice,
    /// Include and exclude zones. Invalid entries from a stored document are
    /// dropped; [`validate`] refuses them in user input instead.
    #[serde(alias = "exclusion_zones", deserialize_with = "deserialize_zones")]
    pub zones: Vec<Zone>,
}

impl Default for MotionSettings {
    fn default() -> Self {
        Self {
            sensitivity: 5,
            cooldown_sec: 10.0,
            min_duration_sec: 0.5,
            low_cpu: false,
            decoder: crate::decoding::DecoderChoice::default(),
            zones: vec![],
        }
    }
}

fn deserialize_zones<'de, D>(deserializer: D) -> Result<Vec<Zone>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| serde_json::from_value::<Zone>(value).ok())
        .filter(Zone::is_valid)
        .collect())
}

/// Validate user-supplied settings before the lenient deserializer can
/// discard invalid zones.
pub fn validate(value: &serde_json::Value) -> Result<MotionSettings, String> {
    if !value.is_object() {
        return Err("Motion settings must be an object".into());
    }
    let settings: MotionSettings =
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
        let zones: Vec<Zone> =
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_camera_motion_document_reads_as_settings() {
        // As NeuroWatch stores it today, with the fields the plugin ignores.
        let stored = json!({
            "enabled": false, "source": "builtin", "sensitivity": 7, "cooldown_sec": 4.0,
            "min_duration_sec": 1.0, "low_cpu": true,
            "zones": [{"id": "door", "name": "Door", "kind": "exclude",
                       "points": [[0.0, 0.0], [0.5, 0.0], [0.5, 0.5]]}]
        });
        let settings: MotionSettings = serde_json::from_value(stored).unwrap();
        assert_eq!(settings.sensitivity, 7);
        assert!(settings.low_cpu);
        assert_eq!(settings.zones[0].kind, ZoneKind::Exclude);
        // Saved before the decoder was a setting: Automatic.
        assert_eq!(settings.decoder, crate::decoding::DecoderChoice::Auto);
    }

    #[test]
    fn the_decoder_is_a_setting_and_an_unknown_one_is_refused() {
        let chosen = validate(&json!({"decoder": "nvdec"})).unwrap();
        assert_eq!(chosen.decoder, crate::decoding::DecoderChoice::Nvdec);
        assert!(validate(&json!({"decoder": "quantum"})).is_err());
    }

    #[test]
    fn stored_documents_drop_bad_zones_and_read_the_legacy_name() {
        let settings: MotionSettings = serde_json::from_value(json!({
            "exclusion_zones": [{"id": "a", "points": [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]},
                                {"id": "b", "points": [[0.0, 0.0], [2.0, 0.0], [1.0, 1.0]]},
                                {"x": 1}]
        }))
        .unwrap();
        assert_eq!(settings.zones.len(), 1);
        assert_eq!(settings.zones[0].id, "a");
        assert_eq!(settings.sensitivity, 5);
    }

    #[test]
    fn validation_refuses_what_the_lenient_reader_would_hide() {
        let square = json!([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]);
        assert!(validate(&json!({})).is_ok());
        assert!(validate(&json!([])).is_err());
        assert!(validate(&json!({"sensitivity": 0})).is_err());
        assert!(validate(&json!({"cooldown_sec": 601.0})).is_err());
        assert!(validate(&json!({"min_duration_sec": -1.0})).is_err());
        assert!(
            validate(&json!({"zones": [{"id": "a", "points": [[0.0, 0.0], [1.0, 1.0]]}]})).is_err()
        );
        assert!(validate(
            &json!({"zones": [{"id": "a", "points": square}, {"id": "a", "points": square}]})
        )
        .is_err());
        let many: Vec<_> = (0..33)
            .map(|i| json!({"id": i.to_string(), "points": square}))
            .collect();
        assert!(validate(&json!({ "zones": many })).is_err());
    }
}
