use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use ed25519_dalek::{SigningKey, VerifyingKey};
use mahoquot_registry::{
    canonicalize_json, verify_catalog_envelope, CatalogEnvelope, CatalogSigner,
    CatalogVerificationError, CatalogVersion, Keyring, RegistrySnapshot, TEST_KEY_ID_V1,
};

#[derive(Parser, Debug)]
#[command(
    name = "mahoquot-model-catalog",
    author = "Indosaram",
    version = "1.0",
    about = "Authoring, canonicalization, signing, and verification tooling for Mahoquot model catalogs"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Validate a model catalog JSON against schema and domain invariants
    Validate(ValidateArgs),

    /// Canonicalize and sign a model catalog JSON with an Ed25519 private key
    Sign(SignArgs),

    /// Cryptographically verify a signed model catalog envelope and payload
    Verify(VerifyArgs),

    /// Canonicalize a JSON file into deterministic RFC 8785 byte order
    Canonicalize(CanonicalizeArgs),

    /// Generate a fresh Ed25519 signing keypair for development or CI
    GenerateKey(GenerateKeyArgs),
}

#[derive(Args, Debug)]
struct ValidateArgs {
    /// Path to the catalog JSON file to validate
    #[arg(value_name = "CATALOG_PATH")]
    catalog_path: Option<PathBuf>,

    /// Alternate flag for path to the catalog JSON file
    #[arg(short, long)]
    input: Option<PathBuf>,

    /// Optional path to a JSON Schema file (for informational schema checking)
    #[arg(long)]
    schema: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct SignArgs {
    /// Input catalog JSON file
    #[arg(short, long)]
    input: PathBuf,

    /// Output path for the canonicalized catalog JSON
    #[arg(short, long)]
    output: PathBuf,

    /// Output path for the detached signature envelope (.json / .sig)
    #[arg(short, long)]
    signature: PathBuf,

    /// Path to Ed25519 private key file (or set MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY)
    #[arg(short, long)]
    key_file: Option<PathBuf>,

    /// Key ID for the signature envelope
    #[arg(long, default_value = TEST_KEY_ID_V1)]
    key_id: String,

    /// Optional catalog expiration timestamp in unix epoch seconds
    #[arg(long)]
    expires_at: Option<u64>,

    /// Optional catalog expiration duration in seconds from current time
    #[arg(long)]
    expires_in_secs: Option<u64>,
}

#[derive(Args, Debug)]
struct VerifyArgs {
    /// Input catalog JSON file to verify
    #[arg(short, long)]
    input: PathBuf,

    /// Path to the detached signature envelope file
    #[arg(short, long)]
    signature: PathBuf,

    /// Optional path to Ed25519 public key file (defaults to embedded keyring)
    #[arg(short, long)]
    key_file: Option<PathBuf>,

    /// Key ID to register custom public key under
    #[arg(long)]
    key_id: Option<String>,

    /// Active version for anti-downgrade threshold check
    #[arg(long)]
    active_version: Option<u64>,

    /// Last-known-good version for anti-downgrade threshold check
    #[arg(long)]
    lkg_version: Option<u64>,

    /// Mock current epoch timestamp (defaults to system time)
    #[arg(long)]
    now: Option<u64>,

    /// Allowed clock skew in seconds
    #[arg(long, default_value = "300")]
    allowed_skew: u64,
}

#[derive(Args, Debug)]
struct CanonicalizeArgs {
    /// Input JSON file
    #[arg(short, long)]
    input: PathBuf,

    /// Output canonicalized JSON file
    #[arg(short, long)]
    output: PathBuf,
}

#[derive(Args, Debug)]
struct GenerateKeyArgs {
    /// Output prefix path (e.g. "dev-key" generates "dev-key.key" and "dev-key.pub")
    #[arg(short, long)]
    output_prefix: PathBuf,
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let clean = s.trim();
    if !clean.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(clean.len() / 2);
    for i in (0..clean.len()).step_by(2) {
        let byte = u8::from_str_radix(&clean[i..i + 2], 16).ok()?;
        bytes.push(byte);
    }
    Some(bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn load_signing_key(key_file: Option<&Path>) -> Result<SigningKey> {
    let raw_bytes = if let Some(path) = key_file {
        fs::read(path).with_context(|| format!("failed to read key file at {}", path.display()))?
    } else if let Ok(val) = std::env::var("MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY") {
        val.into_bytes()
    } else {
        bail!("private key not provided: pass --key-file or set MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY");
    };

    let text = String::from_utf8_lossy(&raw_bytes).trim().to_string();

    // 1. Try 64-character hex decode
    if let Some(bytes) = decode_hex(&text) {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            return Ok(SigningKey::from_bytes(&arr));
        }
    }

    // 2. Try base64 decode
    use base64::Engine as _;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&text) {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            return Ok(SigningKey::from_bytes(&arr));
        }
    }

    // 3. Try raw 32 bytes
    if raw_bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw_bytes);
        return Ok(SigningKey::from_bytes(&arr));
    }

    bail!("invalid Ed25519 private key: expected 32-byte seed in hex, base64, or raw format");
}

fn load_verifying_key(key_file: &Path) -> Result<VerifyingKey> {
    let raw_bytes = fs::read(key_file)
        .with_context(|| format!("failed to read public key file at {}", key_file.display()))?;
    let text = String::from_utf8_lossy(&raw_bytes).trim().to_string();

    // 1. Try 64-character hex decode
    if let Some(bytes) = decode_hex(&text) {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            return VerifyingKey::from_bytes(&arr)
                .map_err(|e| anyhow::anyhow!("invalid verifying key bytes: {}", e));
        }
    }

    // 2. Try base64 decode
    use base64::Engine as _;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&text) {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            return VerifyingKey::from_bytes(&arr)
                .map_err(|e| anyhow::anyhow!("invalid verifying key bytes: {}", e));
        }
    }

    // 3. Try raw 32 bytes
    if raw_bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw_bytes);
        return VerifyingKey::from_bytes(&arr)
            .map_err(|e| anyhow::anyhow!("invalid verifying key bytes: {}", e));
    }

    bail!("invalid Ed25519 public key: expected 32 bytes in hex, base64, or raw format");
}

fn validate_catalog_json(bytes: &[u8]) -> Result<RegistrySnapshot> {
    // 1. Valid JSON syntax
    let v: serde_json::Value =
        serde_json::from_slice(bytes).context("catalog is not valid JSON")?;

    // 2. Structural schema validation
    if !v.is_object() {
        bail!("catalog root must be a JSON object");
    }
    let obj = v.as_object().unwrap();
    if !obj.contains_key("version") {
        bail!("catalog missing required 'version' field");
    }
    if !obj.contains_key("source") {
        bail!("catalog missing required 'source' field");
    }
    if !obj.contains_key("models") {
        bail!("catalog missing required 'models' field");
    }
    if !obj.contains_key("providers") {
        bail!("catalog missing required 'providers' field");
    }

    // 3. Strongly-typed deserialization into domain snapshot
    let snapshot: RegistrySnapshot =
        serde_json::from_value(v).context("catalog failed domain deserialization")?;

    // 4. Invariant checks
    snapshot
        .validate()
        .context("catalog failed domain invariant validation")?;

    // 5. Must have at least one fallback-routable binding
    let has_routable = snapshot.models().values().any(|m| !m.bindings.is_empty());
    if !has_routable {
        bail!("catalog contains zero fallback-routable bindings");
    }

    Ok(snapshot)
}

fn cmd_validate(args: ValidateArgs) -> Result<()> {
    let path = match (args.catalog_path, args.input) {
        (Some(p), _) => p,
        (None, Some(p)) => p,
        (None, None) => bail!("no catalog path provided: specify CATALOG_PATH or --input <PATH>"),
    };

    let raw = fs::read(&path)
        .with_context(|| format!("failed to read catalog file at {}", path.display()))?;

    let snapshot = validate_catalog_json(&raw)?;

    println!(
        "Catalog valid: {} (version: {}, models: {}, providers: {}, aliases: {}, exclusions: {})",
        path.display(),
        snapshot.version(),
        snapshot.models().len(),
        snapshot.providers().len(),
        snapshot.aliases().len(),
        snapshot.exclusions().len()
    );

    Ok(())
}

fn cmd_sign(args: SignArgs) -> Result<()> {
    let raw = fs::read(&args.input)
        .with_context(|| format!("failed to read input catalog at {}", args.input.display()))?;

    // Always validate catalog invariants before signing
    let snapshot = validate_catalog_json(&raw).context("cannot sign an invalid catalog")?;

    let canonical_payload = canonicalize_json(&raw)
        .map_err(|e| anyhow::anyhow!("failed to canonicalize catalog JSON: {}", e))?;

    let signing_key = load_signing_key(args.key_file.as_deref())?;
    let signer = CatalogSigner::new(signing_key, &args.key_id);

    let now = now_epoch_secs();
    let expires_at = match (args.expires_at, args.expires_in_secs) {
        (Some(ts), _) => Some(ts),
        (None, Some(dur)) => Some(now.saturating_add(dur)),
        (None, None) => None,
    };

    let envelope = signer
        .sign_catalog(snapshot.version(), now, expires_at, &canonical_payload)
        .map_err(|e| anyhow::anyhow!("signing failed: {}", e))?;

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&args.output, &canonical_payload).with_context(|| {
        format!(
            "failed to write canonical output to {}",
            args.output.display()
        )
    })?;

    if let Some(parent) = args.signature.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let envelope_json = envelope
        .to_json()
        .map_err(|e| anyhow::anyhow!("failed to serialize envelope: {}", e))?;
    fs::write(&args.signature, envelope_json).with_context(|| {
        format!(
            "failed to write signature envelope to {}",
            args.signature.display()
        )
    })?;

    println!(
        "Signed catalog: version={}, key_id='{}', models={}, generated_at={}",
        snapshot.version(),
        args.key_id,
        snapshot.models().len(),
        now
    );
    println!("  Output payload: {}", args.output.display());
    println!("  Output signature: {}", args.signature.display());

    Ok(())
}

fn cmd_verify(args: VerifyArgs) -> Result<()> {
    let payload = fs::read(&args.input).with_context(|| {
        format!(
            "failed to read input catalog payload at {}",
            args.input.display()
        )
    })?;

    let env_json = fs::read_to_string(&args.signature)
        .with_context(|| format!("failed to read envelope at {}", args.signature.display()))?;

    let envelope = CatalogEnvelope::from_json(&env_json)
        .map_err(|e| anyhow::anyhow!("malformed envelope JSON: {}", e))?;

    let mut keyring = Keyring::embedded_default();

    if let Some(key_path) = &args.key_file {
        let vk = load_verifying_key(key_path)?;
        let kid = args.key_id.as_deref().unwrap_or(envelope.key_id.as_str());
        keyring.add_key(kid, vk);
    }

    let now = args.now.unwrap_or_else(now_epoch_secs);
    let active_ver = args.active_version.map(CatalogVersion::new);
    let lkg_ver = args.lkg_version.map(CatalogVersion::new);

    let snapshot = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        active_ver,
        lkg_ver,
        now,
        args.allowed_skew,
    )
    .map_err(|e| match e {
        CatalogVerificationError::SignatureVerificationFailed => {
            eprintln!("signature verification failed");
            anyhow::anyhow!("signature verification failed")
        }
        other => anyhow::anyhow!("{}", other),
    })?;

    println!(
        "Verified catalog: version={}, source={}, models={}, key_id='{}', generated_at={}",
        snapshot.version(),
        snapshot.source(),
        snapshot.models().len(),
        envelope.key_id,
        envelope.generated_at
    );

    Ok(())
}

fn cmd_canonicalize(args: CanonicalizeArgs) -> Result<()> {
    let raw = fs::read(&args.input)
        .with_context(|| format!("failed to read {}", args.input.display()))?;

    let canonical =
        canonicalize_json(&raw).map_err(|e| anyhow::anyhow!("canonicalization failed: {}", e))?;

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&args.output, canonical)
        .with_context(|| format!("failed to write {}", args.output.display()))?;

    println!("Canonicalized JSON written to {}", args.output.display());
    Ok(())
}

fn cmd_generate_key(args: GenerateKeyArgs) -> Result<()> {
    use rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    let priv_hex = encode_hex(&signing_key.to_bytes());
    let pub_hex = encode_hex(verifying_key.as_bytes());

    let prefix_str = args.output_prefix.to_string_lossy();
    let priv_path = PathBuf::from(format!("{}.key", prefix_str));
    let pub_path = PathBuf::from(format!("{}.pub", prefix_str));

    if let Some(parent) = priv_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    fs::write(&priv_path, format!("{}\n", priv_hex))?;
    fs::write(&pub_path, format!("{}\n", pub_hex))?;

    println!("Generated Ed25519 keypair:");
    println!("  Private key: {}", priv_path.display());
    println!("  Public key:  {}", pub_path.display());
    println!("  Public key hex: {}", pub_hex);

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Validate(args) => cmd_validate(args),
        Commands::Sign(args) => cmd_sign(args),
        Commands::Verify(args) => cmd_verify(args),
        Commands::Canonicalize(args) => cmd_canonicalize(args),
        Commands::GenerateKey(args) => cmd_generate_key(args),
    }
}
