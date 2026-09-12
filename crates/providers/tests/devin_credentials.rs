//! Devin credential provider contract tests (plan P1).
//!
//! Contract under test, per `.omo/plans/devin-provider-integration.md` §6.1:
//! - Official Devin CLI `credentials.toml` (`windsurf_api_key`, `api_server_url`)
//!   parsed with a real TOML parser and normalized into `DevinAccount`.
//! - JSON account form: type=devin, stable `identity_slug`, optional
//!   label/email, `access_token` (NOT a JWT), optional `api_server_url`
//!   defaulting to https://server.codeium.com, `disabled` flag.
//! - Session token must be non-empty with no whitespace/control characters
//!   (header-injection safe). Auth header is the LITERAL
//!   `Authorization: Basic <token>-<token>` — never base64.
//! - Secret values never appear in Debug or error output.
//! - Path resolution is injectable (explicit > XDG_DATA_HOME > ~/.local/share)
//!   and never touches the process environment.
//! - Original CLI bytes are preserved; no refresh/OAuth surface is invented.

use std::path::{Path, PathBuf};

use mahoquot_providers::devin::{
    parse_cli_credentials, resolve_credentials_path, sanitize_toml_error, sanitize_url_for_debug,
    DevinAccount, DevinCliCredentials, DevinCredentialsError, DEVIN_DEFAULT_API_SERVER_URL,
    DEVIN_TYPE,
};

const VALID_TOKEN: &str = "devin-session-token$abc123";
const OTHER_TOKEN: &str = "devin-session-token$xyz789";

fn account_with_token(token: &str) -> DevinAccount {
    DevinAccount {
        provider_type: DEVIN_TYPE.to_string(),
        identity_slug: "devin-work".to_string(),
        label: Some("Devin Work".to_string()),
        email: None,
        access_token: token.to_string(),
        api_server_url: DEVIN_DEFAULT_API_SERVER_URL.to_string(),
        disabled: false,
    }
}

fn write_toml(
    dir: &Path,
    name: &str,
    windsurf_api_key: Option<&str>,
    api_server_url: Option<&str>,
) -> PathBuf {
    let mut body = String::new();
    if let Some(key) = windsurf_api_key {
        body.push_str(&format!("windsurf_api_key = \"{key}\"\n"));
    }
    if let Some(url) = api_server_url {
        body.push_str(&format!("api_server_url = \"{url}\"\n"));
    }
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write cli toml");
    path
}

fn tmp_dir(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mahoquot-devin-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

// ---------------------------------------------------------------------------
// JSON account schema
// ---------------------------------------------------------------------------

#[test]
fn json_round_trip_has_devin_type_and_optional_fields() {
    let json = r#"{
        "type": "devin",
        "identity_slug": "devin-work",
        "label": "Devin Work",
        "access_token": "tok-no-jwt",
        "disabled": false
    }"#;
    let acct: DevinAccount = serde_json::from_str(json).expect("parse devin account json");
    assert_eq!(acct.identity_slug(), "devin-work");
    assert_eq!(acct.label(), Some("Devin Work"));
    assert_eq!(acct.email(), None);
    assert_eq!(acct.access_token_secret(), "tok-no-jwt");
    assert_eq!(acct.api_server_url(), DEVIN_DEFAULT_API_SERVER_URL);
    assert!(!acct.disabled());

    let serialized = serde_json::to_value(&acct).expect("serialize");
    assert_eq!(serialized["type"], "devin");
    assert_eq!(serialized["identity_slug"], "devin-work");
    assert_eq!(serialized["access_token"], "tok-no-jwt");
    assert_eq!(serialized["api_server_url"], DEVIN_DEFAULT_API_SERVER_URL);
    assert_eq!(serialized["disabled"], false);
    assert_eq!(serialized["email"], serde_json::Value::Null);
}

#[test]
fn api_server_url_default_is_official_upstream() {
    assert_eq!(DEVIN_DEFAULT_API_SERVER_URL, "https://server.codeium.com");
}

#[test]
fn empty_identity_slug_is_rejected() {
    let err = account_with_token(VALID_TOKEN)
        .with_identity_slug("   ")
        .validate();
    assert!(matches!(
        err,
        Err(DevinCredentialsError::InvalidIdentity { .. })
    ));
}

#[test]
fn empty_whitespace_and_control_token_is_rejected() {
    for bad in [
        "",
        "   ",
        "\t",
        "tok\ntoken",
        "tok\rtoken",
        "tok token",
        "tok\u{0}x",
    ] {
        let err = account_with_token(bad).validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidToken { .. })),
            "token {bad:?} must be rejected"
        );
    }
}

#[test]
fn header_injection_attempt_is_rejected() {
    let injected = format!("{VALID_TOKEN}\nAuthorization: Bearer stolen");
    let err = account_with_token(&injected).validate();
    assert!(matches!(
        err,
        Err(DevinCredentialsError::InvalidToken { .. })
    ));
}

#[test]
fn auth_header_is_literal_basic_token_token_not_base64() {
    let acct = account_with_token(VALID_TOKEN).validate().expect("valid");
    let (name, value) = acct.authorization_header();
    assert_eq!(name, "Authorization");
    assert_eq!(value, format!("Basic {VALID_TOKEN}-{VALID_TOKEN}"));
    // Explicitly not the base64 encoding of "user:pass".
    use base64::Engine;
    let b64 =
        base64::engine::general_purpose::STANDARD.encode(format!("{VALID_TOKEN}:{VALID_TOKEN}"));
    assert_ne!(value, format!("Basic {b64}"));
}

#[test]
fn debug_and_errors_redact_the_token() {
    let acct = account_with_token(VALID_TOKEN).validate().expect("valid");
    let debug = format!("{acct:?}");
    assert!(
        !debug.contains(VALID_TOKEN),
        "token leaked via Debug: {debug}"
    );
    assert!(debug.contains("[REDACTED]"));

    let leaked = format!("{VALID_TOKEN}\nnext-line");
    let err = account_with_token(&leaked).validate().expect_err("invalid");
    let err_text = format!("{err:?}");
    assert!(
        !err_text.contains("devin-session-token"),
        "token leaked via error Debug: {err_text}"
    );
}

#[test]
fn bad_url_is_rejected_at_the_boundary() {
    for bad in [
        "http://user:pass@server.codeium.com", // credentials in URL
        "ftp://server.codeium.com",
        "https://evil.com@example.com",
        "not a url",
        "https://",
    ] {
        let err = account_with_token(VALID_TOKEN)
            .with_api_server_url(bad)
            .validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidUrl { .. })),
            "url {bad:?} must be rejected"
        );
    }
    // Accepted forms.
    for ok in [
        "https://server.codeium.com",
        "https://server.codeium.com/",
        DEVIN_DEFAULT_API_SERVER_URL,
    ] {
        account_with_token(VALID_TOKEN)
            .with_api_server_url(ok)
            .validate()
            .unwrap_or_else(|_| panic!("url {ok:?} must be accepted"));
    }
}

#[test]
fn disabled_account_keeps_token_but_reports_flag() {
    let acct = DevinAccount {
        disabled: true,
        ..account_with_token(VALID_TOKEN)
    }
    .validate()
    .expect("disabled account still validates");
    assert!(acct.disabled());
}

// ---------------------------------------------------------------------------
// Official CLI credentials.toml parsing
// ---------------------------------------------------------------------------

#[test]
fn parses_official_cli_toml_and_normalizes_fields() {
    let dir = tmp_dir("parse");
    let path = write_toml(
        &dir,
        "credentials.toml",
        Some(VALID_TOKEN),
        Some("https://server.codeium.com/"),
    );
    let acct = mahoquot_providers::devin::load_devin_account(&path).expect("load");
    assert_eq!(acct.access_token_secret(), VALID_TOKEN);
    assert_eq!(acct.api_server_url(), "https://server.codeium.com/");
    assert_eq!(acct.identity_slug(), "credentials");
    assert_eq!(acct.provider_type(), DEVIN_TYPE);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_toml_without_api_server_url_uses_default() {
    let dir = tmp_dir("default-url");
    let path = write_toml(&dir, "credentials.toml", Some(VALID_TOKEN), None);
    let acct = mahoquot_providers::devin::load_devin_account(&path).expect("load");
    assert_eq!(acct.api_server_url(), DEVIN_DEFAULT_API_SERVER_URL);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_toml_missing_key_is_an_error() {
    let dir = tmp_dir("missing-key");
    let path = write_toml(
        &dir,
        "credentials.toml",
        None,
        Some("https://server.codeium.com"),
    );
    let err = mahoquot_providers::devin::load_devin_account(&path).expect_err("missing key");
    assert!(matches!(err, DevinCredentialsError::Parse { .. }));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn corrupt_toml_is_an_error() {
    let dir = tmp_dir("corrupt");
    let path = dir.join("credentials.toml");
    std::fs::write(&path, "windsurf_api_key = [unclosed\nbroken ==").expect("write");
    let err = mahoquot_providers::devin::load_devin_account(&path).expect_err("corrupt toml");
    assert!(matches!(err, DevinCredentialsError::Parse { .. }));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn io_error_on_absent_cli_file() {
    let dir = tmp_dir("absent");
    let err = mahoquot_providers::devin::load_devin_account(&dir.join("nope.toml"))
        .expect_err("absent file");
    assert!(matches!(err, DevinCredentialsError::Io { .. }));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_toml_with_bad_key_value_is_rejected() {
    let dir = tmp_dir("bad-key");
    let path = write_toml(&dir, "credentials.toml", Some(r"bad	key"), None);
    let err = mahoquot_providers::devin::load_devin_account(&path).expect_err("bad token");
    assert!(matches!(err, DevinCredentialsError::InvalidToken { .. }));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_toml_with_credentials_in_url_is_rejected() {
    let dir = tmp_dir("url-creds");
    let path = write_toml(
        &dir,
        "credentials.toml",
        Some(VALID_TOKEN),
        Some("https://user:pass@server.codeium.com"),
    );
    let err = mahoquot_providers::devin::load_devin_account(&path).expect_err("url creds");
    assert!(matches!(err, DevinCredentialsError::InvalidUrl { .. }));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn original_cli_bytes_are_preserved_after_load() {
    let dir = tmp_dir("preserve");
    let path = write_toml(&dir, "credentials.toml", Some(VALID_TOKEN), None);
    let before = std::fs::read(&path).expect("read before");
    let acct = mahoquot_providers::devin::load_devin_account(&path).expect("load");
    acct.replace_token(OTHER_TOKEN);
    let after = std::fs::read(&path).expect("read after");
    assert_eq!(before, after, "original CLI file must never be modified");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn token_replacement_keeps_identity() {
    let dir = tmp_dir("replace");
    let path = write_toml(&dir, "credentials.toml", Some(VALID_TOKEN), None);
    let acct = mahoquot_providers::devin::load_devin_account(&path).expect("load");
    let replaced = acct.replace_token(OTHER_TOKEN);
    assert_eq!(replaced.identity_slug(), acct.identity_slug());
    assert_eq!(replaced.access_token_secret(), OTHER_TOKEN);
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------------------
// Path resolution (injectable, no global env access)
// ---------------------------------------------------------------------------

#[test]
fn explicit_path_wins() {
    let resolved = resolve_credentials_path(
        Some(Path::new("/explicit/devin/creds.toml")),
        Some(Path::new("/xdg")),
        Some(Path::new("/home/u")),
    );
    assert_eq!(resolved, PathBuf::from("/explicit/devin/creds.toml"));
}

#[test]
fn xdg_data_home_wins_over_local_share() {
    let resolved = resolve_credentials_path(
        None,
        Some(Path::new("/xdg/data")),
        Some(Path::new("/home/u")),
    );
    assert_eq!(resolved, PathBuf::from("/xdg/data/devin/credentials.toml"));
}

#[test]
fn default_falls_back_to_local_share() {
    let resolved = resolve_credentials_path(None, None, Some(Path::new("/home/u")));
    assert_eq!(
        resolved,
        PathBuf::from("/home/u/.local/share/devin/credentials.toml")
    );
}

#[test]
fn empty_xdg_value_is_treated_as_unset() {
    let resolved = resolve_credentials_path(None, Some(Path::new("")), Some(Path::new("/home/u")));
    assert_eq!(
        resolved,
        PathBuf::from("/home/u/.local/share/devin/credentials.toml")
    );
}

#[test]
fn resolution_is_total_even_with_missing_home() {
    // No inputs at all still yields a deterministic (if unusable) path rather
    // than reading process state.
    let resolved = resolve_credentials_path(None, None, None);
    assert_eq!(
        resolved,
        PathBuf::from(".local/share/devin/credentials.toml")
    );
}

#[test]
fn empty_explicit_path_falls_back_to_xdg_or_home() {
    let resolved = resolve_credentials_path(
        Some(Path::new("")),
        Some(Path::new("/xdg/data")),
        Some(Path::new("/home/u")),
    );
    assert_eq!(resolved, PathBuf::from("/xdg/data/devin/credentials.toml"));

    let resolved_home =
        resolve_credentials_path(Some(Path::new("")), None, Some(Path::new("/home/u")));
    assert_eq!(
        resolved_home,
        PathBuf::from("/home/u/.local/share/devin/credentials.toml")
    );
}

#[test]
fn devin_credentials_path_env_constant() {
    assert_eq!(
        mahoquot_providers::devin::DEVIN_CREDENTIALS_PATH_ENV,
        "DEVIN_CREDENTIALS_PATH"
    );
}

#[test]
fn token_exceeding_max_length_is_rejected() {
    let long_token = "a".repeat(4097);
    let err = account_with_token(&long_token).validate();
    assert!(matches!(
        err,
        Err(DevinCredentialsError::InvalidToken { .. })
    ));
}

#[test]
fn url_with_whitespace_or_control_chars_is_rejected() {
    for bad in [
        "https://server.codeium.com\nEvil: true",
        "https://server.codeium.com\r\nEvil: true",
        "https://server .codeium.com",
        "https://server.codeium.com\t",
    ] {
        let err = account_with_token(VALID_TOKEN)
            .with_api_server_url(bad)
            .validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidUrl { .. })),
            "url {bad:?} must be rejected"
        );
    }
}

#[test]
fn devin_cli_credentials_debug_redacts_token_and_url_credentials() {
    let creds = DevinCliCredentials {
        windsurf_api_key: "secret-cli-token-xyz".to_string(),
        api_server_url: Some("https://user:pass123@server.codeium.com".to_string()),
    };
    let debug = format!("{creds:?}");
    assert!(
        !debug.contains("secret-cli-token-xyz"),
        "token leaked in DevinCliCredentials Debug: {debug}"
    );
    assert!(
        !debug.contains("pass123"),
        "url password leaked in DevinCliCredentials Debug: {debug}"
    );
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn toml_parse_error_does_not_leak_source_token_line() {
    let dir = tmp_dir("leak-token-err");
    let path = dir.join("credentials.toml");
    let secret = "devin-secret-token-do-not-leak";
    let malformed = format!("windsurf_api_key = \"{secret}\nunclosed string here\n");
    std::fs::write(&path, &malformed).expect("write");
    let err = mahoquot_providers::devin::load_devin_account(&path).expect_err("should fail");
    let err_msg = format!("{err}");
    let err_debug = format!("{err:?}");
    assert!(
        !err_msg.contains(secret),
        "token leaked in Parse error Display: {err_msg}"
    );
    assert!(
        !err_debug.contains(secret),
        "token leaked in Parse error Debug: {err_debug}"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn invalid_url_error_redacts_credentials_in_url() {
    let secret_pass = "super_secret_password";
    let bad_url = format!("https://user:{secret_pass}@server.codeium.com");
    let err = account_with_token(VALID_TOKEN)
        .with_api_server_url(&bad_url)
        .validate()
        .expect_err("url with userinfo must be rejected");
    let err_msg = format!("{err}");
    let err_debug = format!("{err:?}");
    assert!(
        !err_msg.contains(secret_pass),
        "password leaked in InvalidUrl Display: {err_msg}"
    );
    assert!(
        !err_debug.contains(secret_pass),
        "password leaked in InvalidUrl Debug: {err_debug}"
    );
    assert!(err_msg.contains("[REDACTED]"));
}

#[test]
fn devin_account_debug_redacts_credentials_in_api_server_url() {
    let secret_pass = "leaked_in_acct_debug";
    let bad_url = format!("https://user:{secret_pass}@server.codeium.com");
    let acct = DevinAccount {
        provider_type: DEVIN_TYPE.to_string(),
        identity_slug: "test".to_string(),
        label: None,
        email: None,
        access_token: VALID_TOKEN.to_string(),
        api_server_url: bad_url,
        disabled: false,
    };
    let debug = format!("{acct:?}");
    assert!(
        !debug.contains(secret_pass),
        "password leaked in DevinAccount Debug: {debug}"
    );
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn non_devin_provider_type_is_rejected() {
    for bad_type in ["openai", "codex", "custom", "DEVIN", " devin "] {
        let acct = DevinAccount {
            provider_type: bad_type.to_string(),
            ..account_with_token(VALID_TOKEN)
        };
        let err = acct.validate();
        assert!(err.is_err(), "provider_type {bad_type:?} must be rejected");
    }
}

#[test]
fn token_with_whitespace_at_edges_is_rejected() {
    for bad in [
        " devin-session-token$123",
        "devin-session-token$123 ",
        " devin-session-token$123 ",
        "devin-session-token$123\n",
        "\ndevin-session-token$123",
        "devin-session-token$123\r\n",
        "\r\ndevin-session-token$123",
        "\tdevin-session-token$123",
        "devin-session-token$123\t",
    ] {
        let err = account_with_token(bad).validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidToken { .. })),
            "token with edge whitespace {bad:?} must be rejected without mutating or trimming"
        );
    }
}

#[test]
fn url_with_whitespace_at_edges_is_rejected() {
    for bad in [
        " https://server.codeium.com",
        "https://server.codeium.com ",
        " https://server.codeium.com ",
        "\nhttps://server.codeium.com",
        "https://server.codeium.com\n",
        "\r\nhttps://server.codeium.com",
        "https://server.codeium.com\r\n",
        "\thttps://server.codeium.com",
        "https://server.codeium.com\t",
    ] {
        let err = account_with_token(VALID_TOKEN)
            .with_api_server_url(bad)
            .validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidUrl { .. })),
            "url with edge whitespace {bad:?} must be rejected"
        );
    }
}

#[test]
fn url_with_query_or_fragment_is_rejected() {
    for bad in [
        "https://server.codeium.com?query=1",
        "https://server.codeium.com/?token=secret",
        "https://server.codeium.com#section",
        "https://server.codeium.com/#fragment",
        "https://server.codeium.com/?token=secret#frag",
    ] {
        let err = account_with_token(VALID_TOKEN)
            .with_api_server_url(bad)
            .validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidUrl { .. })),
            "url with query or fragment {bad:?} must be rejected"
        );
    }
}

#[test]
fn url_with_malformed_authority_or_port_is_rejected() {
    for bad in [
        "https://server.codeium.com:99999999",
        "https://server.codeium.com:0",
        "https://server.codeium.com:abc",
        "https://:8080",
        "https://",
        "https:///path",
    ] {
        let err = account_with_token(VALID_TOKEN)
            .with_api_server_url(bad)
            .validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidUrl { .. })),
            "url with malformed authority/port {bad:?} must be rejected"
        );
    }
}

#[test]
fn url_with_path_traversal_is_rejected() {
    for bad in [
        "https://server.codeium.com/../evil",
        "https://server.codeium.com/foo/../bar",
        "https://server.codeium.com//double-slash",
    ] {
        let err = account_with_token(VALID_TOKEN)
            .with_api_server_url(bad)
            .validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidUrl { .. })),
            "url with path traversal {bad:?} must be rejected"
        );
    }
}

#[test]
fn url_sanitizer_does_not_leak_secrets_in_unsupported_scheme_query_or_path() {
    let secret = "super_secret_leak_test_token";
    let bad_url = format!("ftp://evil.com?token={secret}");
    let sanitized = sanitize_url_for_debug(&bad_url);
    assert!(
        !sanitized.contains(secret),
        "secret leaked in sanitized url: {sanitized}"
    );

    let err = account_with_token(VALID_TOKEN)
        .with_api_server_url(&bad_url)
        .validate()
        .expect_err("unsupported scheme with query must fail");
    let err_display = format!("{err}");
    let err_debug = format!("{err:?}");
    assert!(
        !err_display.contains(secret),
        "secret leaked in InvalidUrl Display: {err_display}"
    );
    assert!(
        !err_debug.contains(secret),
        "secret leaked in InvalidUrl Debug: {err_debug}"
    );

    let path_secret = "secret_in_raw_path_xyz";
    let path_url = format!("https://server.codeium.com/token/{path_secret}");
    let sanitized_path = sanitize_url_for_debug(&path_url);
    assert!(
        !sanitized_path.contains(path_secret),
        "path secret leaked in sanitized url: {sanitized_path}"
    );
}

#[test]
fn toml_semantic_and_type_parse_errors_do_not_leak_scalar_values() {
    let dummy_path = Path::new("test/credentials.toml");

    // Bad type integer for windsurf_api_key
    let secret_int = "987654321";
    let bad_key_toml = format!("windsurf_api_key = {secret_int}\n");
    let err = parse_cli_credentials(bad_key_toml.as_bytes(), dummy_path)
        .expect_err("integer windsurf_api_key must fail");
    let err_display = format!("{err}");
    let err_debug = format!("{err:?}");
    assert!(
        !err_display.contains(secret_int),
        "secret integer leaked in Parse error Display: {err_display}"
    );
    assert!(
        !err_debug.contains(secret_int),
        "secret integer leaked in Parse error Debug: {err_debug}"
    );

    // Bad type integer for api_server_url
    let secret_url_int = "12345678";
    let bad_url_toml =
        format!("windsurf_api_key = \"valid-token\"\napi_server_url = {secret_url_int}\n");
    let err2 = parse_cli_credentials(bad_url_toml.as_bytes(), dummy_path)
        .expect_err("integer api_server_url must fail");
    let err2_display = format!("{err2}");
    let err2_debug = format!("{err2:?}");
    assert!(
        !err2_display.contains(secret_url_int),
        "secret url integer leaked in Parse error Display: {err2_display}"
    );
    assert!(
        !err2_debug.contains(secret_url_int),
        "secret url integer leaked in Parse error Debug: {err2_debug}"
    );

    // Bad type sequence containing secret string
    let secret_seq_token = "secret-token-in-seq-999";
    let bad_seq_toml = format!("windsurf_api_key = [\"{secret_seq_token}\"]\n");
    let err3 = parse_cli_credentials(bad_seq_toml.as_bytes(), dummy_path)
        .expect_err("sequence windsurf_api_key must fail");
    let err3_display = format!("{err3}");
    let err3_debug = format!("{err3:?}");
    assert!(
        !err3_display.contains(secret_seq_token),
        "secret token in sequence leaked in Parse error Display: {err3_display}"
    );
    assert!(
        !err3_debug.contains(secret_seq_token),
        "secret token in sequence leaked in Parse error Debug: {err3_debug}"
    );
}

#[test]
fn identity_slug_enforces_safe_filename_without_collapsing() {
    // Distinct existing identities are preserved without collapsing
    let acct_work = account_with_token(VALID_TOKEN)
        .with_identity_slug("work")
        .validate()
        .expect("work is valid");
    let acct_devin_work = account_with_token(VALID_TOKEN)
        .with_identity_slug("devin-work")
        .validate()
        .expect("devin-work is valid");
    assert_eq!(acct_work.identity_slug(), "work");
    assert_eq!(acct_devin_work.identity_slug(), "devin-work");
    assert_ne!(acct_work.identity_slug(), acct_devin_work.identity_slug());

    // Other safe slugs
    for safe in ["devin", "credentials", "account_1", "acc-2", "user.name"] {
        let acct = account_with_token(VALID_TOKEN)
            .with_identity_slug(safe)
            .validate()
            .unwrap_or_else(|e| panic!("slug {safe:?} should be valid, got: {e}"));
        assert_eq!(acct.identity_slug(), safe);
    }

    // Traversal, separators, control, and whitespace must be rejected
    for bad in [
        "..",
        ".",
        "../evil",
        "evil/..",
        "work/devin",
        "work\\devin",
        "work:devin",
        "work\0",
        "work\ndevin",
        " work",
        "work ",
        "work devin",
        "\twork",
        "work?test",
        "work*test",
    ] {
        let err = account_with_token(VALID_TOKEN)
            .with_identity_slug(bad)
            .validate();
        assert!(
            matches!(err, Err(DevinCredentialsError::InvalidIdentity { .. })),
            "bad slug {bad:?} must be rejected"
        );
    }
}

#[test]
fn toml_parse_error_does_not_leak_scalar_containing_expected_and_sentinel() {
    let dummy_path = Path::new("test/credentials.toml");

    // Invalid scalar containing literal expected and sentinel in parse_cli_credentials
    let sentinel = "secret-sentinel-xyz";
    let bad_toml = format!("windsurf_api_key = \"tok\"\napi_server_url = expected {sentinel}\n");
    let err = parse_cli_credentials(bad_toml.as_bytes(), dummy_path)
        .expect_err("bare expected-sentinel must fail");
    let err_display = format!("{err}");
    let err_debug = format!("{err:?}");
    assert!(
        !err_display.contains(sentinel),
        "sentinel leaked in Parse error Display: {err_display}"
    );
    assert!(
        !err_debug.contains(sentinel),
        "sentinel leaked in Parse error Debug: {err_debug}"
    );

    // Semantic wrong-type scalar containing literal expected and sentinel
    let bad_acct_toml = format!(
        "provider_type = \"devin\"\nidentity_slug = \"id\"\naccess_token = \"tok\"\napi_server_url = \"https://server.codeium.com\"\ndisabled = \"expected {sentinel}\"\n"
    );
    let toml_err = toml::from_str::<DevinAccount>(&bad_acct_toml).expect_err("bad type must fail");
    let sanitized = sanitize_toml_error(&toml_err);
    assert!(
        !sanitized.contains(sentinel),
        "sentinel leaked in sanitized toml error: {sanitized}"
    );
    assert!(
        !sanitized.contains("expected secret-sentinel"),
        "expected-sentinel leaked in sanitized toml error: {sanitized}"
    );
}
