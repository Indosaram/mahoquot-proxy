//! AI Studio worker identity and WebSocket relay message contract.
//!
//! AI Studio worker credentials are created by the connected worker, not OAuth.
//! A disconnected worker therefore has no usable credential and no local model
//! catalog to advertise.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROVIDER: &str = "aistudio";
pub const WORKER_ID_PREFIX: &str = "aistudio-";
pub const WORKER_PATH: &str = "/v1/ws";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WorkerMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub message_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<BTreeMap<String, Value>>,
}

impl WorkerMessage {
    pub fn request(
        id: impl Into<String>,
        method: &str,
        url: &str,
        headers: BTreeMap<String, Value>,
        body: &[u8],
    ) -> Self {
        let mut payload = headers;
        payload.insert("method".into(), Value::String(method.into()));
        payload.insert("url".into(), Value::String(url.into()));
        payload.insert(
            "body".into(),
            Value::String(String::from_utf8_lossy(body).into_owned()),
        );
        Self {
            id: id.into(),
            message_type: "http_request".into(),
            payload: Some(payload),
        }
    }
}

pub fn is_worker_id(id: &str) -> bool {
    id.starts_with(WORKER_ID_PREFIX) && id.len() > WORKER_ID_PREFIX.len()
}

/// The upstream worker is authoritative for its current model catalog.
/// Missing/disconnected worker metadata remains unknown rather than empty-as-zero.
pub fn connected_catalog(models: Option<&[String]>) -> Option<Vec<String>> {
    models.map(|models| {
        models
            .iter()
            .filter(|model| !model.trim().is_empty())
            .cloned()
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_id_requires_nonempty_worker_suffix() {
        assert!(is_worker_id("aistudio-peer-01"));
        assert!(!is_worker_id("aistudio-"));
        assert!(!is_worker_id("other-peer"));
    }

    #[test]
    fn request_envelope_keeps_correlation_and_worker_payload() {
        let request = WorkerMessage::request(
            "call-7",
            "POST",
            "https://service.invalid/v1beta/models/m:generateContent",
            BTreeMap::new(),
            br#"{"contents":[]}"#,
        );
        assert_eq!(request.id, "call-7");
        assert_eq!(request.message_type, "http_request");
        assert_eq!(
            request.payload.as_ref().unwrap()["body"],
            r#"{"contents":[]}"#
        );
    }

    #[test]
    fn disconnected_catalog_is_unknown_and_connected_catalog_is_remote() {
        assert_eq!(connected_catalog(None), None);
        assert_eq!(
            connected_catalog(Some(&["model-from-worker".into()])),
            Some(vec!["model-from-worker".into()])
        );
    }
}
