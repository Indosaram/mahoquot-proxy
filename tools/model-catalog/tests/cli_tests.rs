use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "mc-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tools/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn catalog_path() -> PathBuf {
    workspace_root().join("crates/registry/catalog/models-v1.json")
}

fn key_path() -> PathBuf {
    workspace_root().join("tests/fixtures/test-ed25519.key")
}

fn run_catalog_tool(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_mahoquot-model-catalog"))
        .current_dir(workspace_root())
        .args(args)
        .output()
        .expect("failed to execute mahoquot-model-catalog binary");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), stdout, stderr)
}

#[test]
fn test_cli_validate_valid_catalog() {
    let cat = catalog_path();
    let (success, stdout, stderr) = run_catalog_tool(&["validate", cat.to_str().unwrap()]);
    assert!(success, "validation should succeed. stderr: {}", stderr);
    assert!(stdout.contains("Catalog valid:"));
    assert!(stdout.contains("version: v1"));
}

#[test]
fn test_cli_validate_malformed_catalog_fails() {
    let tmp = TempDir::new();
    let bad_path = tmp.path().join("bad.json");
    fs::write(&bad_path, "not a json").unwrap();

    let (success, _stdout, stderr) = run_catalog_tool(&["validate", bad_path.to_str().unwrap()]);
    assert!(!success, "malformed catalog should fail validation");
    assert!(stderr.contains("catalog is not valid JSON") || stderr.contains("Error"));
}

#[test]
fn test_cli_sign_and_verify_roundtrip() {
    let tmp = TempDir::new();
    let out_json = tmp.path().join("models-v1.json");
    let out_sig = tmp.path().join("models-v1.json.sig");
    let cat = catalog_path();
    let key = key_path();

    let (sign_ok, sign_out, sign_err) = run_catalog_tool(&[
        "sign",
        "--key-file",
        key.to_str().unwrap(),
        "--input",
        cat.to_str().unwrap(),
        "--output",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
    ]);
    assert!(sign_ok, "signing should succeed. stderr: {}", sign_err);
    assert!(sign_out.contains("Signed catalog:"));

    let (verify_ok, verify_out, verify_err) = run_catalog_tool(&[
        "verify",
        "--input",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
    ]);
    assert!(verify_ok, "verify should succeed. stderr: {}", verify_err);
    assert!(verify_out.contains("Verified catalog:"));
    assert!(verify_out.contains("key_id='test-ed25519-v1'"));
}

#[test]
fn test_cli_verify_detects_tampered_payload() {
    let tmp = TempDir::new();
    let out_json = tmp.path().join("models-v1.json");
    let out_sig = tmp.path().join("models-v1.json.sig");
    let cat = catalog_path();
    let key = key_path();

    let (sign_ok, _, sign_err) = run_catalog_tool(&[
        "sign",
        "--key-file",
        key.to_str().unwrap(),
        "--input",
        cat.to_str().unwrap(),
        "--output",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
    ]);
    assert!(sign_ok, "sign failed: {}", sign_err);

    // Tamper payload byte
    let content = fs::read(&out_json).unwrap();
    let mut tampered = content.clone();
    tampered.push(b' '); // Appending whitespace violates canonical JSON
    fs::write(&out_json, tampered).unwrap();

    let (verify_ok, _, stderr) = run_catalog_tool(&[
        "verify",
        "--input",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
    ]);
    assert!(!verify_ok, "tampered payload must fail verification");
    assert!(
        stderr.contains("canonicalization mismatch")
            || stderr.contains("signature verification failed")
            || stderr.contains("Error")
    );
}

#[test]
fn test_cli_verify_detects_tampered_signature() {
    let tmp = TempDir::new();
    let out_json = tmp.path().join("models-v1.json");
    let out_sig = tmp.path().join("models-v1.json.sig");
    let cat = catalog_path();
    let key = key_path();

    let (sign_ok, _, sign_err) = run_catalog_tool(&[
        "sign",
        "--key-file",
        key.to_str().unwrap(),
        "--input",
        cat.to_str().unwrap(),
        "--output",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
    ]);
    assert!(sign_ok, "sign failed: {}", sign_err);

    // Tamper signature JSON
    let sig_str = fs::read_to_string(&out_sig).unwrap();
    let mut val: serde_json::Value = serde_json::from_str(&sig_str).unwrap();
    val["signature"] = serde_json::Value::String(
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .to_string(),
    );
    fs::write(&out_sig, serde_json::to_string(&val).unwrap()).unwrap();

    let (verify_ok, _, stderr) = run_catalog_tool(&[
        "verify",
        "--input",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
    ]);
    assert!(!verify_ok, "tampered signature must fail verification");
    assert!(stderr.contains("signature verification failed") || stderr.contains("Error"));
}

#[test]
fn test_cli_verify_anti_downgrade() {
    let tmp = TempDir::new();
    let out_json = tmp.path().join("models-v1.json");
    let out_sig = tmp.path().join("models-v1.json.sig");
    let cat = catalog_path();
    let key = key_path();

    let (sign_ok, _, sign_err) = run_catalog_tool(&[
        "sign",
        "--key-file",
        key.to_str().unwrap(),
        "--input",
        cat.to_str().unwrap(),
        "--output",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
    ]);
    assert!(sign_ok, "sign failed: {}", sign_err);

    // Active version is 2, incoming is 1 -> must fail
    let (verify_ok, _, stderr) = run_catalog_tool(&[
        "verify",
        "--input",
        out_json.to_str().unwrap(),
        "--signature",
        out_sig.to_str().unwrap(),
        "--active-version",
        "2",
    ]);
    assert!(!verify_ok, "downgrade should be rejected");
    assert!(stderr.contains("anti-downgrade check failed") || stderr.contains("Error"));
}

#[test]
fn test_cli_generate_key() {
    let tmp = TempDir::new();
    let prefix = tmp.path().join("gen-test");

    let (ok, stdout, stderr) =
        run_catalog_tool(&["generate-key", "--output-prefix", prefix.to_str().unwrap()]);
    assert!(ok, "generate-key should succeed. stderr: {}", stderr);
    assert!(stdout.contains("Generated Ed25519 keypair:"));

    let priv_file = tmp.path().join("gen-test.key");
    let pub_file = tmp.path().join("gen-test.pub");
    assert!(priv_file.exists());
    assert!(pub_file.exists());

    let priv_content = fs::read_to_string(&priv_file).unwrap();
    let pub_content = fs::read_to_string(&pub_file).unwrap();
    assert_eq!(priv_content.trim().len(), 64);
    assert_eq!(pub_content.trim().len(), 64);
}
