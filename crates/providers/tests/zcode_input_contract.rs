use mahoquot_providers::zcode::{extract_callback_code, parse_zcode_input, ZcodeInput};

#[test]
fn authorize_page_without_code_resumes_approval() {
    let input = "https://chat.z.ai/auth/oauth/authorize?response_type=code&client_id=client_P8X5CMWmlaRO9gyO-KSqtg&redirect_uri=zcode://oauth/callback&state=previous";
    let ZcodeInput::AuthorizationUrl(url) = parse_zcode_input(input, "current").unwrap() else {
        panic!("authorization page must not be exchanged as a code");
    };
    let url = reqwest::Url::parse(&url).unwrap();
    assert_eq!(url.host_str(), Some("chat.z.ai"));
    assert!(url
        .query_pairs()
        .any(|(key, value)| key == "state" && value == "current"));
}

#[test]
fn callback_query_and_bare_code_recover_the_same_code() {
    for input in [
        "ZCODE://oauth/callback?code=fixture&state=current",
        "code=fixture&state=current",
        "fixture",
        "https://chat.z.ai/auth/oauth/authorize?code=fixture&state=current",
        "https://chat.z.ai/auth/oauth/authorize?redirect_uri=zcode%3A%2F%2Foauth%2Fcallback%3Fcode%3Dfixture%26state%3Dcurrent&state=current",
    ] {
        assert_eq!(extract_callback_code(input, "current").unwrap(), "fixture");
    }
}

#[test]
fn authorize_pastes_cannot_bypass_state_or_host_checks() {
    for input in [
        "https://other.example/auth/oauth/authorize?code=fixture&state=current",
        "https://chat.z.ai/auth/oauth/authorize?code=fixture&state=other",
        "https://chat.z.ai/auth/oauth/authorize?code=fixture&%63ode=other&state=current",
    ] {
        assert!(parse_zcode_input(input, "current").is_err());
    }
}
