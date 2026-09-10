use mahoquot_providers::zcode::extract_callback_code;

#[test]
fn callback_query_decodes_opaque_code_and_state() {
    let callback = "zcode://oauth/callback?code=code%2Fwith%2Bpadding%3D&state=state%2Bvalue";
    let code = extract_callback_code(callback, "state+value");
    assert_eq!(code.unwrap(), "code/with+padding=");
}

#[test]
fn encoded_parameter_names_cannot_bypass_duplicate_checks() {
    assert!(extract_callback_code(
        "zcode://oauth/callback?code=first&%63ode=second&state=valid",
        "valid",
    )
    .is_err());
}
