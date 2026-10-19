//! Alert settings (`config.json` `alerts`): where alerts go (channels) and
//! which events send them (rules).
//!
//! Channel secrets (a Telegram bot token, an ntfy access token, a webhook's
//! shared secret) are stored like camera passwords: in the private config
//! file only. `alerts.get` never returns them ([`AlertsConfig::masked`]); a
//! client sends [`KEPT_SECRET`] back to keep the stored one.

use nw_core::event::EventKind;
use serde::{Deserialize, Serialize};

/// Stands for a stored secret in `alerts.get`, and keeps it in `alerts.set`.
pub const KEPT_SECRET: &str = "••••••••";
pub const MAX_CHANNELS: usize = 20;
pub const MAX_RULES: usize = 50;
pub const DEFAULT_NTFY_SERVER: &str = "https://ntfy.sh";
pub const DEFAULT_COOLDOWN_SEC: u32 = 60;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertsConfig {
    #[serde(default)]
    pub channels: Vec<AlertChannel>,
    #[serde(default)]
    pub rules: Vec<AlertRule>,
}

/// Somewhere alerts are sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertChannel {
    pub id: String,
    pub name: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Send the event's saved picture too (webhook, ntfy, Telegram).
    #[serde(default)]
    pub attach_picture: bool,
    #[serde(flatten)]
    pub target: ChannelTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelTarget {
    /// A notification on this computer's desktop (the NeuroWatch app shows it).
    Desktop,
    /// An HTTP POST of the alert as JSON; `secret` goes in `X-NeuroWatch-Token`.
    Webhook {
        url: String,
        #[serde(default)]
        secret: String,
    },
    /// A push notification through an ntfy server (`ntfy.sh` or your own).
    Ntfy {
        #[serde(default = "default_ntfy_server")]
        server: String,
        topic: String,
        #[serde(default)]
        token: String,
    },
    /// A Telegram message from your bot to a chat.
    Telegram { bot_token: String, chat_id: String },
}

/// Which events send alerts, when, and where.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub name: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Event kinds that fire it (at least one).
    pub kinds: Vec<EventKind>,
    /// Cameras it watches; empty: every camera (and recorder-wide events).
    #[serde(default)]
    pub cameras: Vec<String>,
    /// When it is active, by the recorder's local clock; empty: always.
    #[serde(default)]
    pub schedule: Vec<AlertWindow>,
    /// After an alert, the same rule stays quiet this long for that camera.
    #[serde(default = "default_cooldown")]
    pub cooldown_sec: u32,
    /// Channel ids it sends to (at least one).
    pub channels: Vec<String>,
}

/// Days (Monday = 0) and a time of day; `end` before `start` runs overnight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertWindow {
    pub days: Vec<u8>,
    pub start: String,
    pub end: String,
}

fn enabled() -> bool {
    true
}
fn default_ntfy_server() -> String {
    DEFAULT_NTFY_SERVER.into()
}
fn default_cooldown() -> u32 {
    DEFAULT_COOLDOWN_SEC
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= 60 && !name.chars().any(char::is_control)
}

/// An http(s) URL without credentials or fragments.
fn web_url(value: &str) -> Result<reqwest::Url, String> {
    let url =
        reqwest::Url::parse(value.trim()).map_err(|_| format!("'{value}' is not a web address"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("use an http:// or https:// address".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("put no user name or password in the address".into());
    }
    Ok(url)
}

impl ChannelTarget {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Webhook { .. } => "webhook",
            Self::Ntfy { .. } => "ntfy",
            Self::Telegram { .. } => "telegram",
        }
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Desktop => Ok(()),
            Self::Webhook { url, secret } => {
                web_url(url)?;
                if secret.chars().count() > 200 || secret.chars().any(char::is_control) {
                    return Err("the webhook secret is at most 200 printable characters".into());
                }
                Ok(())
            }
            Self::Ntfy {
                server,
                topic,
                token,
            } => {
                web_url(server)?;
                if topic.is_empty()
                    || topic.len() > 64
                    || !topic
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err("an ntfy topic is 1–64 letters, digits, - or _".into());
                }
                if token.len() > 200 || !token.bytes().all(|b| b.is_ascii_graphic()) {
                    return Err("the ntfy access token is not valid".into());
                }
                Ok(())
            }
            Self::Telegram { bot_token, chat_id } => {
                let (number, rest) = bot_token.split_once(':').unwrap_or(("", ""));
                if number.is_empty()
                    || !number.bytes().all(|b| b.is_ascii_digit())
                    || rest.len() < 20
                    || rest.len() > 100
                    || !rest
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(
                        "a Telegram bot token looks like 123456:ABC-… (from @BotFather)".into(),
                    );
                }
                let numeric = chat_id
                    .strip_prefix('-')
                    .unwrap_or(chat_id)
                    .bytes()
                    .all(|b| b.is_ascii_digit());
                let username = chat_id.strip_prefix('@').is_some_and(|name| {
                    (5..=32).contains(&name.len())
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                });
                if chat_id.is_empty() || chat_id.len() > 40 || !(numeric || username) {
                    return Err(
                        "a Telegram chat id is a number (e.g. -1001234) or @channelname".into(),
                    );
                }
                Ok(())
            }
        }
    }

    /// The same target with its secrets replaced by [`KEPT_SECRET`].
    fn masked(&self) -> Self {
        let hide = |secret: &str| {
            if secret.is_empty() {
                String::new()
            } else {
                KEPT_SECRET.to_owned()
            }
        };
        match self {
            Self::Desktop => Self::Desktop,
            Self::Webhook { url, secret } => Self::Webhook {
                url: url.clone(),
                secret: hide(secret),
            },
            Self::Ntfy {
                server,
                topic,
                token,
            } => Self::Ntfy {
                server: server.clone(),
                topic: topic.clone(),
                token: hide(token),
            },
            Self::Telegram { bot_token, chat_id } => Self::Telegram {
                bot_token: hide(bot_token),
                chat_id: chat_id.clone(),
            },
        }
    }

    /// Put back secrets a client left as [`KEPT_SECRET`] (same channel, same kind).
    fn restore(&mut self, stored: Option<&Self>) {
        let keep = |secret: &mut String, old: Option<&String>| {
            if secret == KEPT_SECRET {
                *secret = old.cloned().unwrap_or_default();
            }
        };
        match (self, stored) {
            (Self::Webhook { secret, .. }, Some(Self::Webhook { secret: old, .. })) => {
                keep(secret, Some(old))
            }
            (Self::Webhook { secret, .. }, _) => keep(secret, None),
            (Self::Ntfy { token, .. }, Some(Self::Ntfy { token: old, .. })) => {
                keep(token, Some(old))
            }
            (Self::Ntfy { token, .. }, _) => keep(token, None),
            (Self::Telegram { bot_token, .. }, Some(Self::Telegram { bot_token: old, .. })) => {
                keep(bot_token, Some(old))
            }
            (Self::Telegram { bot_token, .. }, _) => keep(bot_token, None),
            (Self::Desktop, _) => {}
        }
    }
}

impl AlertsConfig {
    /// Why these settings cannot be used, or `Ok`.
    pub fn validate(&self) -> Result<(), String> {
        if self.channels.len() > MAX_CHANNELS || self.rules.len() > MAX_RULES {
            return Err(format!(
                "at most {MAX_CHANNELS} channels and {MAX_RULES} rules"
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for channel in &self.channels {
            if !valid_id(&channel.id) || !ids.insert(channel.id.as_str()) {
                return Err(format!(
                    "channel id '{}' is invalid or repeated",
                    channel.id
                ));
            }
            if !valid_name(&channel.name) {
                return Err("a channel name is 1–60 printable characters".into());
            }
            channel
                .target
                .validate()
                .map_err(|e| format!("{}: {e}", channel.name))?;
        }
        let mut rule_ids = std::collections::HashSet::new();
        for rule in &self.rules {
            if !valid_id(&rule.id) || !rule_ids.insert(rule.id.as_str()) {
                return Err(format!("rule id '{}' is invalid or repeated", rule.id));
            }
            if !valid_name(&rule.name) {
                return Err("a rule name is 1–60 printable characters".into());
            }
            if rule.kinds.is_empty() {
                return Err(format!("{}: choose at least one kind of event", rule.name));
            }
            if rule.channels.is_empty() || rule.channels.iter().any(|id| !ids.contains(id.as_str()))
            {
                return Err(format!(
                    "{}: choose at least one existing channel",
                    rule.name
                ));
            }
            if rule.cameras.len() > 256
                || rule
                    .cameras
                    .iter()
                    .any(|id| id.is_empty() || id.len() > 128)
            {
                return Err(format!("{}: invalid camera list", rule.name));
            }
            if rule.cooldown_sec > 86_400 {
                return Err(format!("{}: the quiet time is at most a day", rule.name));
            }
            if rule.schedule.len() > 28 {
                return Err(format!("{}: at most 28 time windows", rule.name));
            }
            for window in &rule.schedule {
                let start = crate::recording::policy::minute_of_day(&window.start);
                let end = crate::recording::policy::minute_of_day(&window.end);
                if window.days.is_empty()
                    || window.days.iter().any(|day| *day > 6)
                    || start.is_err()
                    || end.is_err()
                    || start == end
                {
                    return Err(format!(
                        "{}: each time window needs days and different start and end times (HH:MM)",
                        rule.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// For clients: secrets replaced by [`KEPT_SECRET`].
    pub fn masked(&self) -> Self {
        let mut masked = self.clone();
        for channel in &mut masked.channels {
            channel.target = channel.target.masked();
        }
        masked
    }

    /// `changes` (a client's full settings) with secrets left as
    /// [`KEPT_SECRET`] taken from `self`, validated.
    pub fn replaced_by(&self, mut changes: Self) -> Result<Self, String> {
        for channel in &mut changes.channels {
            let stored = self
                .channels
                .iter()
                .find(|old| old.id == channel.id)
                .map(|old| &old.target);
            channel.target.restore(stored);
        }
        changes.validate()?;
        Ok(changes)
    }
}

impl AlertWindow {
    /// Whether local `day` (Monday = 0) and `minute` fall inside.
    pub fn contains(&self, day: u8, minute: usize) -> bool {
        let (Ok(start), Ok(end)) = (
            crate::recording::policy::minute_of_day(&self.start),
            crate::recording::policy::minute_of_day(&self.end),
        ) else {
            return false;
        };
        if start < end {
            self.days.contains(&day) && (start..end).contains(&minute)
        } else {
            // Overnight: from `start` on a listed day until `end` the next day.
            let yesterday = (day + 6) % 7;
            (self.days.contains(&day) && minute >= start)
                || (self.days.contains(&yesterday) && minute < end)
        }
    }
}

impl AlertRule {
    /// Whether this rule is active at local `day`/`minute` (always without a schedule).
    pub fn active_at(&self, day: u8, minute: usize) -> bool {
        self.schedule.is_empty() || self.schedule.iter().any(|w| w.contains(day, minute))
    }

    /// Whether an event of `kind` on `camera` fires it (ignoring time and cooldown).
    pub fn fires_for(&self, kind: EventKind, camera: Option<&str>) -> bool {
        self.enabled
            && self.kinds.contains(&kind)
            && (self.cameras.is_empty()
                || camera.is_some_and(|id| self.cameras.iter().any(|c| c == id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> AlertsConfig {
        serde_json::from_value(json!({
            "channels": [
                {"id": "phone", "name": "My phone", "kind": "ntfy", "topic": "gate-alerts", "token": "tk_secret"},
                {"id": "bot", "name": "Family chat", "kind": "telegram",
                 "bot_token": "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZ", "chat_id": "-1001234"},
                {"id": "desk", "name": "This computer", "kind": "desktop"},
                {"id": "hook", "name": "Home Assistant", "kind": "webhook", "url": "https://ha.local/api/webhook/x"}
            ],
            "rules": [{"id": "night", "name": "Motion at night", "kinds": ["motion"],
                       "cameras": ["cam-1"], "channels": ["phone", "desk"],
                       "schedule": [{"days": [0,1,2,3,4,5,6], "start": "22:00", "end": "06:00"}]}]
        }))
        .unwrap()
    }

    #[test]
    fn settings_parse_with_defaults_and_validate() {
        let config = sample();
        assert!(config.validate().is_ok(), "{:?}", config.validate());
        assert_eq!(config.rules[0].cooldown_sec, DEFAULT_COOLDOWN_SEC);
        assert!(
            matches!(&config.channels[0].target, ChannelTarget::Ntfy { server, .. } if server == DEFAULT_NTFY_SERVER)
        );
        for (path, value) in [
            ("/channels/0/topic", json!("bad topic!")),
            ("/channels/1/bot_token", json!("nope")),
            ("/channels/1/chat_id", json!("me")),
            ("/channels/3/url", json!("ftp://x")),
            ("/channels/3/url", json!("https://user:pw@host/")),
            ("/rules/0/channels", json!(["missing"])),
            ("/rules/0/kinds", json!([])),
            ("/rules/0/schedule/0/end", json!("22:00")),
            ("/channels/2/id", json!("phone")),
        ] {
            let mut raw = serde_json::to_value(sample()).unwrap();
            *raw.pointer_mut(path).unwrap() = value.clone();
            let config: AlertsConfig = serde_json::from_value(raw).unwrap();
            assert!(config.validate().is_err(), "{path} = {value}");
        }
    }

    #[test]
    fn clients_never_see_secrets_and_keep_them_by_sending_the_marker_back() {
        let stored = sample();
        let masked = stored.masked();
        let text = serde_json::to_string(&masked).unwrap();
        assert!(!text.contains("tk_secret") && !text.contains("ABCDEFGHIJ"));
        let kept = stored.replaced_by(masked.clone()).unwrap();
        assert_eq!(kept, stored, "the marker keeps every stored secret");
        let mut changed = masked;
        if let ChannelTarget::Ntfy { token, .. } = &mut changed.channels[0].target {
            *token = String::new();
        }
        let cleared = stored.replaced_by(changed).unwrap();
        assert!(
            matches!(&cleared.channels[0].target, ChannelTarget::Ntfy { token, .. } if token.is_empty())
        );
        // A new channel cannot borrow another channel's secret.
        let mut renamed = stored.masked();
        renamed.channels[1].id = "other".into();
        renamed.rules.clear();
        let fresh = stored.replaced_by(renamed);
        assert!(fresh.is_err(), "a new Telegram channel needs its own token");
    }

    #[test]
    fn rules_match_kind_camera_and_overnight_windows() {
        let rule = &sample().rules[0];
        assert!(rule.fires_for(EventKind::Motion, Some("cam-1")));
        assert!(!rule.fires_for(EventKind::Motion, Some("cam-2")));
        assert!(!rule.fires_for(EventKind::Tamper, Some("cam-1")));
        assert!(!rule.fires_for(EventKind::Motion, None));
        assert!(rule.active_at(2, 23 * 60));
        assert!(rule.active_at(3, 5 * 60 + 59));
        assert!(!rule.active_at(3, 12 * 60));
        let mut weekend: AlertWindow =
            serde_json::from_value(json!({"days": [5], "start": "22:00", "end": "02:00"})).unwrap();
        assert!(
            weekend.contains(5, 23 * 60) && weekend.contains(6, 60) && !weekend.contains(0, 60)
        );
        weekend.days = vec![];
        assert!(!weekend.contains(5, 23 * 60));
    }
}
