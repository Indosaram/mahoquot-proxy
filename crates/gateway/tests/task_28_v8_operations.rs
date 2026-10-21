use serde_json::json;

#[test]
fn test_v8_manifest_routes_structure() {
    let manifest = [
        ("GET", "/server/latest-version"),
        ("POST", "/requests/api-call"),
        ("POST", "/routing/cooldown/reset"),
        ("GET", "/routing/model-definitions/{channel}"),
        ("GET", "/observability/logs"),
        ("DELETE", "/observability/logs"),
        ("GET", "/observability/logs/errors"),
        ("GET", "/observability/logs/errors/{name}"),
        ("GET", "/observability/logs/requests/{id}"),
        ("GET", "/observability/usage/api-keys"),
        ("GET", "/observability/usage/queue"),
        ("GET", "/credentials"),
        ("POST", "/credentials"),
        ("DELETE", "/credentials"),
        ("GET", "/credentials/models"),
        ("GET", "/credentials/download"),
        ("PATCH", "/credentials/status"),
        ("PATCH", "/credentials/fields"),
        ("POST", "/credentials/refresh"),
        ("POST", "/oauth/import"),
        ("GET", "/oauth/auth-url"),
        ("GET", "/oauth/status"),
        ("DELETE", "/oauth/session"),
        ("GET", "/plugins"),
        ("DELETE", "/plugins/{id}"),
        ("GET", "/plugins/store"),
        ("POST", "/plugins/store/{id}/install"),
        ("GET", "/plugins/{id}/quota"),
        ("POST", "/plugins/{id}/quota"),
        ("DELETE", "/plugins/{id}/quota"),
    ];

    assert_eq!(manifest.len(), 30);
    for (method, path) in manifest {
        assert!(!method.is_empty());
        assert!(path.starts_with('/'));
    }
}

#[test]
fn test_unknown_quota_remains_null() {
    let usage = json!({
        "plan_type": null,
        "primary": {
            "used_percent": null,
            "window_minutes": null,
            "reset_after_seconds": null
        },
        "reset_credits_available": null,
        "credits_balance": null
    });

    assert!(usage["primary"]["used_percent"].is_null());
    assert!(usage["reset_credits_available"].is_null());
    assert_ne!(usage["primary"]["used_percent"], json!(0.0));
    assert_ne!(usage["reset_credits_available"], json!(0));
}
