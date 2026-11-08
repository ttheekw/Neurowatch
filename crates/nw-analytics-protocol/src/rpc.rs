//! JSON-RPC 2.0 envelopes, as both sides send them.
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// One message, classified.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
}

#[derive(Serialize, Deserialize)]
struct Wire {
    jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

impl Message {
    pub fn request(id: u64, method: &str, params: impl Serialize) -> Self {
        Self::Request {
            id,
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }

    pub fn notification(method: &str, params: impl Serialize) -> Self {
        Self::Notification {
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }

    pub fn reply(id: u64, result: Result<impl Serialize, RpcError>) -> Self {
        Self::Response {
            id,
            result: result.map(|value| serde_json::to_value(value).unwrap_or(Value::Null)),
        }
    }

    pub fn to_value(&self) -> Value {
        let wire = match self {
            Self::Request { id, method, params } => Wire {
                jsonrpc: "2.0".into(),
                id: Some(*id),
                method: Some(method.clone()),
                params: Some(params.clone()),
                result: None,
                error: None,
            },
            Self::Notification { method, params } => Wire {
                jsonrpc: "2.0".into(),
                id: None,
                method: Some(method.clone()),
                params: Some(params.clone()),
                result: None,
                error: None,
            },
            Self::Response { id, result } => Wire {
                jsonrpc: "2.0".into(),
                id: Some(*id),
                method: None,
                params: None,
                // `Some(Null)` is written as `"result": null`, as 2.0 requires.
                result: result.as_ref().ok().cloned(),
                error: result.as_ref().err().cloned(),
            },
        };
        serde_json::to_value(wire).unwrap_or(Value::Null)
    }

    pub fn from_value(value: Value) -> Result<Self, &'static str> {
        let wire: Wire = serde_json::from_value(value).map_err(|_| "not a JSON-RPC message")?;
        if wire.jsonrpc != "2.0" {
            return Err("not JSON-RPC 2.0");
        }
        match (wire.id, wire.method) {
            (Some(id), Some(method)) => Ok(Self::Request {
                id,
                method,
                params: wire.params.unwrap_or(Value::Null),
            }),
            (None, Some(method)) => Ok(Self::Notification {
                method,
                params: wire.params.unwrap_or(Value::Null),
            }),
            (Some(id), None) => Ok(Self::Response {
                id,
                result: match wire.error {
                    Some(error) => Err(error),
                    None => Ok(wire.result.unwrap_or(Value::Null)),
                },
            }),
            (None, None) => Err("a message needs an id or a method"),
        }
    }
}

/// Typed parameters, or an `INVALID_PARAMS` error naming what was wrong.
pub fn params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new(crate::error_code::INVALID_PARAMS, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip() {
        let messages = [
            Message::request(1, "hello", json!({"protocol": 1})),
            Message::notification("status", json!({"bindings": []})),
            Message::reply(1, Ok::<_, RpcError>(json!({"ok": true}))),
            Message::reply(2, Ok::<_, RpcError>(Value::Null)),
            Message::reply(3, Err::<Value, _>(RpcError::new(-33002, "busy"))),
        ];
        for message in messages {
            let value = message.to_value();
            assert_eq!(value["jsonrpc"], "2.0");
            assert_eq!(Message::from_value(value).unwrap(), message);
        }
        let error = Message::reply(3, Err::<Value, _>(RpcError::new(-1, "x"))).to_value();
        assert!(error.get("result").is_none());
    }

    #[test]
    fn non_messages_are_refused() {
        assert!(Message::from_value(json!({"jsonrpc": "1.0", "method": "x"})).is_err());
        assert!(Message::from_value(json!({"jsonrpc": "2.0"})).is_err());
        assert!(Message::from_value(json!([1])).is_err());
    }
}
