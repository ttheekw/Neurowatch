//! Ed25519 session authentication, independent of any transport or recorder.
pub use ed25519_dalek::SigningKey;
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};
use nw_analytics_protocol::{
    auth::{Authenticate, PluginIdentity},
    messages::Manifest,
};
use sha2::{Digest, Sha256};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut bytes = [0; N];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let digit = |value: u8| match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            _ => None,
        };
        bytes[index] = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(bytes)
}
pub fn challenge() -> Result<String, getrandom::Error> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes)?;
    Ok(hex(&bytes))
}
pub fn random_key() -> Result<SigningKey, getrandom::Error> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes)?;
    Ok(SigningKey::from_bytes(&bytes))
}
pub fn public_key(key: &SigningKey) -> String {
    hex(key.verifying_key().as_bytes())
}
pub fn fingerprint(public: &str) -> Option<String> {
    Some(hex(&Sha256::digest(unhex::<32>(public)?)))
}

// Sort arbitrary schema objects too, independent of serde_json feature flags.
fn canonical(value: &serde_json::Value, bytes: &mut Vec<u8>) {
    match value {
        serde_json::Value::Object(object) => {
            bytes.push(b'{');
            let ordered: std::collections::BTreeMap<_, _> = object.iter().collect();
            for (index, (key, value)) in ordered.into_iter().enumerate() {
                if index > 0 {
                    bytes.push(b',');
                }
                bytes.extend(serde_json::to_vec(key).unwrap_or_default());
                bytes.push(b':');
                canonical(value, bytes);
            }
            bytes.push(b'}');
        }
        serde_json::Value::Array(array) => {
            bytes.push(b'[');
            for (index, value) in array.iter().enumerate() {
                if index > 0 {
                    bytes.push(b',');
                }
                canonical(value, bytes);
            }
            bytes.push(b']');
        }
        value => bytes.extend(serde_json::to_vec(value).unwrap_or_default()),
    }
}
fn manifest_bytes(manifest: &Manifest, nonce: &str, identity: &PluginIdentity) -> Vec<u8> {
    let mut unsigned = manifest.clone();
    unsigned.identity = None;
    let mut bytes = b"NeuroWatch analytics plugin v1\0".to_vec();
    canonical(
        &serde_json::json!({"manifest":unsigned,"node_challenge":nonce,"plugin_challenge":identity.challenge,"channel_binding":identity.channel_binding}),
        &mut bytes,
    );
    bytes
}
pub fn sign_manifest(
    manifest: &mut Manifest,
    key: &SigningKey,
    nonce: &str,
    channel_binding: &str,
) -> Result<(), getrandom::Error> {
    let mut identity = PluginIdentity {
        public_key: public_key(key),
        challenge: challenge()?,
        signature: String::new(),
        channel_binding: channel_binding.into(),
    };
    identity.signature = hex(&key
        .sign(&manifest_bytes(manifest, nonce, &identity))
        .to_bytes());
    manifest.identity = Some(identity);
    Ok(())
}
pub fn verify_manifest(manifest: &Manifest, nonce: &str, channel_binding: &str) -> bool {
    let Some(identity) = manifest.identity.as_ref() else {
        return false;
    };
    if unhex::<32>(nonce).is_none()
        || unhex::<32>(&identity.challenge).is_none()
        || identity.channel_binding != channel_binding
    {
        return false;
    }
    verify(
        &identity.public_key,
        &identity.signature,
        &manifest_bytes(manifest, nonce, identity),
    )
}
fn auth_bytes(
    public: &str,
    nonce: &str,
    plugin: &PluginIdentity,
    credential: Option<&str>,
) -> Vec<u8> {
    let mut bytes = b"NeuroWatch analytics node v1\0".to_vec();
    canonical(
        &serde_json::json!({"node_key":public,"plugin_key":plugin.public_key,"node_challenge":nonce,"plugin_challenge":plugin.challenge,"channel_binding":plugin.channel_binding,"credential_digest":credential.map(|value| hex(&Sha256::digest(value.as_bytes())))}),
        &mut bytes,
    );
    bytes
}
pub fn authenticate(
    key: &SigningKey,
    nonce: &str,
    plugin: &PluginIdentity,
    credential: Option<String>,
) -> Authenticate {
    let public_key = public_key(key);
    Authenticate {
        signature: hex(&key
            .sign(&auth_bytes(
                &public_key,
                nonce,
                plugin,
                credential.as_deref(),
            ))
            .to_bytes()),
        public_key,
        credential,
    }
}
pub fn verify_authenticate(auth: &Authenticate, nonce: &str, plugin: &PluginIdentity) -> bool {
    verify(
        &auth.public_key,
        &auth.signature,
        &auth_bytes(&auth.public_key, nonce, plugin, auth.credential.as_deref()),
    )
}
fn verify(public: &str, signature: &str, bytes: &[u8]) -> bool {
    let Some(public) = unhex::<32>(public) else {
        return false;
    };
    let Some(signature) = unhex::<64>(signature) else {
        return false;
    };
    VerifyingKey::from_bytes(&public).is_ok_and(|key| {
        key.verify(bytes, &Signature::from_bytes(&signature))
            .is_ok()
    })
}

pub fn sign_user(
    key: &SigningKey,
    context: nw_analytics_protocol::web::UserContext,
) -> nw_analytics_protocol::web::SignedUser {
    let mut bytes = b"NeuroWatch analytics user v1\0".to_vec();
    canonical(&serde_json::json!(context), &mut bytes);
    nw_analytics_protocol::web::SignedUser {
        context,
        signature: hex(&key.sign(&bytes).to_bytes()),
    }
}
pub fn verify_user(
    node_key: &str,
    plugin_key: &str,
    user: &nw_analytics_protocol::web::SignedUser,
    now: i64,
) -> bool {
    let context = &user.context;
    if context.plugin_key != plugin_key
        || context.expires_at < now
        || context.expires_at > now.saturating_add(60)
        || context.user.len() > 128
        || context.cameras.len() > 1024
        || !["admin", "operator", "viewer"].contains(&context.role.as_str())
        || unhex::<32>(&context.nonce).is_none()
    {
        return false;
    }
    let mut bytes = b"NeuroWatch analytics user v1\0".to_vec();
    canonical(&serde_json::json!(context), &mut bytes);
    verify(node_key, &user.signature, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signatures_bind_the_permissions_nonces_and_tls_connection() {
        let mut manifest: Manifest = serde_json::from_value(serde_json::json!({"id":"test","name":"Test","vendor":"Test","version":"1","protocol":1,"permissions":["video"],"media":{"video":{"stream":"main","codecs":["h264"]}}})).unwrap();
        let key = SigningKey::from_bytes(&[1; 32]);
        let nonce = challenge().unwrap();
        sign_manifest(&mut manifest, &key, &nonce, "tls-a").unwrap();
        assert!(verify_manifest(&manifest, &nonce, "tls-a"));
        assert!(!verify_manifest(&manifest, &nonce, "tls-b"));
        assert!(!verify_manifest(&manifest, &challenge().unwrap(), "tls-a"));
        let plugin = manifest.identity.as_ref().unwrap();
        let auth = authenticate(&SigningKey::from_bytes(&[2; 32]), &nonce, plugin, None);
        assert!(verify_authenticate(&auth, &nonce, plugin));
        assert!(!verify_authenticate(&auth, &challenge().unwrap(), plugin));
        manifest.permissions.push("decoded_frames".into());
        assert!(!verify_manifest(&manifest, &nonce, "tls-a"));
    }
}
