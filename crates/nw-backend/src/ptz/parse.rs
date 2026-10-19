//! Bounded ONVIF PTZ reply parsing and normalized velocity spaces.
use crate::discovery::onvif::OnvifError;
use serde::Serialize;

pub fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub fn token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
fn document(reply: &str) -> Result<roxmltree::Document<'_>, OnvifError> {
    roxmltree::Document::parse(reply).map_err(|_| OnvifError::Invalid("Invalid PTZ reply".into()))
}
pub fn text(root: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    root.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
#[derive(Clone)]
pub struct Profile {
    pub token: String,
    pub config: String,
    pub node: Option<String>,
}
pub fn profile(reply: &str) -> Result<Profile, OnvifError> {
    let doc = document(reply)?;
    for p in doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Profiles")
    {
        let Some(config) = p
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "PTZConfiguration")
        else {
            continue;
        };
        if let (Some(profile), Some(config_token)) =
            (p.attribute("token"), config.attribute("token"))
        {
            if token(profile) && token(config_token) {
                return Ok(Profile {
                    token: profile.into(),
                    config: config_token.into(),
                    node: text(config, "NodeToken").filter(|s| token(s)),
                });
            }
        }
    }
    Err(OnvifError::Invalid(
        "This camera has no PTZ-enabled media profile".into(),
    ))
}
#[derive(Debug, Clone)]
pub struct Space {
    pub uri: String,
    pub x: (f64, f64),
    pub y: Option<(f64, f64)>,
}
impl Space {
    pub fn x(&self, v: f64) -> f64 {
        scale(v, self.x)
    }
    pub fn y(&self, v: f64) -> f64 {
        scale(v, self.y.unwrap_or(self.x))
    }
}
fn scale(value: f64, range: (f64, f64)) -> f64 {
    if value >= 0.0 {
        value * range.1
    } else {
        -value * range.0
    }
}
fn range(node: roxmltree::Node<'_, '_>, name: &str) -> Option<(f64, f64)> {
    let r = node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == name)?;
    let min: f64 = text(r, "Min")?.parse().ok()?;
    let max: f64 = text(r, "Max")?.parse().ok()?;
    (min.is_finite() && max.is_finite() && min < 0.0 && max > 0.0).then_some((min, max))
}
fn space(root: roxmltree::Node<'_, '_>, name: &str, pan: bool) -> Option<Space> {
    root.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == name)
        .find_map(|s| {
            let uri =
                text(s, "URI").filter(|s| s.len() <= 256 && !s.chars().any(char::is_control))?;
            let x = range(s, "XRange")?;
            let y = if pan { Some(range(s, "YRange")?) } else { None };
            Some(Space { uri, x, y })
        })
}
pub struct Options {
    pub pan: Option<Space>,
    pub zoom: Option<Space>,
    pub relative_pan: Option<Space>,
    pub relative_zoom: Option<Space>,
    /// The camera's own stop timeout for a held move, at most 1 s.
    pub timeout: f64,
    /// The longest stop timeout the camera accepts, at most 3 s (longer leases
    /// for clients that cannot renew quickly).
    pub longest: f64,
}
fn seconds(value: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut number = String::new();
    for c in value.strip_prefix("PT")?.chars() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let n = number.parse::<f64>().ok()?;
        number.clear();
        total += n * match c {
            'H' => 3600.0,
            'M' => 60.0,
            'S' => 1.0,
            _ => return None,
        };
    }
    (number.is_empty() && total.is_finite() && total >= 0.0).then_some(total)
}
pub fn options(reply: &str) -> Result<Options, OnvifError> {
    let doc = document(reply)?;
    let root = doc.root_element();
    let timeout = root
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "PTZTimeout")
        .and_then(|n| Some((seconds(&text(n, "Min")?)?, seconds(&text(n, "Max")?)?)))
        .filter(|(min, max)| *min <= 1.0 && *max >= 0.5 && min <= max)
        .map(|(_, max)| max);
    Ok(Options {
        pan: timeout.and_then(|_| space(root, "ContinuousPanTiltVelocitySpace", true)),
        zoom: timeout.and_then(|_| space(root, "ContinuousZoomVelocitySpace", false)),
        relative_pan: space(root, "RelativePanTiltTranslationSpace", true),
        relative_zoom: space(root, "RelativeZoomTranslationSpace", false),
        timeout: timeout.map_or(1.0, |max| max.min(1.0)),
        longest: timeout.map_or(1.0, |max| max.min(3.0)),
    })
}
#[derive(Debug, Serialize)]
pub struct Preset {
    pub token: String,
    pub name: String,
}
pub fn presets(reply: &str) -> Result<Vec<Preset>, OnvifError> {
    let doc = document(reply)?;
    Ok(doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Preset")
        .filter_map(|n| {
            let id = n.attribute("token").filter(|s| token(s))?;
            Some(Preset {
                token: id.into(),
                name: text(n, "Name")
                    .unwrap_or_else(|| id.into())
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(64)
                    .collect(),
            })
        })
        .take(256)
        .collect())
}
pub fn node(reply: &str) -> (bool, bool) {
    let Ok(doc) = document(reply) else {
        return (false, true);
    };
    let home = text(doc.root_element(), "HomeSupported").is_some_and(|s| s == "true" || s == "1");
    let presets = text(doc.root_element(), "MaximumNumberOfPresets")
        .and_then(|s| s.parse::<u32>().ok())
        .map_or(true, |n| n > 0);
    (home, presets)
}
pub fn idle(reply: &str) -> Option<bool> {
    let doc = document(reply).ok()?;
    let root = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "MoveStatus")?;
    let status: Vec<_> = root
        .children()
        .filter_map(|n| n.text().map(str::trim))
        .collect();
    if status.contains(&"MOVING") {
        Some(false)
    } else if !status.is_empty() && status.iter().all(|s| *s == "IDLE") {
        Some(true)
    } else {
        None
    }
}
