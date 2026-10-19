//! Camera responses hide stored passwords, including URL userinfo. Preserve
//! the exact rest of each URL so a portal edit can safely round-trip it.

use std::ops::Range;

use serde_json::Value;

/// The authority's raw userinfo, including its final `@`. Looking at the
/// authority only preserves paths, queries, IPv6 and percent escapes, and
/// also strips credentials from incomplete URLs instead of echoing them.
fn userinfo(url: &str) -> Option<Range<usize>> {
    let start = url.find("://")? + 3;
    let end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |index| start + index);
    let at = url[start..end].rfind('@')? + start;
    Some(start..at + 1)
}

fn without_userinfo(url: &str) -> String {
    let Some(range) = userinfo(url) else {
        return url.to_owned();
    };
    format!("{}{}", &url[..range.start], &url[range.end..])
}

/// Leave the portal's editable username, but never return a password or
/// URL userinfo. Return whether any password was stored. Local non-admin
/// callers also remove the username field; the portal exposes only this flag.
pub(crate) fn redact_passwords(camera: &mut Value) -> bool {
    fn visit(value: &mut Value) -> bool {
        match value {
            Value::Object(fields) => {
                let mut has_password = fields
                    .remove("password_b64")
                    .and_then(|value| value.as_str().map(|text| !text.is_empty()))
                    .unwrap_or(false);
                for value in fields.values_mut() {
                    has_password |= visit(value);
                }
                has_password
            }
            Value::Array(items) => {
                let mut has_password = false;
                for item in items {
                    has_password |= visit(item);
                }
                has_password
            }
            Value::String(url) => {
                let Some(range) = userinfo(url) else {
                    return false;
                };
                let has_password = url[range.start..range.end - 1]
                    .split_once(':')
                    .is_some_and(|(_, password)| !password.is_empty());
                *url = without_userinfo(url);
                has_password
            }
            _ => false,
        }
    }
    visit(camera)
}

/// A redacted URL submitted unchanged means "keep the stored credentials".
/// New URLs and explicitly supplied userinfo win. Streams match by id so
/// reordering them cannot transfer one stream's credentials to another.
pub(crate) fn preserve_url_credentials(current: &Value, changes: &mut Value) {
    match (current, changes) {
        (Value::String(old), Value::String(new)) => {
            if userinfo(old).is_some() && *new == without_userinfo(old) {
                *new = old.clone();
            }
        }
        (Value::Object(old), Value::Object(new)) => {
            for (key, value) in new {
                if let Some(stored) = old.get(key) {
                    preserve_url_credentials(stored, value);
                }
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            for (index, value) in new.iter_mut().enumerate() {
                let stored = match value.get("id") {
                    Some(id) => old.iter().find(|item| item.get("id") == Some(id)),
                    None => old.get(index),
                };
                if let Some(stored) = stored {
                    preserve_url_credentials(stored, value);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn security_redaction_preserves_noncredential_url_bytes_and_handles_incomplete_urls() {
        for (original, redacted) in [
            (
                "rtsp://u:p%40ss@[::1]:8554/live?x=a@b#part",
                "rtsp://[::1]:8554/live?x=a@b#part",
            ),
            ("http://user:secret@", "http://"),
            ("rtsp://user@camera/live", "rtsp://camera/live"),
            ("file:///tmp/a@b.mp4", "file:///tmp/a@b.mp4"),
            ("/dev/video0", "/dev/video0"),
        ] {
            assert_eq!(without_userinfo(original), redacted);
        }
        let mut camera = json!({"url":"rtsp://u:p@cam/main"});
        assert!(redact_passwords(&mut camera));
        let mut camera = json!({"url":"rtsp://u@cam/main"});
        assert!(!redact_passwords(&mut camera));
    }

    #[test]
    fn security_stream_credentials_survive_reordering_but_do_not_override_new_urls() {
        let stored = json!({"streams":[
            {"id":"a","url":"rtsp://a:first@cam/a"},
            {"id":"b","url":"rtsp://b:second@cam/b"}
        ]});
        let mut reordered = json!({"streams":[
            {"id":"b","url":"rtsp://cam/b"}, {"id":"a","url":"rtsp://cam/a"}
        ]});
        preserve_url_credentials(&stored, &mut reordered);
        assert_eq!(reordered["streams"][0]["url"], stored["streams"][1]["url"]);
        assert_eq!(reordered["streams"][1]["url"], stored["streams"][0]["url"]);
        let mut changed = json!({"streams":[
            {"id":"a","url":"rtsp://new:new-password@cam/a"},
            {"id":"b","url":"rtsp://different-camera/b"}
        ]});
        let expected = changed.clone();
        preserve_url_credentials(&stored, &mut changed);
        assert_eq!(changed, expected);
    }
}
