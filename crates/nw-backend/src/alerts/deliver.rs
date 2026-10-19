//! Sending one alert to one channel. Errors are short and never carry a
//! request URL (a Telegram URL holds the bot token).

use serde_json::json;

use super::config::ChannelTarget;
use super::Alert;

/// Why a request failed, without its URL.
fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "could not connect".into()
    } else if let Some(status) = error.status() {
        format!("HTTP {}", status.as_u16())
    } else if error.is_builder() {
        "invalid address".into()
    } else {
        "request failed".into()
    }
}

async fn check(response: reqwest::Response) -> Result<(), String> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    // Telegram and ntfy explain refusals in their JSON; keep it short.
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    let reason = body["description"]
        .as_str()
        .or_else(|| body["error"].as_str())
        .map(|text| {
            text.chars()
                .filter(|c| !c.is_control())
                .take(120)
                .collect::<String>()
        });
    Err(match reason {
        Some(reason) => format!("HTTP {}: {reason}", status.as_u16()),
        None => format!("HTTP {}", status.as_u16()),
    })
}

/// HTTP headers carry ASCII only; ntfy reads `Title`/`Message` from them when
/// the body is a picture.
fn header_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                ' '
            }
        })
        .take(200)
        .collect()
}

fn multipart(boundary: &str, fields: &[(&str, &str)], file: (&str, &str, &[u8])) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    let (name, filename, bytes) = file;
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: image/jpeg\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

pub async fn deliver(
    client: &reqwest::Client,
    target: &ChannelTarget,
    alert: &Alert,
    picture: Option<&[u8]>,
) -> Result<(), String> {
    match target {
        // The desktop app shows these (`alerts.desktop`); nothing is sent.
        ChannelTarget::Desktop => Ok(()),
        ChannelTarget::Webhook { url, secret } => {
            let mut body = json!({
                "title": alert.title,
                "message": alert.message,
                "rule": alert.rule_name,
                "test": alert.test,
                "event": {
                    "id": alert.event.id,
                    "kind": alert.event.kind.as_str(),
                    "camera_id": alert.event.camera_id,
                    "camera_name": alert.camera_name,
                    "start_ms": alert.event.start_ms,
                    "detail": alert.event.detail,
                },
            });
            if let Some(picture) = picture {
                use base64::Engine as _;
                body["picture_jpeg_b64"] =
                    json!(base64::engine::general_purpose::STANDARD.encode(picture));
            }
            let mut request = client.post(url.trim()).json(&body);
            if !secret.is_empty() {
                request = request.header("X-NeuroWatch-Token", secret.as_str());
            }
            check(request.send().await.map_err(|e| describe(&e))?).await
        }
        ChannelTarget::Ntfy {
            server,
            topic,
            token,
        } => {
            let base = server.trim().trim_end_matches('/');
            let request = match picture {
                Some(picture) => client
                    .put(format!("{base}/{topic}"))
                    .header("Title", header_text(&alert.title))
                    .header("Message", header_text(&alert.message))
                    .header("Filename", "event.jpg")
                    .header("Tags", "rotating_light")
                    .body(picture.to_vec()),
                None => client.post(base).json(&json!({
                    "topic": topic,
                    "title": alert.title,
                    "message": alert.message,
                    "tags": ["rotating_light"],
                })),
            };
            let request = if token.is_empty() {
                request
            } else {
                request.bearer_auth(token)
            };
            check(request.send().await.map_err(|e| describe(&e))?).await
        }
        ChannelTarget::Telegram { bot_token, chat_id } => {
            let text = format!("{}\n{}", alert.title, alert.message);
            let api = format!("https://api.telegram.org/bot{bot_token}");
            let request = match picture {
                Some(picture) => {
                    let boundary = format!("neurowatch{}", uuid::Uuid::new_v4().simple());
                    let caption: String = text.chars().take(1000).collect();
                    client
                        .post(format!("{api}/sendPhoto"))
                        .header(
                            "Content-Type",
                            format!("multipart/form-data; boundary={boundary}"),
                        )
                        .body(multipart(
                            &boundary,
                            &[("chat_id", chat_id), ("caption", &caption)],
                            ("photo", "event.jpg", picture),
                        ))
                }
                None => client
                    .post(format!("{api}/sendMessage"))
                    .json(&json!({"chat_id": chat_id, "text": text})),
            };
            check(request.send().await.map_err(|e| describe(&e))?).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_stay_ascii_and_multipart_frames_the_photo() {
        assert_eq!(header_text("Motion · Gate\n"), "Motion   Gate ");
        let body = multipart("b", &[("chat_id", "-1")], ("photo", "e.jpg", b"\xff\xd8"));
        let text = String::from_utf8_lossy(&body);
        assert!(text
            .starts_with("--b\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n-1\r\n"));
        assert!(text.contains("filename=\"e.jpg\"\r\nContent-Type: image/jpeg"));
        assert!(text.ends_with("\r\n--b--\r\n"));
    }
}
