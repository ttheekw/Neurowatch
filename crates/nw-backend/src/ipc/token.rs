//! The per-user secret that marks this user's own clients on a TCP endpoint.
//!
//! A Unix socket is owner-only and checks the peer's uid; any local process
//! can reach a TCP port (Windows always uses one). So the backend writes a
//! random token to a file only this user can read, `<data_local_dir>/
//! neurowatch/ipc-token-<address>` (`%LOCALAPPDATA%` on Windows, inside the
//! user's profile), and a request carrying it (`"auth"`) is treated like one
//! from the Unix socket. The UI reads the same file (`neurowatch.ipc.auth`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the token for TCP address `addr` (`host:port`) is kept.
pub fn path_for(addr: &str) -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("neurowatch")
            .join(format!("ipc-token-{}", file_label(addr))),
    )
}

/// `127.0.0.1:7676` → `127.0.0.1-7676`: letters, digits and dots kept.
fn file_label(addr: &str) -> String {
    addr.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Writes a new token for `addr`; returns its file and the token.
pub fn publish(addr: &str) -> anyhow::Result<(PathBuf, String)> {
    let path = path_for(addr).ok_or_else(|| anyhow::anyhow!("no local data directory"))?;
    let token = generate()?;
    write_private(&path, &token)?;
    Ok((path, token))
}

fn generate() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("random token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Replaces `path` with `token`, readable only by this user (0600 on Unix;
/// on Windows the file inherits the profile folder's owner-only access).
fn write_private(path: &Path, token: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("token path has no folder"))?;
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".ipc-token-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    Ok(result?)
}

/// Whether `request` carries `token` (compared in constant time).
pub fn presented(request: &serde_json::Value, token: &str) -> bool {
    let Some(given) = request.get("auth").and_then(|value| value.as_str()) else {
        return false;
    };
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_token_file_is_named_after_the_address() {
        assert_eq!(file_label("127.0.0.1:7676"), "127.0.0.1-7676");
        assert_eq!(file_label("[::1]:7676"), "---1--7676");
    }

    #[test]
    fn only_the_exact_token_is_accepted() {
        let token = generate().unwrap();
        assert_eq!(token.len(), 64);
        assert_ne!(token, generate().unwrap());
        assert!(presented(
            &json!({"method": "cloud.status", "auth": token}),
            &token
        ));
        assert!(!presented(&json!({"method": "cloud.status"}), &token));
        assert!(!presented(&json!({"auth": ""}), &token));
        assert!(!presented(&json!({"auth": &token[..63]}), &token));
        assert!(!presented(&json!({"auth": 7}), &token));
        let mut wrong = token.clone().into_bytes();
        wrong[10] ^= 1;
        assert!(!presented(
            &json!({"auth": String::from_utf8(wrong).unwrap()}),
            &token
        ));
    }

    #[test]
    fn the_token_is_written_privately_and_replaced_on_restart() {
        let dir = std::env::temp_dir().join(format!("nw-token-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("ipc-token-x");
        write_private(&path, "first").unwrap();
        write_private(&path, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
