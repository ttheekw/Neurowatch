use nw_backend::config::AppConfig;
use nw_core::camera::RuntimeMode;

#[test]
fn legacy_config_without_runtime_mode_stays_local_standalone() {
    let mut legacy = serde_json::to_value(AppConfig::default()).expect("default config serializes");
    legacy
        .as_object_mut()
        .expect("config is a JSON object")
        .remove("runtime_mode");

    let config: AppConfig = serde_json::from_value(legacy).expect("legacy config deserializes");

    assert_eq!(config.runtime_mode, RuntimeMode::LocalStandalone);
}

#[test]
fn default_runtime_mode_does_not_change_existing_config_output() {
    let config = AppConfig::default();
    let serialized = serde_json::to_value(config).expect("default config serializes");

    assert_eq!(
        serialized.get("runtime_mode"),
        None,
        "the implicit local compatibility mode must not rewrite existing config output"
    );
}
