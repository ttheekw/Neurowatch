//! Plugins over the network that cannot keep up (2026-11-16, the user's rule:
//! a delay sending video to a hosted or remote plugin raises an event and a
//! notification). The recorder never waits for a plugin: when the connection
//! is too slow, the binding's queue skips to the next keyframe and counts what
//! it gave up (`BindingView::dropped`). A binding that lost at least
//! [`LAG_FRACTION`] of its video over the last [`LAG_WINDOW`] is lagging until
//! a whole window passes with less.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::{PluginServer, ServerStatus};

/// How long a loss is measured over before it counts (and how long it must be
/// gone before it ends).
pub const LAG_WINDOW: Duration = Duration::from_secs(60);
/// The share of the video lost that makes a binding lagging.
pub const LAG_FRACTION: f64 = 0.05;
/// Fewer lost units than this in a window is never lagging (a reconnect).
const LAG_MIN_DROPS: u64 = 10;

/// A binding to a plugin over the network losing video.
#[derive(Debug, Clone, PartialEq)]
pub struct LaggingBinding {
    pub server: String,
    pub name: String,
    pub camera_id: String,
    /// What was lost over the window, 0–100.
    pub percent_lost: u32,
}

/// (when, sent, dropped).
type Sample = (Instant, u64, u64);

#[derive(Default)]
pub(super) struct LagWatch {
    /// (server, camera) → samples of (when, sent, dropped), oldest first.
    samples: HashMap<(String, String), VecDeque<Sample>>,
}

/// Reached over the network (or provided by NeuroWatch), where a slow
/// connection is the likely cause; a local plugin falling behind is busy,
/// not short of bandwidth.
fn over_network(server: &PluginServer) -> bool {
    server.managed.is_some()
        || server.address.starts_with("tls:")
        || server.address.starts_with("quic:")
}

impl LagWatch {
    /// Note the bindings' counts at `now` and answer the lagging ones, in
    /// config order.
    pub fn lagging(
        &mut self,
        order: &[PluginServer],
        status: &HashMap<String, ServerStatus>,
        now: Instant,
    ) -> Vec<LaggingBinding> {
        let mut seen = std::collections::HashSet::new();
        let mut lagging = Vec::new();
        for server in order.iter().filter(|s| s.enabled && over_network(s)) {
            let Some(current) = status.get(&server.id) else {
                continue;
            };
            if current.state != "connected" {
                continue; // offline is its own event
            }
            let bindings: &BTreeMap<_, _> = &current.bindings;
            for (camera, binding) in bindings {
                let key = (server.id.clone(), camera.clone());
                seen.insert(key.clone());
                let samples = self.samples.entry(key).or_default();
                // Counters restart with a new session: start measuring again.
                if samples.back().is_some_and(|&(_, sent, dropped)| {
                    binding.sent < sent || binding.dropped < dropped
                }) {
                    samples.clear();
                }
                samples.push_back((now, binding.sent, binding.dropped));
                while samples.len() > 2
                    && samples
                        .get(1)
                        .is_some_and(|&(at, _, _)| now.saturating_duration_since(at) >= LAG_WINDOW)
                {
                    samples.pop_front();
                }
                let (Some(&(first_at, sent0, dropped0)), Some(&(_, sent1, dropped1))) =
                    (samples.front(), samples.back())
                else {
                    continue;
                };
                if now.saturating_duration_since(first_at) < LAG_WINDOW {
                    continue; // not measured for a whole window yet
                }
                let lost = dropped1 - dropped0;
                let total = lost + (sent1 - sent0);
                if lost >= LAG_MIN_DROPS && lost as f64 >= LAG_FRACTION * total as f64 {
                    lagging.push(LaggingBinding {
                        server: server.id.clone(),
                        name: server.shown_name(current.manifest.as_ref().map(|m| m.name.as_str())),
                        camera_id: camera.clone(),
                        percent_lost: ((lost * 100) / total.max(1)) as u32,
                    });
                }
            }
        }
        self.samples.retain(|key, _| seen.contains(key));
        lagging
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_host::BindingView;

    fn server(address: &str) -> PluginServer {
        serde_json::from_value(serde_json::json!({
            "id": "remote", "address": address, "enabled": true
        }))
        .unwrap()
    }

    fn status(sent: u64, dropped: u64) -> HashMap<String, ServerStatus> {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "cam-1".to_owned(),
            BindingView {
                sent,
                dropped,
                ..BindingView::default()
            },
        );
        HashMap::from([(
            "remote".to_owned(),
            ServerStatus {
                id: "remote".into(),
                state: "connected",
                bindings,
                ..ServerStatus::default()
            },
        )])
    }

    /// Feed one sample every 5 s (the health loop's pace) with these
    /// per-sample increments; answer whether it lags at the end.
    fn run(
        watch: &mut LagWatch,
        order: &[PluginServer],
        start: Instant,
        steps: &[(u64, u64)],
    ) -> (Vec<LaggingBinding>, Instant, u64, u64) {
        let (mut sent, mut dropped, mut at, mut last) = (0, 0, start, vec![]);
        for &(s, d) in steps {
            sent += s;
            dropped += d;
            last = watch.lagging(order, &status(sent, dropped), at);
            at += Duration::from_secs(5);
        }
        (last, at, sent, dropped)
    }

    #[test]
    fn a_remote_plugin_losing_video_for_a_minute_is_lagging_until_it_keeps_up() {
        let order = [server("quic:plugins.example.test:7700")];
        let mut watch = LagWatch::default();
        let start = Instant::now();
        // 10 fps, a fifth of it lost: lagging once a whole minute is measured.
        let (early, _, _, _) = run(&mut watch, &order, start, &[(40, 10); 6]);
        assert!(early.is_empty(), "not a whole window yet");
        let mut watch = LagWatch::default();
        let (lagging, _, _, _) = run(&mut watch, &order, start, &[(40, 10); 14]);
        assert_eq!(lagging.len(), 1);
        assert_eq!(lagging[0].camera_id, "cam-1");
        assert_eq!(lagging[0].percent_lost, 20);
        // Keeping up again for a whole window ends it.
        let mut steps = vec![(40, 10); 14];
        steps.extend([(50, 0); 14]);
        let mut watch = LagWatch::default();
        let (lagging, _, _, _) = run(&mut watch, &order, start, &steps);
        assert!(lagging.is_empty(), "{lagging:?}");
    }

    #[test]
    fn a_brief_loss_a_local_plugin_or_a_new_session_is_not_lagging() {
        let start = Instant::now();
        // A reconnect's handful of lost units.
        let mut steps = vec![(50, 0); 14];
        steps[7] = (45, 5);
        let mut watch = LagWatch::default();
        let order = [server("tls:plugins.example.test:7700")];
        assert!(run(&mut watch, &order, start, &steps).0.is_empty());
        // The same loss on a plugin on this computer: busy, not bandwidth.
        let mut watch = LagWatch::default();
        let local = [server("unix:/run/nw/plugin.sock")];
        assert!(run(&mut watch, &local, start, &[(40, 10); 14]).0.is_empty());
        // A new session's counters start again from zero: measured afresh.
        let mut watch = LagWatch::default();
        let (_, at, _, _) = run(&mut watch, &order, start, &[(40, 10); 14]);
        assert!(watch.lagging(&order, &status(5, 0), at).is_empty());
    }
}
