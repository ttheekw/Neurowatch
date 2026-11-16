//! Hosted plugin services on the recorder. Every [`CHECK_INTERVAL`] the portal
//! says which services this recorder should use (`link::services`); each
//! plugin service becomes a **managed** plugin server. Its address and key
//! come from the portal's reply, so it needs no local approval, and nobody
//! edits it here. A service no longer offered is removed, or switched off
//! if cameras are bound to it, so their settings are there if it returns.
//!
//! The portal also names the cameras each plugin is for, and those are bound
//! here: nobody binds a hosted plugin by hand. A camera that leaves the list
//! is switched off with its settings kept, and comes back on if it returns.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, RwLock};

use super::link::services::{ServiceEntry, BACKUP_SERVICE};
use super::link::{CloudNode, LinkPhase};
use crate::app_state::AppState;
use crate::plugin_host::{Managed, PluginBinding, PluginServer, PluginsConfig};

pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);
const UNAVAILABLE_PAUSE: Duration = Duration::from_secs(300);
/// What a hosted plugin may ask for. Decoded pictures need the same machine.
const HOSTED_PERMISSIONS: [&str; 4] = ["video", "events", "overlays", "web_ui"];
const SERVER_PREFIX: &str = "hosted.";
const MAX_ID: usize = 64;
/// The longest catalog name kept (the portal allows 80 characters).
const MAX_LABEL: usize = 80;

/// The catalog's name for a hosted plugin, if it is one fit to show.
fn label(entry: &ServiceEntry) -> Option<String> {
    let label = entry.label.as_deref()?.trim();
    (!label.is_empty() && !label.chars().any(char::is_control))
        .then(|| label.chars().take(MAX_LABEL).collect())
}

/// The plugin address of a service entry: only encrypted transports.
fn address(entry: &ServiceEntry) -> Option<String> {
    entry
        .endpoint
        .quic
        .as_ref()
        .map(|address| format!("quic:{address}"))
        .or_else(|| {
            entry
                .endpoint
                .tls
                .as_ref()
                .map(|address| format!("tls:{address}"))
        })
}

/// Make one hosted plugin's bindings match the cameras the portal names.
/// Only `known` cameras are bound: a binding to any other is invalid.
fn bind_cameras(next: &mut PluginsConfig, server: &str, cameras: &[String], known: &[String]) {
    for camera in cameras.iter().filter(|camera| known.contains(camera)) {
        let existing = next
            .bindings
            .iter_mut()
            .find(|binding| binding.server == server && &binding.camera_id == camera);
        match existing {
            // Off by its owner's choice stays off; off because it lapsed does not.
            Some(binding) => {
                if binding.withdrawn {
                    binding.withdrawn = false;
                    binding.enabled = true;
                }
            }
            None => next.bindings.push(PluginBinding {
                server: server.to_owned(),
                camera_id: camera.clone(),
                enabled: true,
                config: serde_json::json!({}),
                stream: None,
                wall_layers: None,
                own_decoder: false,
                withdrawn: false,
            }),
        }
    }
    for binding in next
        .bindings
        .iter_mut()
        .filter(|binding| binding.server == server && !cameras.contains(&binding.camera_id))
    {
        binding.enabled = false;
        binding.withdrawn = true;
    }
}

/// The settings with the portal's answer applied; `None` when nothing changes.
/// `known`: this recorder's cameras.
pub fn reconcile(
    config: &PluginsConfig,
    entries: &[ServiceEntry],
    known: &[String],
) -> Option<PluginsConfig> {
    let mut next = config.clone();
    let mut offered = Vec::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.service != BACKUP_SERVICE)
    {
        let id = format!("{SERVER_PREFIX}{}", entry.service);
        let (Some(address), Some(public_key)) = (address(entry), entry.public_key.clone()) else {
            tracing::warn!(service = %entry.service, "Hosted service has no encrypted address or key");
            continue;
        };
        if id.len() > MAX_ID {
            continue;
        }
        offered.push(id.clone());
        bind_cameras(&mut next, &id, &entry.cameras, known);
        let managed = Managed {
            service: entry.service.clone(),
            instance: entry.instance.clone(),
            label: label(entry),
        };
        let approval = nw_analytics_protocol::auth::Approval {
            public_key,
            permissions: HOSTED_PERMISSIONS.map(str::to_owned).to_vec(),
        };
        match next.servers.iter_mut().find(|server| server.id == id) {
            Some(server) => {
                server.address = address;
                server.approval = Some(approval);
                server.managed = Some(managed);
                server.enabled = true;
            }
            None => next.servers.push(PluginServer {
                id,
                address,
                enabled: true,
                approval: Some(approval),
                credential_revision: 0,
                plugin_id: None,
                managed: Some(managed),
            }),
        }
    }
    let bound: Vec<String> = next
        .bindings
        .iter()
        .map(|binding| binding.server.clone())
        .collect();
    next.servers.retain_mut(|server| {
        if server.managed.is_none() || offered.contains(&server.id) {
            return true;
        }
        server.enabled = false;
        bound.contains(&server.id)
    });
    // A plugin no longer provided covers none of its cameras: they are off
    // with their settings kept, and come back on if it returns.
    let gone: Vec<String> = next
        .servers
        .iter()
        .filter(|server| server.managed.is_some() && !offered.contains(&server.id))
        .map(|server| server.id.clone())
        .collect();
    for binding in next
        .bindings
        .iter_mut()
        .filter(|binding| binding.enabled && gone.contains(&binding.server))
    {
        binding.enabled = false;
        binding.withdrawn = true;
    }
    (next != *config).then_some(next)
}

fn tickets(entries: &[ServiceEntry]) -> HashMap<String, String> {
    entries
        .iter()
        .filter(|entry| entry.service != BACKUP_SERVICE)
        .map(|entry| {
            (
                format!("{SERVER_PREFIX}{}", entry.service),
                entry.ticket.clone(),
            )
        })
        .collect()
}

pub(crate) async fn run_loop(
    node: CloudNode,
    state: Arc<RwLock<AppState>>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let mut pause = CHECK_INTERVAL;
        if node.status().phase == LinkPhase::Linked {
            match node.hosted_services().await {
                Ok(entries) => apply(&state, &entries).await,
                Err(error) => {
                    pause = UNAVAILABLE_PAUSE.min(pause * 2);
                    // Tickets lapse by themselves; settings stay as they are.
                    tracing::debug!(%error, "Hosted services could not be checked");
                }
            }
        } else {
            // Unlinked: nothing hosted may run.
            apply(&state, &[]).await;
        }
        tokio::select! {
            _ = tokio::time::sleep(pause) => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
        }
    }
}

async fn apply(state: &RwLock<AppState>, entries: &[ServiceEntry]) {
    let mut st = state.write().await;
    st.plugins.set_tickets(tickets(entries));
    let cameras: Vec<String> = st
        .config
        .cameras
        .iter()
        .map(|camera| camera.id.clone())
        .collect();
    let Some(next) = reconcile(&st.config.plugins, entries, &cameras) else {
        return;
    };
    if let Err(problem) = next.validate(cameras.iter().map(String::as_str)) {
        tracing::warn!(%problem, "Hosted services could not be applied");
        return;
    }
    let previous = std::mem::replace(&mut st.config.plugins, next);
    if let Err(error) = st.save_config() {
        st.config.plugins = previous;
        tracing::warn!(%error, "Hosted services could not be saved");
    }
}

#[cfg(test)]
mod tests {
    use super::super::link::services::ServiceEndpoint;
    use super::*;
    use crate::plugin_host::PluginBinding;

    fn entry(service: &str, instance: &str) -> ServiceEntry {
        ServiceEntry {
            service: service.into(),
            instance: instance.into(),
            endpoint: ServiceEndpoint {
                quic: Some(format!("{instance}:443")),
                ..Default::default()
            },
            public_key: Some("ab".repeat(32)),
            ca_pem: None,
            ticket: "a.b".into(),
            cameras: Vec::new(),
            label: None,
        }
    }

    fn known() -> Vec<String> {
        vec!["cam-1".into(), "cam-2".into()]
    }

    fn offered(config: &PluginsConfig, entries: &[ServiceEntry]) -> Option<PluginsConfig> {
        reconcile(config, entries, &known())
    }

    fn binding(server: &str) -> PluginBinding {
        serde_json::from_value(serde_json::json!({"server": server, "camera_id": "cam-1"})).unwrap()
    }

    #[test]
    fn a_service_becomes_a_managed_server_needing_no_approval() {
        let next = offered(&PluginsConfig::default(), &[entry("nw.vcount", "v1")]).unwrap();
        let server = &next.servers[0];
        assert_eq!(server.id, "hosted.nw.vcount");
        assert_eq!(server.address, "quic:v1:443");
        assert_eq!(server.managed.as_ref().unwrap().instance, "v1");
        assert_eq!(
            server.approval.as_ref().unwrap().public_key,
            "ab".repeat(32)
        );
    }

    #[test]
    fn a_hosted_plugin_is_shown_by_its_catalog_name() {
        let named = ServiceEntry {
            label: Some("  Vehicle counting ".into()),
            ..entry("nw.vcount", "v1")
        };
        let next = offered(&PluginsConfig::default(), std::slice::from_ref(&named)).unwrap();
        assert_eq!(next.servers[0].shown_name(None), "Vehicle counting");
        assert_eq!(
            next.servers[0].shown_name(Some("nw-vcount")),
            "Vehicle counting"
        );
        // A renamed plugin follows the portal.
        let renamed = ServiceEntry {
            label: Some("Traffic counts".into()),
            ..named
        };
        let next = offered(&next, &[renamed]).unwrap();
        assert_eq!(next.servers[0].shown_name(None), "Traffic counts");
        // No name, or one unfit to show: the plugin's own, else the id.
        let bad = ServiceEntry {
            label: Some("bad\nname".into()),
            ..entry("nw.vcount", "v1")
        };
        let next = offered(&next, &[bad]).unwrap();
        assert_eq!(next.servers[0].shown_name(Some("Counting")), "Counting");
        assert_eq!(next.servers[0].shown_name(None), "hosted.nw.vcount");
    }

    #[test]
    fn the_same_answer_changes_nothing() {
        let entries = [entry("nw.vcount", "v1")];
        let next = offered(&PluginsConfig::default(), &entries).unwrap();
        assert_eq!(offered(&next, &entries), None);
    }

    #[test]
    fn a_move_to_another_instance_follows_the_portal() {
        let next = offered(&PluginsConfig::default(), &[entry("nw.vcount", "v1")]).unwrap();
        let moved = offered(&next, &[entry("nw.vcount", "v2")]).unwrap();
        assert_eq!(moved.servers[0].address, "quic:v2:443");
    }

    #[test]
    fn backup_is_not_a_plugin() {
        assert_eq!(
            offered(&PluginsConfig::default(), &[entry(BACKUP_SERVICE, "i1")]),
            None
        );
    }

    #[test]
    fn a_service_withdrawn_is_removed_but_bound_cameras_keep_their_settings() {
        let mut config = offered(&PluginsConfig::default(), &[entry("nw.vcount", "v1")]).unwrap();
        let unbound = offered(&config, &[]).unwrap();
        assert!(unbound.servers.is_empty());
        config.bindings.push(binding("hosted.nw.vcount"));
        let kept = offered(&config, &[]).unwrap();
        assert!(!kept.servers[0].enabled);
        assert_eq!(kept.bindings.len(), 1);
        let back = offered(&kept, &[entry("nw.vcount", "v1")]).unwrap();
        assert!(back.servers[0].enabled);
    }

    #[test]
    fn a_plugin_taken_away_takes_its_cameras_off_and_brings_them_back() {
        let mut config = offered(
            &PluginsConfig::default(),
            &[for_cameras(&["cam-1", "cam-2"])],
        )
        .unwrap();
        config.bindings[1].enabled = false; // switched off by its owner
        let gone = offered(&config, &[]).unwrap();
        assert!(!gone.servers[0].enabled);
        assert!(!gone.bindings[0].enabled && gone.bindings[0].withdrawn);
        assert!(!gone.bindings[1].enabled && !gone.bindings[1].withdrawn);
        let back = offered(&gone, &[for_cameras(&["cam-1", "cam-2"])]).unwrap();
        assert!(back.bindings[0].enabled && !back.bindings[0].withdrawn);
        assert!(!back.bindings[1].enabled, "the owner's choice stays");
    }

    fn for_cameras(cameras: &[&str]) -> ServiceEntry {
        ServiceEntry {
            cameras: cameras.iter().map(|camera| (*camera).to_owned()).collect(),
            ..entry("nw.vcount", "v1")
        }
    }

    #[test]
    fn the_cameras_the_portal_names_are_bound_without_anyone_doing_it() {
        let next = offered(
            &PluginsConfig::default(),
            &[for_cameras(&["cam-1", "cam-2"])],
        )
        .unwrap();
        let bound: Vec<_> = next
            .bindings
            .iter()
            .map(|b| (&*b.server, &*b.camera_id, b.enabled))
            .collect();
        assert_eq!(
            bound,
            [
                ("hosted.nw.vcount", "cam-1", true),
                ("hosted.nw.vcount", "cam-2", true)
            ]
        );
    }

    #[test]
    fn a_camera_this_recorder_does_not_have_is_not_bound() {
        let next = offered(&PluginsConfig::default(), &[for_cameras(&["gone"])]).unwrap();
        assert!(next.bindings.is_empty());
    }

    #[test]
    fn a_camera_taken_off_stops_keeping_its_settings_and_comes_back() {
        let mut config = offered(
            &PluginsConfig::default(),
            &[for_cameras(&["cam-1", "cam-2"])],
        )
        .unwrap();
        config.bindings[1].config = serde_json::json!({"fps": 5});
        let off = offered(&config, &[for_cameras(&["cam-1"])]).unwrap();
        assert!(!off.bindings[1].enabled && off.bindings[1].withdrawn);
        assert_eq!(off.bindings[1].config, serde_json::json!({"fps": 5}));
        let back = offered(&off, &[for_cameras(&["cam-1", "cam-2"])]).unwrap();
        assert!(back.bindings[1].enabled && !back.bindings[1].withdrawn);
        assert_eq!(back.bindings[1].config, serde_json::json!({"fps": 5}));
    }

    #[test]
    fn a_camera_switched_off_by_its_owner_stays_off() {
        let mut config = offered(&PluginsConfig::default(), &[for_cameras(&["cam-1"])]).unwrap();
        config.bindings[0].enabled = false;
        assert_eq!(offered(&config, &[for_cameras(&["cam-1"])]), None);
    }

    #[test]
    fn hand_added_plugins_are_left_alone() {
        let config: PluginsConfig = serde_json::from_value(serde_json::json!({
            "servers": [{"id": "motion", "address": "unix:/tmp/m.sock"}]}))
        .unwrap();
        assert_eq!(offered(&config, &[]), None);
    }
}
