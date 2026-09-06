use mahoquot_providers::derive_identity_slug_from_filename;

#[test]
fn r25_filename_identity_preserves_arbitrary_suffixes_and_legacy_codex_plans() {
    for (name, expected) in [
        ("claude-work.json", "claude-work"),
        ("claude-personal.json", "claude-personal"),
        ("generic-command-code-key-one.json", "generic-command-code-key-one"),
        ("codex-user-name.json", "user-name"),
        ("codex-user-name-pro.json", "user-name"),
        ("codex-user-name-plus.json", "user-name"),
        ("codex-user-team.json", "user"),
        ("codex-user-free.json", "user"),
        ("claude-user-pro.json", "claude-user-pro"),
    ] {
        assert_eq!(derive_identity_slug_from_filename(name), expected, "{name}");
    }
}
