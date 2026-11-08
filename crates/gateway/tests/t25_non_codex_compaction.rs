//! Task 25: Non-Codex compaction and built-in search contracts.
use mahoquot_gateway::compat::responses::{
    is_recognized_compaction_capsule, responses_to_openai, seal_compaction_capsule,
    unseal_compaction_capsule, unseal_compaction_capsule_for_model,
};
use serde_json::json;

#[test]
fn test_capsule_sealing_and_model_binding() {
    let summary = "User discussed weather and deployment plans.";
    let model = "claude-3-7-sonnet-20250219";
    let capsule = seal_compaction_capsule(summary, model).expect("sealing succeeds");

    assert!(is_recognized_compaction_capsule(&capsule));

    // Matching model unseals correctly
    let unsealed = unseal_compaction_capsule_for_model(&capsule, model)
        .expect("unseal with matching model succeeds");
    assert_eq!(unsealed, summary);

    // Default unseal without model check succeeds
    let unsealed_plain = unseal_compaction_capsule(&capsule).expect("plain unseal succeeds");
    assert_eq!(unsealed_plain, summary);

    // Mismatched model rejects with model mismatch error
    let err = unseal_compaction_capsule_for_model(&capsule, "gemini-2.5-flash")
        .expect_err("cross-model unseal must be rejected");
    assert!(err.contains("model mismatch"));
}

#[test]
fn test_tampered_capsule_rejected() {
    let err = unseal_compaction_capsule("cpa-ag-compact-v1:invalid-tampered-data")
        .expect_err("tampered capsule must be rejected");
    assert!(err.contains("decode") || err.contains("invalid") || err.contains("short"));

    let err2 = unseal_compaction_capsule("not-a-capsule")
        .expect_err("unrecognized prefix must be rejected");
    assert!(err2.contains("unrecognized"));
}

#[test]
fn test_responses_to_openai_injects_unsealed_summary() {
    let summary = "Important background context.";
    let model = "claude-sonnet-4-6";
    let capsule = seal_compaction_capsule(summary, model).expect("seal succeeds");

    let req = json!({
        "model": model,
        "input": [
            {
                "type": "compaction",
                "encrypted_content": capsule
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "What next?"}]
            }
        ]
    });

    let openai_chat = responses_to_openai(&req).expect("conversion succeeds");
    let messages = openai_chat["messages"].as_array().expect("messages array");

    assert_eq!(messages[0]["role"], "developer");
    let dev_content = messages[0]["content"]
        .as_str()
        .expect("developer content string");
    assert!(dev_content.contains(summary));

    assert_eq!(messages[1]["role"], "user");
}

#[test]
fn test_responses_to_openai_rejects_cross_model_capsule() {
    let summary = "Secret user information.";
    let sealed_model = "claude-haiku-4-5";
    let capsule = seal_compaction_capsule(summary, sealed_model).expect("seal succeeds");

    let req = json!({
        "model": "different-target-model",
        "input": [
            {
                "type": "compaction",
                "encrypted_content": capsule
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Hello"}]
            }
        ]
    });

    let err = responses_to_openai(&req).expect_err("mismatched model capsule must be rejected");
    assert!(err.to_string().contains("model mismatch"));
}
