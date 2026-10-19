//! Cross-implementation protocol vectors.
//!
//! Every other test in this suite signs with `api_harness.rs`, which is a
//! *third* copy of the node's signing rules: it can only ever prove that copy
//! agrees with the control plane. The vectors below are different — they are
//! the node client's own recorded fixtures
//! (`crates/nw-backend/src/cloud/link/fixtures/node-auth-v1.json`), produced by the
//! node's code with a real key, and `crates/nw-api/src/protocol.rs` cites them
//! as the definition of the canonical form.
//!
//! This file is therefore the only place where the two implementations are
//! checked against each other. If the node changes its canonical form or its
//! signing inputs, this test fails.

use serde_json::Value;

use nw_api::protocol::{
    canonical_json, verify_signature, without_signature, ChallengeResponse, NODE_AUDIENCE,
    NODE_CHALLENGE_AUDIENCE, OPERATIONS, PROTOCOL_VERSION,
};

const NODE_AUTH_VECTORS: &str =
    include_str!("../../crates/nw-backend/src/cloud/link/fixtures/node-auth-v1.json");

fn vectors() -> Value {
    serde_json::from_str(NODE_AUTH_VECTORS).expect("the node's recorded vectors are valid JSON")
}

/// The four (signed body, canonical string) pairs the node recorded.
fn signed_pairs(fixture: &Value) -> Vec<(&'static str, Value, String)> {
    [
        ("challenge_request", "canonical_challenge_request"),
        ("check_in", "canonical_check_in"),
        (
            "unlink_challenge_request",
            "canonical_unlink_challenge_request",
        ),
        ("unlink", "canonical_unlink"),
    ]
    .into_iter()
    .map(|(name, canonical_key)| {
        let signed = fixture[format!("signed_{name}")].clone();
        assert!(signed.is_object(), "missing vector signed_{name}");
        let canonical = fixture[canonical_key]
            .as_str()
            .unwrap_or_else(|| panic!("missing vector {canonical_key}"))
            .to_owned();
        (name, signed, canonical)
    })
    .collect()
}

#[test]
fn the_canonical_form_is_byte_identical_to_the_nodes() {
    let fixture = vectors();
    for (name, signed, expected) in signed_pairs(&fixture) {
        let produced = canonical_json(&without_signature(&signed))
            .expect("the control plane can canonicalise a node body");
        assert_eq!(
            produced, expected,
            "{name}: the control plane signs different bytes than the node"
        );
    }
}

#[test]
fn the_nodes_own_signatures_verify_against_the_control_plane() {
    let fixture = vectors();
    let public_key = fixture["public_key"].as_str().expect("public_key");
    for (name, signed, _) in signed_pairs(&fixture) {
        let signature = signed["signature"].as_str().expect("signature");
        assert!(
            verify_signature(public_key, signature, &without_signature(&signed)),
            "{name}: a signature the node really produced was rejected"
        );
    }
}

#[test]
fn a_single_altered_field_breaks_verification() {
    let fixture = vectors();
    let public_key = fixture["public_key"].as_str().expect("public_key");
    for (name, signed, _) in signed_pairs(&fixture) {
        let signature = signed["signature"].as_str().expect("signature").to_owned();
        let fields: Vec<String> = signed
            .as_object()
            .expect("object")
            .keys()
            .filter(|key| *key != "signature")
            .cloned()
            .collect();
        assert!(fields.len() >= 8, "{name}: unexpectedly small vector");
        for field in fields {
            let mut tampered = signed.clone();
            // Replace the value with a different one of the same JSON type, so
            // the body still parses and only the signed bytes differ.
            tampered[&field] = match &signed[&field] {
                Value::String(text) => Value::String(format!("{text}x")),
                Value::Number(_) => Value::from(99_u64),
                other => panic!("{name}.{field}: unexpected vector type {other}"),
            };
            assert!(
                !verify_signature(public_key, &signature, &without_signature(&tampered)),
                "{name}: altering {field} after signing was not detected"
            );
        }
    }
}

#[test]
fn a_signature_from_another_key_is_rejected() {
    let fixture = vectors();
    let (_, signed, _) = signed_pairs(&fixture).remove(0);
    let signature = signed["signature"].as_str().expect("signature");
    // A syntactically valid ed25519 key that did not sign this body.
    let stranger = {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::SigningKey;
        let key = SigningKey::from_bytes(&[7_u8; 32]);
        URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
    };
    assert!(!verify_signature(
        &stranger,
        signature,
        &without_signature(&signed)
    ));
}

#[test]
fn the_challenge_answer_matches_the_response_the_node_recorded() {
    let fixture = vectors();
    let recorded = fixture["challenge_response"].clone();

    // Rebuild the same answer through this crate's own response type and
    // require it to serialise to exactly what the node recorded — same fields,
    // same values, nothing extra. The node deserialises this with
    // `deny_unknown_fields`, so an added field breaks every deployed node.
    let produced = serde_json::to_value(ChallengeResponse {
        protocol_version: recorded["protocol_version"].as_u64().expect("version") as u8,
        audience: NODE_AUDIENCE,
        operation: recorded["operation"]
            .as_str()
            .expect("operation")
            .to_owned(),
        challenge: recorded["challenge"]
            .as_str()
            .expect("challenge")
            .to_owned(),
        expires_at: recorded["expires_at"]
            .as_str()
            .expect("expires_at")
            .to_owned(),
    })
    .expect("serialise");

    assert_eq!(produced, recorded);
    assert_eq!(recorded["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(recorded["audience"], NODE_AUDIENCE);
    assert!(OPERATIONS.contains(&recorded["operation"].as_str().expect("operation")));
}

#[test]
fn the_audiences_in_the_vectors_are_the_ones_this_crate_uses() {
    let fixture = vectors();
    assert_eq!(
        fixture["signed_challenge_request"]["audience"],
        NODE_CHALLENGE_AUDIENCE
    );
    assert_eq!(
        fixture["signed_unlink_challenge_request"]["audience"],
        NODE_CHALLENGE_AUDIENCE
    );
    assert_eq!(fixture["signed_check_in"]["audience"], NODE_AUDIENCE);
    assert_eq!(fixture["signed_unlink"]["audience"], NODE_AUDIENCE);
    for key in [
        "signed_challenge_request",
        "signed_unlink_challenge_request",
    ] {
        assert!(OPERATIONS.contains(&fixture[key]["operation"].as_str().expect("operation")));
    }
}
