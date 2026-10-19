//! Local JSON-RPC facade for the optional cloud node link.

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::cloud::link::{CloudError, CloudNode, EnrollmentInput};
use crate::cloud::media_publish::PublisherSupervisor;

/// `cloud.begin` params: the portal's single enrollment code and a node name.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginRequest {
    enrollment_code: String,
    display_label: String,
}

/// The request shape used before enrollment codes, where the UI carried the
/// whole claim binding. Still accepted for one release so an older UI keeps
/// linking; new callers send an `enrollment_code` instead.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyBeginRequest {
    endpoint: String,
    enrollment_id: String,
    tenant_id: String,
    expires_at: String,
    challenge: String,
    transfer_proof: String,
    display_label: String,
}

impl From<LegacyBeginRequest> for EnrollmentInput {
    fn from(request: LegacyBeginRequest) -> Self {
        Self {
            endpoint: request.endpoint,
            enrollment_id: request.enrollment_id,
            tenant_id: request.tenant_id,
            expires_at: request.expires_at,
            challenge: request.challenge,
            transfer_proof: request.transfer_proof,
            display_label: request.display_label,
        }
    }
}

/// Either accepted `cloud.begin` shape. The two are never mixed: an
/// `enrollment_code` alongside claim-binding fields is a malformed request,
/// not a hint to guess which half the caller meant.
enum BeginInput {
    Code(BeginRequest),
    Legacy(Box<LegacyBeginRequest>),
}

fn begin_input(params: &Value) -> Result<BeginInput, RpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("cloud.begin params must be an object"))?;
    if object.contains_key("enrollment_code") {
        serde_json::from_value(params.clone())
            .map(BeginInput::Code)
            .map_err(invalid)
    } else {
        serde_json::from_value(params.clone())
            .map(|request| BeginInput::Legacy(Box::new(request)))
            .map_err(invalid)
    }
}
use nw_core::ipc::RpcError;

pub struct CloudHandler {
    node: Mutex<CloudNode>,
    publisher: Option<std::sync::Arc<PublisherSupervisor>>,
}

impl CloudHandler {
    pub fn new(node: CloudNode) -> Self {
        Self {
            node: Mutex::new(node),
            publisher: None,
        }
    }

    pub fn with_publisher(mut self, publisher: std::sync::Arc<PublisherSupervisor>) -> Self {
        self.publisher = Some(publisher);
        self
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "cloud.status" => self.report(&*self.node.lock().await),
            "cloud.begin" | "cloud.claim" => {
                // Shape errors are decided before the node is touched, so a
                // malformed request can never half-start a link.
                let input = begin_input(params)?;
                let mut node = self.node.lock().await;
                match input {
                    BeginInput::Code(request) => {
                        node.begin_with_code(&request.enrollment_code, request.display_label)
                            .await
                    }
                    BeginInput::Legacy(request) => node.begin_legacy((*request).into()).await,
                }
                .map_err(cloud_error)?;
                // Linking is one step for the operator: confirm the claim the
                // cloud just accepted right away.
                node.confirm_remote().await.map_err(cloud_error)?;
                self.report(&node)
            }
            // Finishes a link whose claim was accepted but whose confirmation
            // did not complete (for example, the network dropped in between).
            // It carries no parameters: the node signs from its saved state.
            "cloud.confirm" if params.as_object().is_some_and(serde_json::Map::is_empty) => {
                let mut node = self.node.lock().await;
                node.confirm_remote().await.map_err(cloud_error)?;
                self.report(&node)
            }
            "cloud.unlink" => {
                self.node.lock().await.unlink_local().map_err(cloud_error)?;
                if let Some(publisher) = &self.publisher {
                    publisher.cancel_all().await;
                }
                Ok(json!({"unlinked":true}))
            }
            "cloud.check_in" | "cloud.rotation_check_in" => {
                self.node
                    .lock()
                    .await
                    .check_in_once()
                    .await
                    .map_err(cloud_error)?;
                Ok(json!({"checked_in":true}))
            }
            _ => Err(RpcError::method_not_found()),
        }
    }

    fn report(&self, node: &CloudNode) -> Result<Value, RpcError> {
        let mut value = report(node)?;
        let sessions = self
            .publisher
            .as_ref()
            .map(|publisher| publisher.status())
            .unwrap_or_default();
        value["media_publish"] = json!({
            "enabled": self.publisher.is_some(),
            "sessions": sessions,
        });
        Ok(value)
    }
}

/// Every status-shaped reply carries the same fields, so a UI reading one
/// never has to guess whether a cloud is configured or an enrollment expired.
fn report(node: &CloudNode) -> Result<Value, RpcError> {
    // A poisoned state lock surfaces as a plain internal error: the caller
    // learns the request failed, never how the node stores its link.
    let status = node.report().map_err(cloud_error)?;
    serde_json::to_value(status).map_err(internal)
}

fn invalid(e: impl std::fmt::Display) -> RpcError {
    RpcError::invalid_params(e.to_string())
}
fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::internal(e.to_string())
}
fn cloud_error(e: CloudError) -> RpcError {
    match e {
        CloudError::Invalid(_) | CloudError::State => invalid(e),
        // Nothing the caller sent is wrong: this installation has no cloud.
        CloudError::NotConfigured => internal(e),
        CloudError::Store(_) | CloudError::Network(_) | CloudError::Refused(_) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forged_boolean_and_capability_are_rejected_and_status_has_no_key_material() {
        let dir = std::env::temp_dir().join(format!("nw-cloud-ipc-{}", uuid::Uuid::new_v4()));
        let handler = CloudHandler::new(CloudNode::open(dir.clone()).unwrap());
        let status = handler.dispatch("cloud.status", &json!({})).await.unwrap();
        assert_eq!(status["phase"], "unlinked");
        assert_eq!(status["media_publish"]["enabled"], false);
        assert_eq!(status["media_publish"]["sessions"], json!({}));
        assert!(!status.to_string().contains("key"));
        let error = handler
            .dispatch("cloud.confirm", &json!({"locally_confirmed":true}))
            .await
            .unwrap_err();
        assert_eq!(error.code, -32601);
        let error = handler
            .dispatch(
                "cloud.confirm",
                &json!({"confirmation_capability":"forged"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, -32601);
        let _ = std::fs::remove_dir_all(dir);
    }

    const TENANT: &str = "6ba7b811-9dad-11d1-80b4-00c04fd430c8";
    const ENROLLMENT: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
    const PROOF: &str = "KFPtAkVSlNOLCOT1Rdp634jsz4foAMaBcKzXXZBPZi0";

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("nw-cloud-ipc-{}", uuid::Uuid::new_v4()))
    }

    fn code() -> String {
        format!("{TENANT}.{ENROLLMENT}.{PROOF}")
    }

    #[tokio::test]
    async fn begin_takes_only_an_enrollment_code_and_a_node_name() {
        let dir = temp_dir();
        let handler = CloudHandler::new(CloudNode::open(dir.clone()).unwrap());
        for params in [
            json!({"enrollment_code": "not-a-code", "display_label": "Front office"}),
            json!({"enrollment_code": "a.b.c"}),
            json!({"endpoint": "https://cloud.example.com", "enrollment_code": "a.b.c", "display_label": "x"}),
        ] {
            let error = handler.dispatch("cloud.begin", &params).await.unwrap_err();
            assert_eq!(error.code, -32602, "{params}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn status_shaped_responses_carry_the_cloud_configuration_state() {
        let dir = temp_dir();
        let handler = CloudHandler::new(CloudNode::open(dir.clone()).unwrap());

        let status = handler.dispatch("cloud.status", &json!({})).await.unwrap();

        assert_eq!(status["cloud_configured"], json!(false));
        assert_eq!(status["cloud_endpoint"], Value::Null);
        assert_eq!(status["pending_expired"], json!(false));
        assert_eq!(status["phase"], "unlinked");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn legacy_request(endpoint: &str) -> Value {
        json!({
            "endpoint": endpoint,
            "enrollment_id": ENROLLMENT,
            "tenant_id": TENANT,
            "expires_at": "2099-01-01T00:00:00Z",
            "challenge": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "transfer_proof": "one-time-proof",
            "display_label": "Front office"
        })
    }

    fn configured_handler(dir: &std::path::Path, endpoint: &str) -> CloudHandler {
        let options = crate::cloud::link::CloudOptions {
            endpoint: Some(endpoint.to_owned()),
            ..crate::cloud::link::CloudOptions::default()
        };
        CloudHandler::new(CloudNode::open_with_options(dir.to_path_buf(), options).unwrap())
    }

    #[tokio::test]
    async fn the_previous_begin_request_shape_still_links_for_one_release() {
        let dir = temp_dir();
        let handler = configured_handler(&dir, "https://cloud.example.test");

        // There is no such cloud, so the call fails on the network; the point is
        // that the deprecated shape is still accepted and the claim is prepared.
        assert!(handler
            .dispatch("cloud.begin", &legacy_request("https://cloud.example.test"))
            .await
            .is_err());

        let status = handler.dispatch("cloud.status", &json!({})).await.unwrap();
        assert_eq!(status["phase"], "awaiting_claim");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_legacy_request_cannot_link_this_node_to_a_cloud_of_its_own_choosing() {
        let dir = temp_dir();
        let handler = configured_handler(&dir, "https://cloud.example.test");

        let error = handler
            .dispatch(
                "cloud.claim",
                &legacy_request("https://attacker.example.net"),
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, -32602);
        let status = handler.dispatch("cloud.status", &json!({})).await.unwrap();
        assert_eq!(status["phase"], "unlinked");
        assert_eq!(status["endpoint"], Value::Null);
        // The UI is still told what it can do instead of being left blank.
        assert_eq!(status["cloud_configured"], json!(true));
        assert_eq!(status["new_link_configured"], json!(true));
        assert_eq!(
            status["cloud_endpoint"],
            json!("https://cloud.example.test")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_legacy_request_to_an_unconfigured_node_persists_nothing() {
        let dir = temp_dir();
        let handler = CloudHandler::new(CloudNode::open(dir.clone()).unwrap());

        let error = handler
            .dispatch(
                "cloud.begin",
                &legacy_request("https://attacker.example.net"),
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, -32603);
        let status = handler.dispatch("cloud.status", &json!({})).await.unwrap();
        assert_eq!(status["phase"], "unlinked");
        assert_eq!(status["endpoint"], Value::Null);
        assert_eq!(status["cloud_configured"], json!(false));
        assert_eq!(status["new_link_configured"], json!(false));
        assert_eq!(status["cloud_endpoint"], Value::Null);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn begin_refuses_params_that_mix_or_omit_both_request_shapes() {
        let dir = temp_dir();
        let handler = CloudHandler::new(CloudNode::open(dir.clone()).unwrap());
        for params in [
            json!({}),
            json!("enrollment-code"),
            json!([code()]),
            json!({"enrollment_code": code(), "enrollment_id": ENROLLMENT, "display_label": "x"}),
            json!({"enrollment_id": ENROLLMENT, "tenant_id": TENANT}),
        ] {
            let error = handler.dispatch("cloud.begin", &params).await.unwrap_err();
            assert_eq!(error.code, -32602, "{params}");
        }
        let status = handler.dispatch("cloud.status", &json!({})).await.unwrap();
        assert_eq!(status["phase"], "unlinked");
        let _ = std::fs::remove_dir_all(dir);
    }
    #[tokio::test]
    async fn there_are_no_confirmation_side_methods() {
        let dir = std::env::temp_dir().join(format!("nw-cloud-ipc-{}", uuid::Uuid::new_v4()));
        let handler = CloudHandler::new(CloudNode::open(dir.clone()).unwrap());
        for method in ["cloud.confirmation.request", "cloud.confirmation.approve"] {
            let error = handler.dispatch(method, &json!({})).await.unwrap_err();
            assert_eq!(error.code, -32601);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
