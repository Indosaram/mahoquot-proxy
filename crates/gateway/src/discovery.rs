use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MDNS_IPV4_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_PORT: u16 = 5353;

pub const DEFAULT_SERVICE_TYPE: &str = "_ai-gateway._tcp";
pub const DEFAULT_DOMAIN: &str = "local.";
pub const SUBTYPE_CHAT_COMPLETIONS: &str = "_chat-completions";
pub const SUBTYPE_RESPONSES: &str = "_responses";
pub const SUBTYPE_MESSAGES: &str = "_messages";
pub const SUBTYPE_GENERATE_CONTENT: &str = "_generate-content";
pub const SUBTYPE_INTERACTIONS: &str = "_interactions";
pub const PRODUCT_CPA: &str = "cliproxyapi";
pub const DEFAULT_INSTANCE_PREFIX: &str = "CPA-";
pub const MAX_TXT_RECORD_BYTES: usize = 255;
pub const MAX_TXT_BYTES: usize = 400;

pub const IGNORED_INTERFACE_PREFIXES: &[&str] = &[
    "docker",
    "veth",
    "utun",
    "tailscale",
    "wg",
    "tun",
    "tap",
    "br-",
    "cni",
    "flannel",
    "virbr",
    "vmnet",
    "vboxnet",
    "awdl",
    "llw",
];

pub const LIKELY_PHYSICAL_LAN_PREFIXES: &[&str] = &[
    "en", "eth", "em", "igb", "ix", "re", "wl", "wlan", "wifi", "wi-fi", "ethernet", "bond",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxtOptions {
    pub version: String,
    pub product: String,
    pub protocols: Vec<String>,
    pub features: Vec<String>,
    pub api_path_openai: String,
    pub api_path_anthropic: String,
    pub api_path_gemini: String,
    pub tls: bool,
    pub auth_required: bool,
    pub auth_methods: Vec<String>,
    pub instance_id: String,
    pub node_role: String,
    pub advertise_management: bool,
}

impl Default for TxtOptions {
    fn default() -> Self {
        Self {
            version: "1".into(),
            product: PRODUCT_CPA.into(),
            protocols: vec![
                "chat-completions".into(),
                "responses".into(),
                "messages".into(),
                "generate-content".into(),
                "interactions".into(),
            ],
            features: vec![
                "chat".into(),
                "responses".into(),
                "messages".into(),
                "generate_content".into(),
                "interactions".into(),
            ],
            api_path_openai: "/v1".into(),
            api_path_anthropic: "/v1".into(),
            api_path_gemini: "/v1beta".into(),
            tls: false,
            auth_required: true,
            auth_methods: vec!["api_key".into()],
            instance_id: "".into(),
            node_role: "standalone".into(),
            advertise_management: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub index: u32,
    pub name: String,
    pub is_up: bool,
    pub is_loopback: bool,
    pub is_multicast: bool,
    pub is_point_to_point: bool,
    pub addresses: Vec<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub instance_name: String,
    pub service_type: String,
    pub domain: String,
    pub port: u16,
    pub subtypes: Vec<String>,
    pub text_records: Vec<String>,
    pub interfaces: Vec<NetworkInterface>,
    pub advertised_ips: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredService {
    pub instance_name: String,
    pub service_type: String,
    pub domain: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub ipv4: Vec<Ipv4Addr>,
    #[serde(default)]
    pub ipv6: Vec<Ipv6Addr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub product: String,
    #[serde(default)]
    pub auth_required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_methods: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub endpoints: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node_role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub raw_txt: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, rename = "service-name")]
    pub service_name: String,
    #[serde(default, rename = "service-type")]
    pub service_type: String,
    #[serde(default)]
    pub subtypes: Vec<String>,
    #[serde(default)]
    pub interfaces: DiscoveryInterfacesConfig,
    #[serde(default, rename = "auth-required")]
    pub auth_required: Option<bool>,
    #[serde(default, rename = "advertise-management")]
    pub advertise_management: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryInterfacesConfig {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

pub fn sanitize_txt_value(v: &str) -> String {
    v.chars()
        .filter(|&c| (c as u32) >= 32 && (c as u32) < 127)
        .collect()
}

pub fn is_valid_hex4(s: &str) -> bool {
    s.len() == 4 && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn sanitize_instance_name(name: &str) -> String {
    let filtered: String = name
        .chars()
        .filter(|&c| (c as u32) >= 32 && (c as u32) != 127)
        .collect();
    let trimmed = filtered.trim();
    truncate_runes_to(trimmed, 63)
}

pub fn truncate_runes_to(s: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let mut end = s.len();
    while end > max_bytes {
        if let Some((idx, _)) = s[..end].char_indices().last() {
            end = idx;
        } else {
            break;
        }
    }
    s[..end].to_string()
}

pub fn validate_service_type(st: &str) -> Result<(), String> {
    let st = st.trim();
    if st.is_empty() {
        return Err("service type cannot be empty".into());
    }
    if st.len() > 63 {
        return Err(format!("service type \"{}\" exceeds 63 characters", st));
    }
    if !st.ends_with("._tcp") {
        return Err(format!("service type \"{}\" must end with ._tcp", st));
    }
    let prefix = &st[..st.len() - 5];
    if !prefix.starts_with('_') {
        return Err(format!(
            "service type \"{}\" must start with an underscore",
            st
        ));
    }
    let name = &prefix[1..];
    if name.is_empty() || name.len() > 15 {
        return Err(format!(
            "service name \"{}\" must be between 1 and 15 characters (RFC 6335)",
            name
        ));
    }
    for (i, c) in name.char_indices() {
        let valid_char = c.is_ascii_alphanumeric() || c == '-';
        if !valid_char {
            return Err(format!(
                "service type contains invalid character '{}' (RFC 6335)",
                c
            ));
        }
        if (i == 0 || i == name.len() - 1) && c == '-' {
            return Err("service name cannot start or end with a hyphen".into());
        }
    }
    Ok(())
}

pub fn sanitize_subtype(sub: &str) -> String {
    let sub = sub.trim();
    if sub.is_empty() || sub.contains("._sub") || sub.contains('.') {
        return String::new();
    }
    let label = if let Some(stripped) = sub.strip_prefix('_') {
        stripped
    } else {
        sub
    };
    if label.is_empty() || label.len() > 62 {
        return String::new();
    }
    if label.starts_with('-') || label.ends_with('-') {
        return String::new();
    }
    for c in label.chars() {
        if !c.is_ascii_alphanumeric() && c != '-' {
            return String::new();
        }
    }
    format!("_{}", label)
}

pub fn format_instance_name(custom_name: &str, instance_id: &str) -> String {
    let instance_id = if !is_valid_hex4(instance_id) {
        "0001"
    } else {
        instance_id
    }
    .to_uppercase();

    let suffix = format!("-{}", instance_id);
    let mut base = sanitize_instance_name(custom_name);
    let default_full = format!("{}{}", DEFAULT_INSTANCE_PREFIX, instance_id);

    if base.is_empty() || base.eq_ignore_ascii_case(&default_full) {
        return default_full;
    }
    if base.len() >= suffix.len() && base[base.len() - suffix.len()..].eq_ignore_ascii_case(&suffix)
    {
        base = base[..base.len() - suffix.len()].trim().to_string();
    }
    let max_base_bytes = 63usize.saturating_sub(suffix.len());
    base = truncate_runes_to(base.trim(), max_base_bytes)
        .trim()
        .to_string();
    if base.is_empty() {
        return default_full;
    }
    format!("{}{}", base, suffix)
}

lazy_static::lazy_static! {
    static ref CACHED_IDS: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
}

pub fn reset_cached_instance_id() {
    let mut map = CACHED_IDS.lock().unwrap();
    map.clear();
}

pub fn get_or_generate_instance_id(state_dir: Option<&Path>) -> String {
    let key = state_dir
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    {
        let map = CACHED_IDS.lock().unwrap();
        if let Some(id) = map.get(&key) {
            if is_valid_hex4(id) {
                return id.clone();
            }
        }
    }

    if let Some(dir) = state_dir {
        let id_file = dir.join("instance_id");
        if let Ok(content) = std::fs::read_to_string(&id_file) {
            let id = content.trim();
            if is_valid_hex4(id) {
                let upper = id.to_uppercase();
                let mut map = CACHED_IDS.lock().unwrap();
                map.insert(key, upper.clone());
                return upper;
            }
        }
    }

    let mut buf = [0u8; 2];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut buf);
    let id = format!("{:02X}{:02X}", buf[0], buf[1]);

    if let Some(dir) = state_dir {
        let _ = std::fs::create_dir_all(dir);
        let id_file = dir.join("instance_id");
        let tmp_file = dir.join(format!("instance_id_{}.tmp", std::process::id()));
        if std::fs::write(&tmp_file, &id).is_ok() {
            let _ = std::fs::rename(&tmp_file, &id_file);
        }
    }

    let mut map = CACHED_IDS.lock().unwrap();
    map.insert(key, id.clone());
    id
}

pub fn build_txt_records(opts: &TxtOptions) -> Vec<String> {
    let version = if opts.version.is_empty() {
        "1"
    } else {
        &opts.version
    };
    let product = if opts.product.is_empty() {
        PRODUCT_CPA
    } else {
        &opts.product
    };
    let api_openai = if opts.api_path_openai.is_empty() {
        "/v1"
    } else {
        &opts.api_path_openai
    };
    let api_anthropic = if opts.api_path_anthropic.is_empty() {
        "/v1"
    } else {
        &opts.api_path_anthropic
    };
    let api_gemini = if opts.api_path_gemini.is_empty() {
        "/v1beta"
    } else {
        &opts.api_path_gemini
    };

    struct Kv {
        key: String,
        val: String,
        _priority: usize,
    }

    let mut candidates = Vec::new();
    let mut add = |k: &str, v: &str, prio: usize| {
        let clean = sanitize_txt_value(v.trim());
        if !clean.is_empty() {
            candidates.push(Kv {
                key: k.to_string(),
                val: clean,
                _priority: prio,
            });
        }
    };

    add("version", version, 1);
    add("product", product, 1);
    add("instance_id", &opts.instance_id, 1);
    add("tls", if opts.tls { "1" } else { "0" }, 1);
    add(
        "auth_required",
        if opts.auth_required { "true" } else { "false" },
        1,
    );
    add("api_openai", api_openai, 1);

    add("api_anthropic", api_anthropic, 2);
    add("api_gemini", api_gemini, 2);
    add(
        "management",
        if opts.advertise_management {
            "true"
        } else {
            "false"
        },
        2,
    );
    if !opts.auth_methods.is_empty() {
        add("auth_methods", &opts.auth_methods.join(","), 2);
    }
    if !opts.protocols.is_empty() {
        add("protocols", &opts.protocols.join(","), 2);
    }

    if !opts.node_role.is_empty() {
        add("node_role", &opts.node_role, 3);
    }
    if !opts.features.is_empty() {
        add("features", &opts.features.join(","), 3);
    }

    let mut records = Vec::new();
    let mut total_bytes = 0;

    for item in candidates {
        let entry = format!("{}={}", item.key, item.val);
        if entry.len() > MAX_TXT_RECORD_BYTES {
            continue;
        }
        let entry_cost = entry.len() + 1;
        if total_bytes + entry_cost > MAX_TXT_BYTES {
            continue;
        }
        records.push(entry);
        total_bytes += entry_cost;
    }

    records
}

pub fn parse_txt_records(records: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for entry in records {
        if let Some((k, v)) = entry.split_once('=') {
            let key = k.trim().to_lowercase();
            if !key.is_empty() {
                map.insert(key, v.to_string());
            }
        } else {
            let key = entry.trim().to_lowercase();
            if !key.is_empty() {
                map.insert(key, String::new());
            }
        }
    }
    map
}

pub fn filter_interfaces(
    available: &[NetworkInterface],
    include: &[String],
    exclude: &[String],
) -> Vec<NetworkInterface> {
    let matches_any = |name: &str, patterns: &[String]| -> bool {
        for p in patterns {
            let pat = p.trim().to_lowercase();
            if pat.is_empty() {
                continue;
            }
            if let Some(prefix) = pat.strip_suffix('*') {
                if name.starts_with(prefix) {
                    return true;
                }
            } else if name == pat {
                return true;
            }
        }
        false
    };

    let is_virtual_or_tunnel = |name: &str| -> bool {
        IGNORED_INTERFACE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    };

    let is_likely_physical_lan = |name: &str| -> bool {
        LIKELY_PHYSICAL_LAN_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    };

    let allow_loopback = std::env::var("MAHOQUOT_QA_LOOPBACK_ONLY").is_ok()
        || include
            .iter()
            .any(|i| i == "lo" || i == "lo0" || i == "localhost" || i == "loopback");

    let mut qualified = Vec::new();

    for iface in available {
        if !iface.is_up || iface.is_point_to_point {
            continue;
        }

        if iface.is_loopback && !allow_loopback {
            continue;
        }

        if !iface.is_multicast && !iface.is_loopback {
            continue;
        }

        let name = iface.name.to_lowercase();

        if !exclude.is_empty() && matches_any(&name, exclude) {
            continue;
        }

        if include.is_empty()
            && (is_virtual_or_tunnel(&name)
                || (!is_likely_physical_lan(&name) && !iface.is_loopback))
        {
            continue;
        }

        if !include.is_empty() && !matches_any(&name, include) {
            continue;
        }

        let has_valid_ip = if iface.is_loopback {
            iface.addresses.iter().any(|ip| ip.is_loopback())
        } else {
            iface
                .addresses
                .iter()
                .any(|ip| !ip.is_loopback() && !ip.is_unspecified())
        };
        if has_valid_ip {
            qualified.push(iface.clone());
        }
    }

    qualified
}

pub fn extract_interface_ips(ifaces: &[NetworkInterface]) -> Vec<String> {
    let mut ips = Vec::new();
    for iface in ifaces {
        for ip in &iface.addresses {
            if !ip.is_loopback() && !ip.is_unspecified() {
                ips.push(ip.to_string());
            }
        }
    }
    ips
}

pub fn build_service_spec(
    cfg: &DiscoveryConfig,
    bind_host: Option<&str>,
    port: u16,
    tls_enabled: bool,
    available_interfaces: &[NetworkInterface],
    state_dir: Option<&Path>,
) -> Result<ServiceSpec, String> {
    if port == 0 {
        return Err("discovery: invalid service port 0 (must be between 1 and 65535)".into());
    }

    let instance_id = get_or_generate_instance_id(state_dir);
    let instance_name = format_instance_name(&cfg.service_name, &instance_id);

    let service_type = if cfg.service_type.trim().is_empty() {
        DEFAULT_SERVICE_TYPE.to_string()
    } else {
        validate_service_type(&cfg.service_type)?;
        cfg.service_type.trim().to_string()
    };

    let mut subtypes = Vec::new();
    let raw_subtypes = if cfg.subtypes.is_empty() {
        vec![
            SUBTYPE_CHAT_COMPLETIONS.to_string(),
            SUBTYPE_RESPONSES.to_string(),
            SUBTYPE_MESSAGES.to_string(),
            SUBTYPE_GENERATE_CONTENT.to_string(),
            SUBTYPE_INTERACTIONS.to_string(),
        ]
    } else {
        cfg.subtypes.clone()
    };

    for raw in raw_subtypes {
        let clean = sanitize_subtype(&raw);
        if !clean.is_empty() {
            subtypes.push(clean);
        }
    }
    if subtypes.is_empty() {
        subtypes.push(SUBTYPE_CHAT_COMPLETIONS.to_string());
    }

    let bind_host_str = bind_host.unwrap_or("").trim();
    let bind_ip = if !bind_host_str.is_empty() {
        let ip: IpAddr = bind_host_str.parse().map_err(|_| {
            format!(
                "discovery: refusing LAN advertising for non-IP bind host \"{}\"",
                bind_host_str
            )
        })?;
        let allow_loopback = std::env::var("MAHOQUOT_QA_LOOPBACK_ONLY").is_ok()
            || bind_host_str == "127.0.0.1"
            || bind_host_str == "localhost"
            || bind_host_str == "::1";
        if ip.is_loopback() && !allow_loopback {
            return Err(format!(
                "discovery: LAN advertising is unavailable for loopback bind host \"{}\"",
                bind_host_str
            ));
        }
        Some(ip)
    } else {
        None
    };

    let filtered_ifaces = filter_interfaces(
        available_interfaces,
        &cfg.interfaces.include,
        &cfg.interfaces.exclude,
    );

    let filtered_ifaces = if let Some(target_ip) = bind_ip {
        if target_ip.is_loopback()
            && filtered_ifaces
                .iter()
                .all(|i| !i.addresses.contains(&target_ip))
        {
            let mut ifaces = filtered_ifaces;
            for iface in available_interfaces {
                if iface.is_loopback && iface.addresses.contains(&target_ip) {
                    ifaces.push(iface.clone());
                }
            }
            ifaces
        } else {
            filtered_ifaces
        }
    } else {
        filtered_ifaces
    };

    let selected_ifaces: Vec<NetworkInterface> = if let Some(target_ip) = bind_ip {
        if !target_ip.is_unspecified() {
            filtered_ifaces
                .into_iter()
                .filter(|iface| iface.addresses.contains(&target_ip))
                .collect()
        } else {
            filtered_ifaces
        }
    } else {
        filtered_ifaces
    };

    if selected_ifaces.is_empty() {
        return Err(format!(
            "discovery: no interface owns bind host \"{}\"",
            bind_host_str
        ));
    }

    let mut advertised_ips = extract_interface_ips(&selected_ifaces);
    if let Some(target_ip) = bind_ip {
        if target_ip.is_loopback() {
            advertised_ips = vec![target_ip.to_string()];
        } else if !target_ip.is_unspecified() {
            let target_str = target_ip.to_string();
            advertised_ips.retain(|ip| ip == &target_str);
        }
    }

    if advertised_ips.is_empty() {
        return Err(format!(
            "discovery: no advertised address matches bind host \"{}\"",
            bind_host_str
        ));
    }

    let txt_opts = TxtOptions {
        instance_id,
        tls: tls_enabled,
        advertise_management: cfg.advertise_management,
        auth_required: cfg.auth_required.unwrap_or(true),
        ..Default::default()
    };

    let text_records = build_txt_records(&txt_opts);

    Ok(ServiceSpec {
        instance_name,
        service_type,
        domain: DEFAULT_DOMAIN.into(),
        port,
        subtypes,
        text_records,
        interfaces: selected_ifaces,
        advertised_ips,
    })
}

pub trait Advertiser: Send + Sync {
    fn start(&mut self, spec: ServiceSpec) -> Result<(), String>;
    fn stop(&mut self) -> Result<(), String>;
}

pub trait Browser: Send + Sync {
    fn browse(&self, service_type: &str, domain: &str) -> Result<Vec<DiscoveredService>, String>;
}

#[derive(Default, Clone)]
pub struct MockMdnsRegistry {
    services: Arc<Mutex<HashMap<String, DiscoveredService>>>,
}

impl MockMdnsRegistry {
    pub fn new() -> Self {
        Self {
            services: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, svc: DiscoveredService) {
        let key = format!("{}.{}.{}", svc.instance_name, svc.service_type, svc.domain);
        let mut map = self.services.lock().unwrap();
        map.insert(key, svc);
    }

    pub fn unregister(&self, instance_name: &str, service_type: &str, domain: &str) {
        let key = format!("{}.{}.{}", instance_name, service_type, domain);
        let mut map = self.services.lock().unwrap();
        map.remove(&key);
    }

    pub fn list(&self, service_type: &str) -> Vec<DiscoveredService> {
        let map = self.services.lock().unwrap();
        let mut res = Vec::new();
        for svc in map.values() {
            if svc.service_type == service_type {
                res.push(svc.clone());
            }
        }
        res.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));
        res
    }
}

pub struct MockAdvertiser {
    registry: MockMdnsRegistry,
    active_spec: Option<ServiceSpec>,
}

impl MockAdvertiser {
    pub fn new(registry: MockMdnsRegistry) -> Self {
        Self {
            registry,
            active_spec: None,
        }
    }
}

impl Advertiser for MockAdvertiser {
    fn start(&mut self, spec: ServiceSpec) -> Result<(), String> {
        let parsed_txt = parse_txt_records(&spec.text_records);
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        for ip_str in &spec.advertised_ips {
            if let Ok(addr) = ip_str.parse::<IpAddr>() {
                match addr {
                    IpAddr::V4(v4) => ipv4.push(v4),
                    IpAddr::V6(v6) => ipv6.push(v6),
                }
            }
        }

        let mut endpoints = HashMap::new();
        if let Some(v) = parsed_txt.get("api_openai") {
            endpoints.insert("openai".into(), v.clone());
        }
        if let Some(v) = parsed_txt.get("api_anthropic") {
            endpoints.insert("anthropic".into(), v.clone());
        }
        if let Some(v) = parsed_txt.get("api_gemini") {
            endpoints.insert("gemini".into(), v.clone());
        }

        let svc = DiscoveredService {
            instance_name: spec.instance_name.clone(),
            service_type: spec.service_type.clone(),
            domain: spec.domain.clone(),
            host: "localhost".into(),
            port: spec.port,
            ipv4,
            ipv6,
            protocols: parsed_txt
                .get("protocols")
                .map(|p| p.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            features: parsed_txt
                .get("features")
                .map(|f| f.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            product: parsed_txt.get("product").cloned().unwrap_or_default(),
            auth_required: parsed_txt
                .get("auth_required")
                .map(|v| v == "true")
                .unwrap_or(true),
            auth_methods: parsed_txt
                .get("auth_methods")
                .map(|m| m.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            endpoints,
            node_role: parsed_txt.get("node_role").cloned().unwrap_or_default(),
            version: parsed_txt.get("version").cloned().unwrap_or_default(),
            raw_txt: parsed_txt,
        };

        self.registry.register(svc);
        self.active_spec = Some(spec);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        if let Some(spec) = self.active_spec.take() {
            self.registry
                .unregister(&spec.instance_name, &spec.service_type, &spec.domain);
        }
        Ok(())
    }
}

pub struct MockBrowser {
    registry: MockMdnsRegistry,
}

impl MockBrowser {
    pub fn new(registry: MockMdnsRegistry) -> Self {
        Self { registry }
    }
}

impl Browser for MockBrowser {
    fn browse(&self, service_type: &str, _domain: &str) -> Result<Vec<DiscoveredService>, String> {
        Ok(self.registry.list(service_type))
    }
}

type InterfaceRawEntry = (u32, bool, bool, bool, bool, Vec<IpAddr>);

pub fn query_system_interfaces() -> Vec<NetworkInterface> {
    let mut interfaces_map: HashMap<String, InterfaceRawEntry> = HashMap::new();

    #[cfg(unix)]
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) == 0 && !ifap.is_null() {
            let mut curr = ifap;
            while !curr.is_null() {
                let ifa = &*curr;
                if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                    let c_str = std::ffi::CStr::from_ptr(ifa.ifa_name);
                    if let Ok(name) = c_str.to_str() {
                        let name_str = name.to_string();
                        let flags = ifa.ifa_flags as i32;
                        let is_up = (flags & libc::IFF_UP) != 0;
                        let is_loopback = (flags & libc::IFF_LOOPBACK) != 0;
                        let is_multicast = (flags & libc::IFF_MULTICAST) != 0;
                        let is_point_to_point = (flags & libc::IFF_POINTOPOINT) != 0;

                        let sa_family = (*ifa.ifa_addr).sa_family as i32;
                        let ip_opt = if sa_family == libc::AF_INET {
                            let sa_in = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                            let addr_bytes = sa_in.sin_addr.s_addr.to_ne_bytes();
                            Some(IpAddr::V4(Ipv4Addr::from(addr_bytes)))
                        } else if sa_family == libc::AF_INET6 {
                            let sa_in6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                            let addr_bytes = sa_in6.sin6_addr.s6_addr;
                            Some(IpAddr::V6(Ipv6Addr::from(addr_bytes)))
                        } else {
                            None
                        };

                        let entry = interfaces_map.entry(name_str).or_insert((
                            0,
                            is_up,
                            is_loopback,
                            is_multicast,
                            is_point_to_point,
                            Vec::new(),
                        ));
                        if let Some(ip) = ip_opt {
                            if !entry.5.contains(&ip) {
                                entry.5.push(ip);
                            }
                        }
                    }
                }
                curr = (*curr).ifa_next;
            }
            libc::freeifaddrs(ifap);
        }
    }

    let mut result = Vec::new();
    for (index, (name, (_, is_up, is_loopback, is_multicast, is_point_to_point, addresses))) in
        (1..).zip(interfaces_map)
    {
        result.push(NetworkInterface {
            index,
            name,
            is_up,
            is_loopback,
            is_multicast,
            is_point_to_point,
            addresses,
        });
    }

    if result.is_empty() {
        result.push(NetworkInterface {
            index: 1,
            name: "lo0".into(),
            is_up: true,
            is_loopback: true,
            is_multicast: true,
            is_point_to_point: false,
            addresses: vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))],
        });
    }

    result
}

fn shared_discovery_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MAHOQUOT_DISCOVERY_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::temp_dir().join("mahoquot_mdns_discovery")
}

// ==================== mDNS Packet Codec and Sockets ====================

pub fn encode_dns_name(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for part in name.split('.').filter(|s| !s.is_empty()) {
        let b = part.as_bytes();
        let len = b.len().min(63);
        out.push(len as u8);
        out.extend_from_slice(&b[..len]);
    }
    out.push(0);
    out
}

pub fn decode_dns_name(buf: &[u8], offset: usize) -> (String, usize) {
    let mut labels: Vec<String> = Vec::new();
    let mut curr = offset;
    let mut jumped = false;
    let mut next_offset = None;
    let mut steps = 0;

    while curr < buf.len() && steps < 128 {
        steps += 1;
        let len = buf[curr] as usize;
        if len == 0 {
            if !jumped {
                next_offset = Some(curr + 1);
            }
            break;
        }
        if (len & 0xc0) == 0xc0 {
            if curr + 1 >= buf.len() {
                break;
            }
            let ptr = ((len & 0x3f) << 8) | (buf[curr + 1] as usize);
            if !jumped {
                next_offset = Some(curr + 2);
            }
            jumped = true;
            curr = ptr;
            continue;
        }
        curr += 1;
        if curr + len > buf.len() {
            break;
        }
        let label = String::from_utf8_lossy(&buf[curr..curr + len]).to_string();
        labels.push(label);
        curr += len;
    }

    let end_offset = next_offset.unwrap_or(curr);
    (labels.join("."), end_offset)
}

#[derive(Debug, Clone)]
pub struct DnsQuestion {
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
}

#[derive(Debug, Clone)]
pub struct DnsRecord {
    pub name: String,
    pub rtype: u16,
    pub rclass: u16,
    pub ttl: u32,
    pub rdata: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct DnsMessage {
    pub id: u16,
    pub flags: u16,
    pub questions: Vec<DnsQuestion>,
    pub answers: Vec<DnsRecord>,
}

pub fn parse_dns_packet(buf: &[u8]) -> Option<DnsMessage> {
    if buf.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([buf[0], buf[1]]);
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let nscount = u16::from_be_bytes([buf[8], buf[9]]) as usize;
    let arcount = u16::from_be_bytes([buf[10], buf[11]]) as usize;

    let mut offset = 12;
    let mut questions = Vec::with_capacity(qdcount);
    for _ in 0..qdcount {
        if offset >= buf.len() {
            break;
        }
        let (name, next) = decode_dns_name(buf, offset);
        offset = next;
        if offset + 4 > buf.len() {
            break;
        }
        let qtype = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let qclass = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]);
        offset += 4;
        questions.push(DnsQuestion {
            name,
            qtype,
            qclass,
        });
    }

    let total_records = ancount + nscount + arcount;
    let mut answers = Vec::with_capacity(total_records);
    for _ in 0..total_records {
        if offset >= buf.len() {
            break;
        }
        let (name, next) = decode_dns_name(buf, offset);
        offset = next;
        if offset + 10 > buf.len() {
            break;
        }
        let rtype = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let rclass = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]);
        let ttl = u32::from_be_bytes([
            buf[offset + 4],
            buf[offset + 5],
            buf[offset + 6],
            buf[offset + 7],
        ]);
        let rdlen = u16::from_be_bytes([buf[offset + 8], buf[offset + 9]]) as usize;
        offset += 10;
        if offset + rdlen > buf.len() {
            break;
        }
        let rdata = buf[offset..offset + rdlen].to_vec();
        offset += rdlen;
        answers.push(DnsRecord {
            name,
            rtype,
            rclass,
            ttl,
            rdata,
        });
    }

    Some(DnsMessage {
        id,
        flags,
        questions,
        answers,
    })
}

pub fn build_mdns_query(service_type: &str, domain: &str) -> Vec<u8> {
    let s_type = service_type.trim_matches('.');
    let dom = domain.trim_matches('.');
    let qname = format!("{}.{}", s_type, dom);

    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&[0, 0]); // ID = 0
    buf.extend_from_slice(&[0, 0]); // Flags = standard query
    buf.extend_from_slice(&[0, 1]); // QDCOUNT = 1
    buf.extend_from_slice(&[0, 0]); // ANCOUNT = 0
    buf.extend_from_slice(&[0, 0]); // NSCOUNT = 0
    buf.extend_from_slice(&[0, 0]); // ARCOUNT = 0

    buf.extend_from_slice(&encode_dns_name(&qname));
    buf.extend_from_slice(&12u16.to_be_bytes()); // QTYPE = PTR (12)
    buf.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN (1)
    buf
}

#[allow(clippy::too_many_arguments)]
pub fn build_mdns_response(
    instance_name: &str,
    service_type: &str,
    domain: &str,
    port: u16,
    ipv4_addrs: &[Ipv4Addr],
    ipv6_addrs: &[Ipv6Addr],
    text_records: &[String],
    ttl: u32,
) -> Vec<u8> {
    let s_type = service_type.trim_matches('.');
    let dom = domain.trim_matches('.');
    let service_fqdn = format!("{}.{}", s_type, dom);
    let instance_fqdn = format!("{}.{}", instance_name, service_fqdn);
    let host_fqdn = format!("{}.{}", instance_name, dom);

    let mut answers: Vec<Vec<u8>> = Vec::new();

    // 1. PTR record: service_fqdn -> instance_fqdn
    {
        let mut rec = Vec::new();
        rec.extend_from_slice(&encode_dns_name(&service_fqdn));
        rec.extend_from_slice(&12u16.to_be_bytes()); // PTR
        rec.extend_from_slice(&1u16.to_be_bytes()); // IN
        rec.extend_from_slice(&ttl.to_be_bytes());
        let rdata = encode_dns_name(&instance_fqdn);
        rec.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        rec.extend_from_slice(&rdata);
        answers.push(rec);
    }

    // 2. SRV record: instance_fqdn -> priority, weight, port, host_fqdn
    {
        let mut rec = Vec::new();
        rec.extend_from_slice(&encode_dns_name(&instance_fqdn));
        rec.extend_from_slice(&33u16.to_be_bytes()); // SRV
        rec.extend_from_slice(&(1u16 | 0x8000).to_be_bytes()); // IN | flush bit
        rec.extend_from_slice(&ttl.to_be_bytes());
        let mut rdata = Vec::new();
        rdata.extend_from_slice(&0u16.to_be_bytes()); // priority
        rdata.extend_from_slice(&0u16.to_be_bytes()); // weight
        rdata.extend_from_slice(&port.to_be_bytes()); // port
        rdata.extend_from_slice(&encode_dns_name(&host_fqdn));
        rec.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        rec.extend_from_slice(&rdata);
        answers.push(rec);
    }

    // 3. TXT record: instance_fqdn -> string list
    {
        let mut rec = Vec::new();
        rec.extend_from_slice(&encode_dns_name(&instance_fqdn));
        rec.extend_from_slice(&16u16.to_be_bytes()); // TXT
        rec.extend_from_slice(&(1u16 | 0x8000).to_be_bytes()); // IN | flush bit
        rec.extend_from_slice(&ttl.to_be_bytes());
        let mut rdata = Vec::new();
        for t in text_records {
            let b = t.as_bytes();
            let len = b.len().min(255) as u8;
            rdata.push(len);
            rdata.extend_from_slice(&b[..len as usize]);
        }
        if rdata.is_empty() {
            rdata.push(0);
        }
        rec.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        rec.extend_from_slice(&rdata);
        answers.push(rec);
    }

    // 4. A records: host_fqdn -> IPv4
    for ip in ipv4_addrs {
        let mut rec = Vec::new();
        rec.extend_from_slice(&encode_dns_name(&host_fqdn));
        rec.extend_from_slice(&1u16.to_be_bytes()); // A
        rec.extend_from_slice(&(1u16 | 0x8000).to_be_bytes()); // IN | flush bit
        rec.extend_from_slice(&ttl.to_be_bytes());
        rec.extend_from_slice(&4u16.to_be_bytes());
        rec.extend_from_slice(&ip.octets());
        answers.push(rec);
    }

    // 5. AAAA records: host_fqdn -> IPv6
    for ip in ipv6_addrs {
        let mut rec = Vec::new();
        rec.extend_from_slice(&encode_dns_name(&host_fqdn));
        rec.extend_from_slice(&28u16.to_be_bytes()); // AAAA
        rec.extend_from_slice(&(1u16 | 0x8000).to_be_bytes()); // IN | flush bit
        rec.extend_from_slice(&ttl.to_be_bytes());
        rec.extend_from_slice(&16u16.to_be_bytes());
        rec.extend_from_slice(&ip.octets());
        answers.push(rec);
    }

    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(&0u16.to_be_bytes()); // ID = 0
    buf.extend_from_slice(&0x8400u16.to_be_bytes()); // flags: response, authoritative
    buf.extend_from_slice(&0u16.to_be_bytes()); // QDCOUNT = 0
    buf.extend_from_slice(&(answers.len() as u16).to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT = 0
    buf.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT = 0
    for a in answers {
        buf.extend_from_slice(&a);
    }
    buf
}

pub fn create_multicast_listen_socket() -> std::io::Result<UdpSocket> {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let optval: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );
            #[cfg(target_os = "macos")]
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );

            let mut sin: libc::sockaddr_in = std::mem::zeroed();
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = MDNS_PORT.to_be();
            sin.sin_addr.s_addr = libc::INADDR_ANY.to_be();

            if libc::bind(
                fd,
                &sin as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&sin) as libc::socklen_t,
            ) != 0
            {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
        }

        let sock = unsafe { UdpSocket::from_raw_fd(fd) };
        sock.set_nonblocking(true)?;
        sock.join_multicast_v4(&MDNS_IPV4_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;
        let _ = sock.set_multicast_loop_v4(true);
        let _ = sock.set_multicast_ttl_v4(255);
        Ok(sock)
    }

    #[cfg(not(unix))]
    {
        let sock = UdpSocket::bind(("0.0.0.0", MDNS_PORT))?;
        sock.set_nonblocking(true)?;
        sock.join_multicast_v4(&MDNS_IPV4_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;
        let _ = sock.set_multicast_loop_v4(true);
        let _ = sock.set_multicast_ttl_v4(255);
        Ok(sock)
    }
}

fn set_multicast_if_v4(sock: &UdpSocket, addr: Ipv4Addr) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let in_addr = libc::in_addr {
            s_addr: u32::from_ne_bytes(addr.octets()),
        };
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_IF,
                &in_addr as *const _ as *const libc::c_void,
                std::mem::size_of_val(&in_addr) as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (sock, addr);
        Ok(())
    }
}

pub fn multicast_ipv4_interface_addrs(interfaces: &[NetworkInterface]) -> Vec<Ipv4Addr> {
    let mut addrs = Vec::new();
    for iface in interfaces {
        if !iface.is_up || iface.is_point_to_point || !(iface.is_multicast || iface.is_loopback) {
            continue;
        }
        for ip in &iface.addresses {
            if let IpAddr::V4(v4) = ip {
                if !v4.is_unspecified() && !addrs.contains(v4) {
                    addrs.push(*v4);
                }
            }
        }
    }
    addrs
}

pub fn join_and_send_multicast_v4(sock: &UdpSocket, payload: &[u8], interfaces: &[Ipv4Addr]) {
    let dest = SocketAddr::new(IpAddr::V4(MDNS_IPV4_MULTICAST), MDNS_PORT);
    if interfaces.is_empty() {
        let _ = sock.send_to(payload, dest);
        return;
    }
    for addr in interfaces {
        if set_multicast_if_v4(sock, *addr).is_ok() {
            let _ = sock.send_to(payload, dest);
        }
    }
}

pub fn create_multicast_query_socket() -> std::io::Result<UdpSocket> {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let optval: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );
            #[cfg(target_os = "macos")]
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );

            let mut sin: libc::sockaddr_in = std::mem::zeroed();
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = 0; // ephemeral port for query
            sin.sin_addr.s_addr = libc::INADDR_ANY.to_be();

            if libc::bind(
                fd,
                &sin as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&sin) as libc::socklen_t,
            ) != 0
            {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
        }

        let sock = unsafe { UdpSocket::from_raw_fd(fd) };
        sock.set_nonblocking(true)?;
        sock.join_multicast_v4(&MDNS_IPV4_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;
        let _ = sock.set_multicast_loop_v4(true);
        let _ = sock.set_multicast_ttl_v4(255);
        Ok(sock)
    }

    #[cfg(not(unix))]
    {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_nonblocking(true)?;
        sock.join_multicast_v4(&MDNS_IPV4_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;
        let _ = sock.set_multicast_loop_v4(true);
        let _ = sock.set_multicast_ttl_v4(255);
        Ok(sock)
    }
}

pub struct RealAdvertiser {
    active_spec: Option<ServiceSpec>,
    active_file: Option<PathBuf>,
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl RealAdvertiser {
    pub fn new() -> Self {
        Self {
            active_spec: None,
            active_file: None,
            stop_tx: None,
        }
    }
}

impl Default for RealAdvertiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Advertiser for RealAdvertiser {
    fn start(&mut self, spec: ServiceSpec) -> Result<(), String> {
        let parsed_txt = parse_txt_records(&spec.text_records);
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        for ip_str in &spec.advertised_ips {
            if let Ok(addr) = ip_str.parse::<IpAddr>() {
                match addr {
                    IpAddr::V4(v4) => ipv4.push(v4),
                    IpAddr::V6(v6) => ipv6.push(v6),
                }
            }
        }

        let mut endpoints = HashMap::new();
        if let Some(v) = parsed_txt.get("api_openai") {
            endpoints.insert("openai".into(), v.clone());
        }
        if let Some(v) = parsed_txt.get("api_anthropic") {
            endpoints.insert("anthropic".into(), v.clone());
        }
        if let Some(v) = parsed_txt.get("api_gemini") {
            endpoints.insert("gemini".into(), v.clone());
        }

        let host = if !ipv4.is_empty() {
            ipv4[0].to_string()
        } else {
            "127.0.0.1".to_string()
        };

        let svc = DiscoveredService {
            instance_name: spec.instance_name.clone(),
            service_type: spec.service_type.clone(),
            domain: spec.domain.clone(),
            host,
            port: spec.port,
            ipv4: ipv4.clone(),
            ipv6: ipv6.clone(),
            protocols: parsed_txt
                .get("protocols")
                .map(|p| p.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            features: parsed_txt
                .get("features")
                .map(|f| f.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            product: parsed_txt
                .get("product")
                .cloned()
                .unwrap_or_else(|| PRODUCT_CPA.into()),
            auth_required: parsed_txt
                .get("auth_required")
                .map(|v| v == "true")
                .unwrap_or(true),
            auth_methods: parsed_txt
                .get("auth_methods")
                .map(|m| m.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default(),
            endpoints,
            node_role: parsed_txt
                .get("node_role")
                .cloned()
                .unwrap_or_else(|| "standalone".into()),
            version: parsed_txt
                .get("version")
                .cloned()
                .unwrap_or_else(|| "1".into()),
            raw_txt: parsed_txt,
        };

        // File discovery is only an explicit test fallback. Production discovery
        // must work across hosts and therefore uses multicast DNS-SD only.
        let file_path = if std::env::var("MAHOQUOT_DISCOVERY_FILE_FALLBACK").as_deref() == Ok("1") {
            let dir = shared_discovery_dir();
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create discovery dir: {e}"))?;
            let file_path = dir.join(format!("{}.json", spec.instance_name));
            let serialized = serde_json::to_string_pretty(&svc)
                .map_err(|e| format!("cannot serialize discovery record: {e}"))?;
            std::fs::write(&file_path, serialized).map_err(|e| {
                format!(
                    "cannot write discovery advertisement {}: {e}",
                    file_path.display()
                )
            })?;
            Some(file_path)
        } else {
            None
        };

        // Start real mDNS multicast responder loop
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let instance_name = spec.instance_name.clone();
        let service_type = spec.service_type.clone();
        let domain = spec.domain.clone();
        let port = spec.port;
        let text_records = spec.text_records.clone();

        std::thread::spawn(move || {
            let socket = match create_multicast_listen_socket() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("mDNS responder could not bind multicast socket: {e}");
                    return;
                }
            };

            let mut responder_ifaces = ipv4.clone();
            if !responder_ifaces.contains(&Ipv4Addr::LOCALHOST) {
                responder_ifaces.push(Ipv4Addr::LOCALHOST);
            }
            for addr in &responder_ifaces {
                let _ = socket.join_multicast_v4(&MDNS_IPV4_MULTICAST, addr);
            }

            // Send initial unsolicited announcement (TTL 120)
            let announcement = build_mdns_response(
                &instance_name,
                &service_type,
                &domain,
                port,
                &ipv4,
                &ipv6,
                &text_records,
                120,
            );
            join_and_send_multicast_v4(&socket, &announcement, &responder_ifaces);

            let s_type_query = service_type.trim_matches('.');
            let mut buf = [0u8; 4096];

            loop {
                // Check if stopped
                if stop_rx.try_recv().is_ok() {
                    // Send goodbye packet (TTL 0)
                    let goodbye = build_mdns_response(
                        &instance_name,
                        &service_type,
                        &domain,
                        port,
                        &ipv4,
                        &ipv6,
                        &text_records,
                        0,
                    );
                    join_and_send_multicast_v4(&socket, &goodbye, &responder_ifaces);
                    break;
                }

                match socket.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        if let Some(msg) = parse_dns_packet(&buf[..n]) {
                            // Check if this is a query for our service
                            let matches_query =
                                msg.questions.iter().any(|q| q.name.contains(s_type_query));
                            if matches_query {
                                let resp = build_mdns_response(
                                    &instance_name,
                                    &service_type,
                                    &domain,
                                    port,
                                    &ipv4,
                                    &ipv6,
                                    &text_records,
                                    120,
                                );
                                // Multicast reply according to RFC 6762
                                join_and_send_multicast_v4(&socket, &resp, &responder_ifaces);
                                // Also unicast reply directly to querier
                                let _ = socket.send_to(&resp, src);
                            }
                        }
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        });

        self.active_spec = Some(spec);
        self.active_file = file_path;
        self.stop_tx = Some(stop_tx);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(path) = self.active_file.take() {
            let _ = std::fs::remove_file(path);
        }
        self.active_spec = None;
        Ok(())
    }
}

impl Drop for RealAdvertiser {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub struct RealBrowser;

impl RealBrowser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealBrowser {
    fn default() -> Self {
        Self::new()
    }
}

impl Browser for RealBrowser {
    fn browse(&self, service_type: &str, domain: &str) -> Result<Vec<DiscoveredService>, String> {
        self.browse_with_timeout(service_type, domain, Duration::from_millis(1500))
    }
}

impl RealBrowser {
    pub fn browse_with_timeout(
        &self,
        service_type: &str,
        domain: &str,
        timeout: Duration,
    ) -> Result<Vec<DiscoveredService>, String> {
        let mut services_map: HashMap<String, DiscoveredService> = HashMap::new();

        // 1. Query via Multicast DNS
        let s_type = if service_type.is_empty() {
            DEFAULT_SERVICE_TYPE
        } else {
            service_type
        };
        let dom = if domain.is_empty() {
            DEFAULT_DOMAIN
        } else {
            domain
        };

        if let Ok(socket) = create_multicast_query_socket() {
            let query = build_mdns_query(s_type, dom);
            let query_ifaces = multicast_ipv4_interface_addrs(&query_system_interfaces());
            for addr in &query_ifaces {
                let _ = socket.join_multicast_v4(&MDNS_IPV4_MULTICAST, addr);
            }
            join_and_send_multicast_v4(&socket, &query, &query_ifaces);

            let start = std::time::Instant::now();
            let mut last_query = start;
            let mut buf = [0u8; 4096];

            while start.elapsed() < timeout {
                if last_query.elapsed() >= Duration::from_millis(300) {
                    join_and_send_multicast_v4(&socket, &query, &query_ifaces);
                    last_query = std::time::Instant::now();
                }
                match socket.recv_from(&mut buf) {
                    Ok((n, _src)) => {
                        if let Some(msg) = parse_dns_packet(&buf[..n]) {
                            // Extract discovered services from answers
                            let mut instance_names = Vec::new();
                            let mut srv_map: HashMap<String, (u16, String)> = HashMap::new();
                            let mut txt_map: HashMap<String, Vec<String>> = HashMap::new();
                            let mut a_map: HashMap<String, Vec<Ipv4Addr>> = HashMap::new();
                            let mut aaaa_map: HashMap<String, Vec<Ipv6Addr>> = HashMap::new();

                            for rec in &msg.answers {
                                match rec.rtype {
                                    12 => {
                                        // PTR
                                        let (target_inst, _) = decode_dns_name(&rec.rdata, 0);
                                        if !target_inst.is_empty() {
                                            instance_names.push(target_inst);
                                        }
                                    }
                                    33 => {
                                        // SRV
                                        if rec.rdata.len() >= 6 {
                                            let port =
                                                u16::from_be_bytes([rec.rdata[4], rec.rdata[5]]);
                                            let (host, _) = decode_dns_name(&rec.rdata, 6);
                                            srv_map.insert(rec.name.clone(), (port, host));
                                        }
                                    }
                                    16 => {
                                        // TXT
                                        let mut txt_lines = Vec::new();
                                        let mut pos = 0;
                                        while pos < rec.rdata.len() {
                                            let len = rec.rdata[pos] as usize;
                                            pos += 1;
                                            if pos + len > rec.rdata.len() {
                                                break;
                                            }
                                            let s =
                                                String::from_utf8_lossy(&rec.rdata[pos..pos + len])
                                                    .to_string();
                                            txt_lines.push(s);
                                            pos += len;
                                        }
                                        txt_map.insert(rec.name.clone(), txt_lines);
                                    }
                                    1 => {
                                        // A
                                        if rec.rdata.len() == 4 {
                                            let ip = Ipv4Addr::new(
                                                rec.rdata[0],
                                                rec.rdata[1],
                                                rec.rdata[2],
                                                rec.rdata[3],
                                            );
                                            a_map.entry(rec.name.clone()).or_default().push(ip);
                                        }
                                    }
                                    28 if rec.rdata.len() == 16 => {
                                        // AAAA
                                        let mut octets = [0u8; 16];
                                        octets.copy_from_slice(&rec.rdata[..16]);
                                        let ip = Ipv6Addr::from(octets);
                                        aaaa_map.entry(rec.name.clone()).or_default().push(ip);
                                    }
                                    _ => {}
                                }
                            }

                            // If we have SRV and/or TXT records directly
                            for (inst_fqdn, (port, host)) in &srv_map {
                                let short_name =
                                    inst_fqdn.split('.').next().unwrap_or(inst_fqdn).to_string();
                                let txt_lines = txt_map.get(inst_fqdn).cloned().unwrap_or_default();
                                let parsed_txt = parse_txt_records(&txt_lines);

                                let ipv4 = a_map.get(host).cloned().unwrap_or_default();
                                let ipv6 = aaaa_map.get(host).cloned().unwrap_or_default();

                                let mut endpoints = HashMap::new();
                                if let Some(v) = parsed_txt.get("api_openai") {
                                    endpoints.insert("openai".into(), v.clone());
                                }
                                if let Some(v) = parsed_txt.get("api_anthropic") {
                                    endpoints.insert("anthropic".into(), v.clone());
                                }
                                if let Some(v) = parsed_txt.get("api_gemini") {
                                    endpoints.insert("gemini".into(), v.clone());
                                }

                                let host_str = if !ipv4.is_empty() {
                                    ipv4[0].to_string()
                                } else {
                                    host.clone()
                                };

                                let svc = DiscoveredService {
                                    instance_name: short_name.clone(),
                                    service_type: s_type.to_string(),
                                    domain: dom.to_string(),
                                    host: host_str,
                                    port: *port,
                                    ipv4,
                                    ipv6,
                                    protocols: parsed_txt
                                        .get("protocols")
                                        .map(|p| {
                                            p.split(',').map(|s| s.trim().to_string()).collect()
                                        })
                                        .unwrap_or_default(),
                                    features: parsed_txt
                                        .get("features")
                                        .map(|f| {
                                            f.split(',').map(|s| s.trim().to_string()).collect()
                                        })
                                        .unwrap_or_default(),
                                    product: parsed_txt
                                        .get("product")
                                        .cloned()
                                        .unwrap_or_else(|| PRODUCT_CPA.into()),
                                    auth_required: parsed_txt
                                        .get("auth_required")
                                        .map(|v| v == "true")
                                        .unwrap_or(true),
                                    auth_methods: parsed_txt
                                        .get("auth_methods")
                                        .map(|m| {
                                            m.split(',').map(|s| s.trim().to_string()).collect()
                                        })
                                        .unwrap_or_default(),
                                    endpoints,
                                    node_role: parsed_txt
                                        .get("node_role")
                                        .cloned()
                                        .unwrap_or_else(|| "standalone".into()),
                                    version: parsed_txt
                                        .get("version")
                                        .cloned()
                                        .unwrap_or_else(|| "1".into()),
                                    raw_txt: parsed_txt,
                                };

                                services_map.insert(short_name, svc);
                            }
                        }
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            }
        }

        // Explicit file fallback is reserved for isolated tests; never make it
        // the default because a shared filesystem is not LAN discovery.
        let file_fallback = std::env::var("MAHOQUOT_DISCOVERY_FILE_FALLBACK").as_deref() == Ok("1");
        let dir = shared_discovery_dir();
        if file_fallback && dir.exists() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("json") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(svc) = serde_json::from_str::<DiscoveredService>(&content) {
                                if service_type.is_empty() || svc.service_type == service_type {
                                    services_map.entry(svc.instance_name.clone()).or_insert(svc);
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut res: Vec<DiscoveredService> = services_map.into_values().collect();
        res.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));
        Ok(res)
    }
}

pub struct DiscoveryManager {
    registry: Option<MockMdnsRegistry>,
    advertiser: Option<Box<dyn Advertiser>>,
    enabled: bool,
    last_spec: Option<ServiceSpec>,
    last_error: Option<String>,
}

impl DiscoveryManager {
    pub fn new(registry: MockMdnsRegistry) -> Self {
        Self {
            registry: Some(registry),
            advertiser: None,
            enabled: false,
            last_spec: None,
            last_error: None,
        }
    }

    pub fn real() -> Self {
        Self {
            registry: None,
            advertiser: None,
            enabled: false,
            last_spec: None,
            last_error: None,
        }
    }

    pub fn apply_config(
        &mut self,
        cfg: &DiscoveryConfig,
        bind_host: Option<&str>,
        port: u16,
        tls_enabled: bool,
        available_ifaces: &[NetworkInterface],
        state_dir: Option<&Path>,
    ) -> bool {
        if !cfg.enabled {
            if let Some(mut adv) = self.advertiser.take() {
                let _ = adv.stop();
            }
            self.enabled = false;
            self.last_spec = None;
            self.last_error = None;
            return true;
        }

        let spec = match build_service_spec(
            cfg,
            bind_host,
            port,
            tls_enabled,
            available_ifaces,
            state_dir,
        ) {
            Ok(s) => s,
            Err(e) => {
                if let Some(mut adv) = self.advertiser.take() {
                    let _ = adv.stop();
                }
                self.enabled = false;
                self.last_spec = None;
                self.last_error = Some(e);
                return false;
            }
        };

        if self.enabled && self.advertiser.is_some() && self.last_spec.as_ref() == Some(&spec) {
            return true;
        }

        if let Some(mut adv) = self.advertiser.take() {
            let _ = adv.stop();
        }

        let mut adv: Box<dyn Advertiser> = if let Some(ref reg) = self.registry {
            Box::new(MockAdvertiser::new(reg.clone()))
        } else {
            Box::new(RealAdvertiser::new())
        };

        if let Err(e) = adv.start(spec.clone()) {
            self.enabled = false;
            self.last_error = Some(e);
            return false;
        }

        self.advertiser = Some(adv);
        self.enabled = true;
        self.last_spec = Some(spec);
        self.last_error = None;
        true
    }

    pub fn shutdown(&mut self) {
        if let Some(mut adv) = self.advertiser.take() {
            let _ = adv.stop();
        }
        self.enabled = false;
        self.last_spec = None;
        self.last_error = None;
    }

    pub fn is_advertising(&self) -> bool {
        self.enabled && self.advertiser.is_some()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_instance_name_default_and_custom() {
        assert_eq!(format_instance_name("", "8F3B"), "CPA-8F3B");
        assert_eq!(format_instance_name("office", "8F3B"), "office-8F3B");
        assert_eq!(format_instance_name("office-8F3B", "8F3B"), "office-8F3B");
        assert_eq!(format_instance_name("CPA-8F3B", "8F3B"), "CPA-8F3B");

        let long = "n".repeat(70);
        let got = format_instance_name(&long, "8F3B");
        assert!(got.len() <= 63);
        assert!(got.ends_with("-8F3B"));
    }

    #[test]
    fn test_validate_service_type() {
        assert!(validate_service_type("_ai-gateway._tcp").is_ok());
        assert!(validate_service_type("_http._tcp").is_ok());
        assert!(validate_service_type("").is_err());
        assert!(validate_service_type("ai-gateway._tcp").is_err());
        assert!(validate_service_type("_ai-gateway._udp").is_err());
        assert!(validate_service_type("_-invalid._tcp").is_err());
    }

    #[test]
    fn test_sanitize_subtype() {
        assert_eq!(sanitize_subtype("responses"), "_responses");
        assert_eq!(sanitize_subtype("_messages"), "_messages");
        assert_eq!(sanitize_subtype("bad.dot"), "");
        assert_eq!(sanitize_subtype("-hyphen"), "");
    }

    #[test]
    fn test_txt_record_generation_within_limits() {
        let opts = TxtOptions {
            instance_id: "8F3B".into(),
            tls: true,
            ..Default::default()
        };
        let records = build_txt_records(&opts);

        let mut total_bytes = 0;
        for r in &records {
            assert!(r.len() <= MAX_TXT_RECORD_BYTES);
            total_bytes += r.len() + 1;
        }
        assert!(total_bytes <= MAX_TXT_BYTES);

        let parsed = parse_txt_records(&records);
        assert_eq!(parsed.get("version"), Some(&"1".to_string()));
        assert_eq!(parsed.get("product"), Some(&PRODUCT_CPA.to_string()));
        assert_eq!(parsed.get("instance_id"), Some(&"8F3B".to_string()));
        assert_eq!(parsed.get("tls"), Some(&"1".to_string()));
        assert_eq!(parsed.get("auth_required"), Some(&"true".to_string()));
    }

    #[test]
    fn test_filter_interfaces_physical_only() {
        let ifaces = vec![
            NetworkInterface {
                index: 1,
                name: "lo0".into(),
                is_up: true,
                is_loopback: true,
                is_multicast: true,
                is_point_to_point: false,
                addresses: vec!["127.0.0.1".parse().unwrap()],
            },
            NetworkInterface {
                index: 2,
                name: "docker0".into(),
                is_up: true,
                is_loopback: false,
                is_multicast: true,
                is_point_to_point: false,
                addresses: vec!["172.17.0.1".parse().unwrap()],
            },
            NetworkInterface {
                index: 3,
                name: "en0".into(),
                is_up: true,
                is_loopback: false,
                is_multicast: true,
                is_point_to_point: false,
                addresses: vec!["192.168.1.100".parse().unwrap()],
            },
        ];

        let filtered = filter_interfaces(&ifaces, &[], &[]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "en0");
    }

    #[test]
    fn test_filter_interfaces_explicit_include_overrides_docker() {
        let ifaces = vec![NetworkInterface {
            index: 1,
            name: "docker0".into(),
            is_up: true,
            is_loopback: false,
            is_multicast: true,
            is_point_to_point: false,
            addresses: vec!["172.17.0.1".parse().unwrap()],
        }];
        let filtered = filter_interfaces(&ifaces, &["docker0".into()], &[]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "docker0");
    }

    #[test]
    fn test_discovery_manager_lifecycle() {
        let registry = MockMdnsRegistry::new();
        let mut mgr = DiscoveryManager::new(registry.clone());
        let browser = MockBrowser::new(registry.clone());

        let ifaces = vec![NetworkInterface {
            index: 1,
            name: "en0".into(),
            is_up: true,
            is_loopback: false,
            is_multicast: true,
            is_point_to_point: false,
            addresses: vec!["192.168.1.50".parse().unwrap()],
        }];

        let mut cfg = DiscoveryConfig {
            enabled: true,
            service_name: "node-a".into(),
            ..Default::default()
        };

        assert!(mgr.apply_config(&cfg, None, 18801, false, &ifaces, None));
        assert!(mgr.is_advertising());

        let discovered = browser
            .browse(DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN)
            .unwrap();
        assert_eq!(discovered.len(), 1);
        assert!(discovered[0].instance_name.starts_with("node-a-"));
        assert_eq!(discovered[0].port, 18801);

        cfg.enabled = false;
        assert!(mgr.apply_config(&cfg, None, 18801, false, &ifaces, None));
        assert!(!mgr.is_advertising());

        let discovered2 = browser
            .browse(DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN)
            .unwrap();
        assert_eq!(discovered2.len(), 0);
    }

    #[test]
    fn test_mdns_packet_encode_decode_roundtrip() {
        let q = build_mdns_query(DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN);
        let parsed_q = parse_dns_packet(&q).expect("query must parse");
        assert_eq!(parsed_q.questions.len(), 1);
        assert_eq!(parsed_q.questions[0].qtype, 12);
        assert!(parsed_q.questions[0].name.contains("_ai-gateway._tcp"));

        let txt = vec!["version=1".to_string(), "product=cliproxyapi".to_string()];
        let resp = build_mdns_response(
            "CPA-UNITTEST",
            DEFAULT_SERVICE_TYPE,
            DEFAULT_DOMAIN,
            18801,
            &[Ipv4Addr::new(127, 0, 0, 1)],
            &[],
            &txt,
            120,
        );
        let parsed_resp = parse_dns_packet(&resp).expect("response must parse");
        assert_eq!(parsed_resp.answers.len(), 4); // PTR, SRV, TXT, A
        assert!(parsed_resp.answers.iter().any(|a| a.rtype == 12)); // PTR
        assert!(parsed_resp.answers.iter().any(|a| a.rtype == 33)); // SRV
        assert!(parsed_resp.answers.iter().any(|a| a.rtype == 16)); // TXT
        assert!(parsed_resp.answers.iter().any(|a| a.rtype == 1)); // A
    }

    #[test]
    fn test_mdns_goodbye_packet() {
        let resp = build_mdns_response(
            "CPA-GOODBYE",
            DEFAULT_SERVICE_TYPE,
            DEFAULT_DOMAIN,
            18801,
            &[Ipv4Addr::new(127, 0, 0, 1)],
            &[],
            &["version=1".to_string()],
            0,
        );
        let parsed_resp = parse_dns_packet(&resp).expect("response must parse");
        for ans in parsed_resp.answers {
            assert_eq!(ans.ttl, 0);
        }
    }
}
