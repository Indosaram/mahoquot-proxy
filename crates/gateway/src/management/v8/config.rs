//! Persisted V8 configuration view and mutation policy; never serialize runtime defaults.
use super::super::settings::{Settings, SettingsError};
use serde_json::{Map, Value};

const PATHS: &[(&str, &str)] = &[
    ("host", "server.host"),
    ("port", "server.port"),
    ("trusted-proxies", "server.trusted-proxies"),
    ("github-token", "server.github-token"),
    ("tls", "server.tls"),
    ("commercial-mode", "server.commercial-mode"),
    ("discovery", "server.discovery"),
    ("remote-management", "management"),
    ("api-keys", "access.api-keys"),
    ("credential-concurrency", "credentials.concurrency"),
    ("credential-in-flight", "credentials.in-flight"),
    ("force-model-prefix", "routing.force-model-prefix"),
    ("request-retry", "routing.retry.request-retry"),
    (
        "max-retry-credentials",
        "routing.retry.max-retry-credentials",
    ),
    ("max-retry-interval", "routing.retry.max-retry-interval"),
    ("disable-cooling", "routing.cooldown.disable-cooling"),
    (
        "save-cooldown-status",
        "routing.cooldown.save-cooldown-status",
    ),
    (
        "transient-error-cooldown-seconds",
        "routing.cooldown.transient-error-cooldown-seconds",
    ),
    ("proxy-url", "requests.proxy-url"),
    ("passthrough-headers", "requests.passthrough-headers"),
    (
        "nonstream-keepalive-interval",
        "requests.nonstream-keepalive-interval",
    ),
    ("streaming", "requests.streaming"),
    ("payload", "requests.payload"),
    ("auth-dir", "oauth.auth-dir"),
    (
        "auth-auto-refresh-workers",
        "oauth.auth-auto-refresh-workers",
    ),
    ("oauth-model-alias", "oauth.model-alias"),
    ("oauth-excluded-models", "oauth.excluded-models"),
    ("oauth-request-scoped-errors", "oauth.request-scoped-errors"),
    ("oauth-settings", "oauth.settings"),
    ("ws-auth", "oauth.providers.aistudio.ws-auth"),
    (
        "codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
    ),
    (
        "codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    (
        "codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
    ),
    (
        "codex.response-steering",
        "upstream.codex.response-steering",
    ),
    ("codex", "oauth.providers.codex"),
    (
        "codex-header-defaults",
        "oauth.providers.codex.header-defaults",
    ),
    ("claude", "upstream.claude"),
    ("claude-code", "upstream.claude"),
    (
        "disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
    ),
    ("claude-header-defaults", "upstream.claude.header-defaults"),
    ("antigravity", "oauth.providers.antigravity"),
    (
        "antigravity-signature-cache-enabled",
        "oauth.providers.antigravity.signature-cache-enabled",
    ),
    (
        "antigravity-signature-bypass-strict",
        "oauth.providers.antigravity.signature-bypass-strict",
    ),
    (
        "quota-exceeded.antigravity-credits",
        "oauth.providers.antigravity.antigravity-credits",
    ),
    ("xai", "upstream.xai"),
    ("devin", "oauth.providers.devin"),
    (
        "disable-image-generation",
        "multimedia.disable-image-generation",
    ),
    (
        "gpt-image-2-base-model",
        "multimedia.gpt-image-2-base-model",
    ),
    (
        "video-result-auth-cache-ttl",
        "multimedia.video-result-auth-cache-ttl",
    ),
    ("debug", "observability.logs.debug"),
    ("logging-to-file", "observability.logs.logging-to-file"),
    (
        "logs-max-total-size-mb",
        "observability.logs.logs-max-total-size-mb",
    ),
    ("request-log", "observability.logs.request-log"),
    (
        "error-logs-max-files",
        "observability.logs.error-logs-max-files",
    ),
    (
        "usage-statistics-enabled",
        "observability.usage.usage-statistics-enabled",
    ),
    (
        "redis-usage-queue-retention-seconds",
        "observability.usage.redis-usage-queue-retention-seconds",
    ),
    ("pprof", "observability.pprof"),
];
const ALIASES: &[(&str, &str)] = &[
    (
        "oauth.providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "oauth.providers.codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "oauth.providers.codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    (
        "oauth.providers.codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
    ),
    (
        "oauth.providers.codex.response-steering",
        "upstream.codex.response-steering",
    ),
    (
        "oauth.providers.claude.model-level-cooling",
        "upstream.claude.model-level-cooling",
    ),
    (
        "oauth.providers.claude.claude-code.disable-cloaking-model-list",
        "upstream.claude.disable-cloaking-model-list",
    ),
    (
        "oauth.providers.claude.disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
    ),
    (
        "oauth.providers.claude.header-defaults.user-agent",
        "upstream.claude.header-defaults.user-agent",
    ),
    (
        "oauth.providers.claude.header-defaults.package-version",
        "upstream.claude.header-defaults.package-version",
    ),
    (
        "oauth.providers.claude.header-defaults.runtime-version",
        "upstream.claude.header-defaults.runtime-version",
    ),
    (
        "oauth.providers.claude.header-defaults.os",
        "upstream.claude.header-defaults.os",
    ),
    (
        "oauth.providers.claude.header-defaults.arch",
        "upstream.claude.header-defaults.arch",
    ),
    (
        "oauth.providers.claude.header-defaults.timeout",
        "upstream.claude.header-defaults.timeout",
    ),
    (
        "oauth.providers.claude.header-defaults.timezone",
        "upstream.claude.header-defaults.timezone",
    ),
    (
        "oauth.providers.claude.header-defaults.stabilize-device-profile",
        "upstream.claude.header-defaults.stabilize-device-profile",
    ),
    (
        "oauth.providers.xai.inject-x-search",
        "upstream.xai.inject-x-search",
    ),
    (
        "oauth.providers.claude.header-defaults",
        "upstream.claude.header-defaults",
    ),
    ("oauth.providers.claude.claude-code", "upstream.claude"),
    ("oauth.providers.claude", "upstream.claude"),
    ("oauth.providers.xai", "upstream.xai"),
];
const FAMILIES: &[&str] = &[
    "gemini",
    "interactions",
    "vertex",
    "codex",
    "claude",
    "xai",
    "meta",
    "openai-compatibility",
];
const REVISIONS: &[&str] = &[
    "credentials.concurrency.lifecycle-config-revision",
    "credentials.concurrency.observation-barrier-revision",
    "plugins.auth-revision",
];
const ICE: &str = "oauth.providers.codex.live-media-relay.ice-servers";
const SCHEMA: &[(&str, &[(&str, &str)])] = &[
    (
        "Config",
        &[
            ("models", "ModelCatalogs"),
            ("host", "string"),
            ("port", "int"),
            ("github-token", "string"),
            ("trusted-proxies", "[]string"),
            ("tls", "TLSConfig"),
            ("credential-concurrency", "CredentialConcurrencyConfig"),
            ("credential-in-flight", "CredentialInFlightConfig"),
            ("remote-management", "RemoteManagement"),
            ("plugins", "PluginsConfig"),
            ("auth-dir", "string"),
            ("debug", "bool"),
            ("pprof", "PprofConfig"),
            ("discovery", "DiscoveryConfig"),
            ("commercial-mode", "bool"),
            ("logging-to-file", "bool"),
            ("logs-max-total-size-mb", "int"),
            ("error-logs-max-files", "int"),
            ("usage-statistics-enabled", "bool"),
            ("redis-usage-queue-retention-seconds", "int"),
            ("disable-cooling", "bool"),
            ("save-cooldown-status", "bool"),
            ("transient-error-cooldown-seconds", "int"),
            ("auth-auto-refresh-workers", "int"),
            ("request-retry", "int"),
            ("max-retry-credentials", "int"),
            ("max-retry-interval", "int"),
            ("quota-exceeded", "QuotaExceeded"),
            ("routing", "RoutingConfig"),
            ("ws-auth", "bool"),
            ("antigravity-signature-cache-enabled", "*bool"),
            ("antigravity-signature-bypass-strict", "*bool"),
            ("antigravity", "AntigravityConfig"),
            ("devin", "DevinConfig"),
            ("gemini-api-key", "[]GeminiKey"),
            ("interactions-api-key", "[]GeminiKey"),
            ("codex-api-key", "[]CodexKey"),
            ("xai-api-key", "[]XAIKey"),
            ("meta-api-key", "[]MetaKey"),
            ("xai", "XAIConfig"),
            ("codex", "CodexConfig"),
            ("codex-header-defaults", "CodexHeaderDefaults"),
            ("claude", "ClaudeConfig"),
            ("claude-api-key", "[]ClaudeKey"),
            ("claude-header-defaults", "ClaudeHeaderDefaults"),
            ("disable-claude-cloak-mode", "bool"),
            ("openai-compatibility", "[]OpenAICompatibility"),
            ("vertex-api-key", "[]VertexCompatKey"),
            ("oauth-excluded-models", "map[string][]string"),
            ("oauth-model-alias", "map[string][]OAuthModelAlias"),
            (
                "oauth-request-scoped-errors",
                "map[string][]RequestScopedErrorRule",
            ),
            ("oauth-settings", "map[string][]OAuthModelSetting"),
            ("payload", "PayloadConfig"),
            ("client", "ClientConfig"),
            ("proxy-url", "string"),
            ("disable-image-generation", "DisableImageGenerationMode"),
            ("gpt-image-2-base-model", "string"),
            ("video-result-auth-cache-ttl", "string"),
            ("force-model-prefix", "bool"),
            ("request-log", "bool"),
            ("claude-code", "ClaudeCodeConfig"),
            ("api-keys", "[]string"),
            ("passthrough-headers", "bool"),
            ("streaming", "StreamingConfig"),
            ("nonstream-keepalive-interval", "int"),
        ],
    ),
    (
        "RequestScopedErrorRule",
        &[
            ("status", "int"),
            ("match", "[]string"),
            ("match-regexr", "[]string"),
            ("action", "string"),
        ],
    ),
    (
        "PluginsConfig",
        &[
            ("enabled", "bool"),
            ("dir", "string"),
            ("store-sources", "[]string"),
            ("store-auth", "[]sdkpluginstore.AuthConfig"),
            ("auth-revision", "int64"),
            ("configs", "map[string]PluginInstanceConfig"),
        ],
    ),
    (
        "PluginInstanceConfig",
        &[("enabled", "*bool"), ("priority", "int")],
    ),
    ("ClaudeConfig", &[("model-level-cooling", "bool")]),
    (
        "ClaudeHeaderDefaults",
        &[
            ("user-agent", "string"),
            ("package-version", "string"),
            ("runtime-version", "string"),
            ("os", "string"),
            ("arch", "string"),
            ("timeout", "string"),
            ("timezone", "string"),
            ("stabilize-device-profile", "*bool"),
        ],
    ),
    (
        "CodexHeaderDefaults",
        &[("user-agent", "string"), ("beta-features", "string")],
    ),
    ("XAIConfig", &[("inject-x-search", "bool")]),
    ("DevinConfig", &[("sensitive-words", "[]string")]),
    (
        "AntigravityConfig",
        &[
            ("sensitive-words", "[]string"),
            ("connection-pool", "AntigravityConnectionPoolConfig"),
        ],
    ),
    (
        "AntigravityConnectionPoolConfig",
        &[
            ("enabled", "*bool"),
            ("idle-conn-timeout", "string"),
            ("max-idle-conns-per-host", "*int"),
        ],
    ),
    (
        "CodexConfig",
        &[
            ("disable-codex-cloaking", "bool"),
            ("stream-bootstrap-buffering", "bool"),
            ("stream-bootstrap-timeout", "stream-bootstrap-timeout"),
            ("orphan-delegation-compatibility", "bool"),
            ("model-level-cooling", "bool"),
            ("live-media-relay", "CodexLiveMediaRelayConfig"),
            ("response-steering", "bool"),
        ],
    ),
    (
        "CodexLiveMediaRelayConfig",
        &[
            ("enabled", "bool"),
            ("max-sessions", "int"),
            ("disable-private-remote-ips", "bool"),
            ("public-ip", "string"),
            ("udp-port-min", "uint16"),
            ("udp-port-max", "uint16"),
            ("ice-servers", "[]CodexLiveICEServer"),
        ],
    ),
    (
        "CodexLiveICEServer",
        &[
            ("urls", "[]string"),
            ("username", "string"),
            ("credential", "string"),
        ],
    ),
    (
        "TLSConfig",
        &[("enable", "bool"), ("cert", "string"), ("key", "string")],
    ),
    ("PprofConfig", &[("enable", "bool"), ("addr", "string")]),
    (
        "DiscoveryInterfacesConfig",
        &[("include", "[]string"), ("exclude", "[]string")],
    ),
    (
        "DiscoveryConfig",
        &[
            ("enabled", "bool"),
            ("service-name", "string"),
            ("service-type", "string"),
            ("subtypes", "[]string"),
            ("interfaces", "DiscoveryInterfacesConfig"),
            ("auth-required", "*bool"),
            ("advertise-management", "bool"),
        ],
    ),
    (
        "RemoteManagement",
        &[
            ("allow-remote", "bool"),
            ("secret-key", "string"),
            ("disable-control-panel", "bool"),
            ("disable-auto-update-panel", "bool"),
            ("panel-github-repository", "string"),
            ("base-url", "string"),
        ],
    ),
    (
        "QuotaExceeded",
        &[
            ("switch-project", "bool"),
            ("switch-preview-model", "bool"),
            ("antigravity-credits", "bool"),
        ],
    ),
    (
        "RoutingConfig",
        &[
            ("strategy", "string"),
            ("session-affinity", "bool"),
            ("session-affinity-ttl", "string"),
            ("session-affinity-subagents", "*bool"),
        ],
    ),
    (
        "OAuthModelAlias",
        &[
            ("name", "string"),
            ("alias", "string"),
            ("fork", "bool"),
            ("display-name", "string"),
            ("force-mapping", "bool"),
        ],
    ),
    (
        "OAuthModelSetting",
        &[
            ("name", "string"),
            ("alias", "string"),
            ("max-context-length", "int"),
        ],
    ),
    (
        "PayloadConfig",
        &[
            ("default", "[]PayloadRule"),
            ("default-raw", "[]PayloadRule"),
            ("override", "[]PayloadRule"),
            ("override-raw", "[]PayloadRule"),
            ("filter", "[]PayloadFilterRule"),
        ],
    ),
    (
        "PayloadFilterRule",
        &[("models", "[]PayloadModelRule"), ("params", "[]string")],
    ),
    (
        "PayloadRule",
        &[
            ("models", "[]PayloadModelRule"),
            ("params", "map[string]any"),
        ],
    ),
    (
        "PayloadModelRule",
        &[
            ("name", "string"),
            ("protocol", "string"),
            ("headers", "map[string]string"),
            ("from-protocol", "string"),
            ("match", "[]map[string]any"),
            ("not-match", "[]map[string]any"),
            ("exist", "[]string"),
            ("not-exist", "[]string"),
        ],
    ),
    (
        "CloakConfig",
        &[
            ("mode", "string"),
            ("strict-mode", "bool"),
            ("sensitive-words", "[]string"),
            ("cache-user-id", "*bool"),
        ],
    ),
    (
        "ClaudeKey",
        &[
            ("api-key", "string"),
            ("priority", "int"),
            ("weight", "*int"),
            ("prefix", "string"),
            ("base-url", "string"),
            ("proxy-url", "string"),
            ("models", "[]ClaudeModel"),
            ("headers", "map[string]string"),
            ("excluded-models", "[]string"),
            ("rebuild-mid-system-message", "bool"),
            ("disable-cooling", "*bool"),
            ("request-retry", "*int"),
            ("request-scoped-errors", "[]RequestScopedErrorRule"),
            ("cloak", "*CloakConfig"),
            ("fingerprint-profile", "string"),
            ("experimental-cch-signing", "bool"),
        ],
    ),
    (
        "ClaudeModel",
        &[
            ("name", "string"),
            ("alias", "string"),
            ("display-name", "string"),
            ("max-context-length", "int"),
            ("force-mapping", "bool"),
            ("is-compat", "bool"),
            ("thinking", "*registry.ThinkingSupport"),
        ],
    ),
    (
        "CodexKey",
        &[
            ("api-key", "string"),
            ("priority", "int"),
            ("weight", "*int"),
            ("prefix", "string"),
            ("base-url", "string"),
            ("websockets", "bool"),
            ("alpha-search", "bool"),
            ("proxy-url", "string"),
            ("models", "[]CodexModel"),
            ("headers", "map[string]string"),
            ("excluded-models", "[]string"),
            ("disable-cooling", "*bool"),
            ("disable-codex-cloaking", "*bool"),
            ("request-retry", "*int"),
            ("request-scoped-errors", "[]RequestScopedErrorRule"),
        ],
    ),
    (
        "CodexModel",
        &[
            ("name", "string"),
            ("alias", "string"),
            ("display-name", "string"),
            ("max-context-length", "int"),
            ("force-mapping", "bool"),
            ("is-compat", "bool"),
            ("support-configuration-update", "bool"),
            ("thinking", "*registry.ThinkingSupport"),
        ],
    ),
    (
        "GeminiKey",
        &[
            ("api-key", "string"),
            ("priority", "int"),
            ("weight", "*int"),
            ("prefix", "string"),
            ("base-url", "string"),
            ("proxy-url", "string"),
            ("models", "[]GeminiModel"),
            ("headers", "map[string]string"),
            ("excluded-models", "[]string"),
            ("disable-cooling", "*bool"),
            ("request-retry", "*int"),
            ("request-scoped-errors", "[]RequestScopedErrorRule"),
        ],
    ),
    (
        "GeminiModel",
        &[
            ("name", "string"),
            ("alias", "string"),
            ("display-name", "string"),
            ("max-context-length", "int"),
            ("force-mapping", "bool"),
            ("is-compat", "bool"),
            ("thinking", "*registry.ThinkingSupport"),
        ],
    ),
    (
        "OpenAICompatibility",
        &[
            ("name", "string"),
            ("priority", "int"),
            ("disabled", "bool"),
            ("prefix", "string"),
            ("base-url", "string"),
            ("api-key-entries", "[]OpenAICompatibilityAPIKey"),
            ("models", "[]OpenAICompatibilityModel"),
            ("headers", "map[string]string"),
            ("support-prompt-cache-key", "bool"),
            ("disable-cooling", "*bool"),
            ("request-retry", "*int"),
            ("request-scoped-errors", "[]RequestScopedErrorRule"),
        ],
    ),
    (
        "OpenAICompatibilityAPIKey",
        &[
            ("api-key", "string"),
            ("weight", "*int"),
            ("proxy-url", "string"),
        ],
    ),
    (
        "OpenAICompatibilityModel",
        &[
            ("name", "string"),
            ("alias", "string"),
            ("display-name", "string"),
            ("max-context-length", "int"),
            ("force-mapping", "bool"),
            ("image", "bool"),
            ("input-modalities", "[]string"),
            ("output-modalities", "[]string"),
            ("is-compat", "bool"),
            ("use-max-completion-tokens", "bool"),
            ("thinking", "*registry.ThinkingSupport"),
        ],
    ),
    (
        "CredentialConcurrencyConfig",
        &[
            ("lifecycle-config-revision", "int64"),
            ("observation-barrier-revision", "int64"),
            ("cpa-heartbeat-timeout", "time.Duration"),
            ("cpa-cancel-bound", "time.Duration"),
            ("reclaim-grace", "time.Duration"),
            ("cleanup-interval", "time.Duration"),
            ("release-flush-interval", "time.Duration"),
            ("release-max-backoff", "time.Duration"),
            ("busy-retry-min", "time.Duration"),
            ("busy-retry-max", "time.Duration"),
            ("max-limit", "int64"),
        ],
    ),
    (
        "CredentialInFlightConfig",
        &[
            ("snapshot-interval", "string"),
            ("stale-after", "string"),
            ("max-part-bytes", "int"),
            ("max-part-count", "int"),
            ("max-revision-bytes", "int"),
            ("max-aggregate-groups", "int"),
            ("max-details", "int"),
            ("max-string-bytes", "int"),
            ("staging-retention", "string"),
        ],
    ),
    (
        "VertexCompatKey",
        &[
            ("api-key", "string"),
            ("priority", "int"),
            ("weight", "*int"),
            ("prefix", "string"),
            ("base-url", "string"),
            ("proxy-url", "string"),
            ("headers", "map[string]string"),
            ("models", "[]VertexCompatModel"),
            ("excluded-models", "[]string"),
            ("disable-cooling", "*bool"),
            ("request-retry", "*int"),
            ("interactions", "*bool"),
        ],
    ),
    (
        "VertexCompatModel",
        &[
            ("name", "string"),
            ("alias", "string"),
            ("display-name", "string"),
            ("force-mapping", "bool"),
            ("thinking", "*registry.ThinkingSupport"),
        ],
    ),
    (
        "SDKConfig",
        &[
            ("client", "ClientConfig"),
            ("proxy-url", "string"),
            ("disable-image-generation", "DisableImageGenerationMode"),
            ("gpt-image-2-base-model", "string"),
            ("video-result-auth-cache-ttl", "string"),
            ("force-model-prefix", "bool"),
            ("request-log", "bool"),
            ("claude-code", "ClaudeCodeConfig"),
            ("api-keys", "[]string"),
            ("passthrough-headers", "bool"),
            ("streaming", "StreamingConfig"),
            ("nonstream-keepalive-interval", "int"),
        ],
    ),
    ("ClientConfig", &[("codex", "CodexClientConfig")]),
    (
        "CodexClientConfig",
        &[
            ("optimize-multi-agent-v2", "bool"),
            ("enable-apply-patch", "bool"),
        ],
    ),
    (
        "ClaudeCodeConfig",
        &[("disable-cloaking-model-list", "bool")],
    ),
    (
        "StreamingConfig",
        &[("keepalive-seconds", "int"), ("bootstrap-retries", "int")],
    ),
    (
        "CatalogSources",
        &[
            ("catalog", "string"),
            ("codex-catalog", "string"),
            ("devin-catalog", "string"),
        ],
    ),
    (
        "AuthConfig",
        &[
            ("match", "string"),
            ("apply-to", "[]string"),
            ("type", "string"),
            ("token-env", "string"),
            ("username-env", "string"),
            ("password-env", "string"),
            ("header-name", "string"),
            ("header-value-env", "string"),
            ("allow-insecure", "bool"),
        ],
    ),
    (
        "ResolvedAuthConfig",
        &[
            ("match", "string"),
            ("apply-to", "[]string"),
            ("type", "string"),
            ("token", "Secret"),
            ("username", "Secret"),
            ("password", "Secret"),
            ("header-name", "string"),
            ("header-value", "Secret"),
        ],
    ),
    (
        "ModelCatalogs",
        &[
            ("catalog", "string"),
            ("codex-catalog", "string"),
            ("devin-catalog", "string"),
        ],
    ),
    (
        "sdkpluginstore.AuthConfig",
        &[
            ("match", "string"),
            ("apply-to", "[]string"),
            ("type", "string"),
            ("token-env", "string"),
            ("username-env", "string"),
            ("password-env", "string"),
            ("header-name", "string"),
            ("header-value-env", "string"),
            ("allow-insecure", "bool"),
        ],
    ),
    (
        "registry.ThinkingSupport",
        &[
            ("min", "int"),
            ("max", "int"),
            ("zero-allowed", "bool"),
            ("dynamic-allowed", "bool"),
            ("levels", "[]string"),
        ],
    ),
];
const LEAVES: &[(&str, &str, &str)] = &[
    ("models.catalog", "models.catalog", "string"),
    ("models.codex-catalog", "models.codex-catalog", "string"),
    ("models.devin-catalog", "models.devin-catalog", "string"),
    ("host", "server.host", "string"),
    ("port", "server.port", "int"),
    ("github-token", "server.github-token", "string"),
    ("trusted-proxies", "server.trusted-proxies", "[]string"),
    ("tls.enable", "server.tls.enable", "bool"),
    ("tls.cert", "server.tls.cert", "string"),
    ("tls.key", "server.tls.key", "string"),
    (
        "credential-concurrency.lifecycle-config-revision",
        "credentials.concurrency.lifecycle-config-revision",
        "int64",
    ),
    (
        "credential-concurrency.observation-barrier-revision",
        "credentials.concurrency.observation-barrier-revision",
        "int64",
    ),
    (
        "credential-concurrency.cpa-heartbeat-timeout",
        "credentials.concurrency.cpa-heartbeat-timeout",
        "time.Duration",
    ),
    (
        "credential-concurrency.cpa-cancel-bound",
        "credentials.concurrency.cpa-cancel-bound",
        "time.Duration",
    ),
    (
        "credential-concurrency.reclaim-grace",
        "credentials.concurrency.reclaim-grace",
        "time.Duration",
    ),
    (
        "credential-concurrency.cleanup-interval",
        "credentials.concurrency.cleanup-interval",
        "time.Duration",
    ),
    (
        "credential-concurrency.release-flush-interval",
        "credentials.concurrency.release-flush-interval",
        "time.Duration",
    ),
    (
        "credential-concurrency.release-max-backoff",
        "credentials.concurrency.release-max-backoff",
        "time.Duration",
    ),
    (
        "credential-concurrency.busy-retry-min",
        "credentials.concurrency.busy-retry-min",
        "time.Duration",
    ),
    (
        "credential-concurrency.busy-retry-max",
        "credentials.concurrency.busy-retry-max",
        "time.Duration",
    ),
    (
        "credential-concurrency.max-limit",
        "credentials.concurrency.max-limit",
        "int64",
    ),
    (
        "credential-in-flight.snapshot-interval",
        "credentials.in-flight.snapshot-interval",
        "string",
    ),
    (
        "credential-in-flight.stale-after",
        "credentials.in-flight.stale-after",
        "string",
    ),
    (
        "credential-in-flight.max-part-bytes",
        "credentials.in-flight.max-part-bytes",
        "int",
    ),
    (
        "credential-in-flight.max-part-count",
        "credentials.in-flight.max-part-count",
        "int",
    ),
    (
        "credential-in-flight.max-revision-bytes",
        "credentials.in-flight.max-revision-bytes",
        "int",
    ),
    (
        "credential-in-flight.max-aggregate-groups",
        "credentials.in-flight.max-aggregate-groups",
        "int",
    ),
    (
        "credential-in-flight.max-details",
        "credentials.in-flight.max-details",
        "int",
    ),
    (
        "credential-in-flight.max-string-bytes",
        "credentials.in-flight.max-string-bytes",
        "int",
    ),
    (
        "credential-in-flight.staging-retention",
        "credentials.in-flight.staging-retention",
        "string",
    ),
    (
        "remote-management.allow-remote",
        "management.allow-remote",
        "bool",
    ),
    (
        "remote-management.secret-key",
        "management.secret-key",
        "string",
    ),
    (
        "remote-management.disable-control-panel",
        "management.disable-control-panel",
        "bool",
    ),
    (
        "remote-management.disable-auto-update-panel",
        "management.disable-auto-update-panel",
        "bool",
    ),
    (
        "remote-management.panel-github-repository",
        "management.panel-github-repository",
        "string",
    ),
    (
        "remote-management.base-url",
        "management.base-url",
        "string",
    ),
    ("plugins.enabled", "plugins.enabled", "bool"),
    ("plugins.dir", "plugins.dir", "string"),
    ("plugins.store-sources", "plugins.store-sources", "[]string"),
    (
        "plugins.store-auth",
        "plugins.store-auth",
        "[]sdkpluginstore.AuthConfig",
    ),
    ("plugins.auth-revision", "plugins.auth-revision", "int64"),
    (
        "plugins.configs",
        "plugins.configs",
        "map[string]PluginInstanceConfig",
    ),
    ("auth-dir", "oauth.auth-dir", "string"),
    ("debug", "observability.logs.debug", "bool"),
    ("pprof.enable", "observability.pprof.enable", "bool"),
    ("pprof.addr", "observability.pprof.addr", "string"),
    ("discovery.enabled", "server.discovery.enabled", "bool"),
    (
        "discovery.service-name",
        "server.discovery.service-name",
        "string",
    ),
    (
        "discovery.service-type",
        "server.discovery.service-type",
        "string",
    ),
    (
        "discovery.subtypes",
        "server.discovery.subtypes",
        "[]string",
    ),
    (
        "discovery.interfaces.include",
        "server.discovery.interfaces.include",
        "[]string",
    ),
    (
        "discovery.interfaces.exclude",
        "server.discovery.interfaces.exclude",
        "[]string",
    ),
    (
        "discovery.auth-required",
        "server.discovery.auth-required",
        "bool",
    ),
    (
        "discovery.advertise-management",
        "server.discovery.advertise-management",
        "bool",
    ),
    ("commercial-mode", "server.commercial-mode", "bool"),
    (
        "logging-to-file",
        "observability.logs.logging-to-file",
        "bool",
    ),
    (
        "logs-max-total-size-mb",
        "observability.logs.logs-max-total-size-mb",
        "int",
    ),
    (
        "error-logs-max-files",
        "observability.logs.error-logs-max-files",
        "int",
    ),
    (
        "usage-statistics-enabled",
        "observability.usage.usage-statistics-enabled",
        "bool",
    ),
    (
        "redis-usage-queue-retention-seconds",
        "observability.usage.redis-usage-queue-retention-seconds",
        "int",
    ),
    (
        "disable-cooling",
        "routing.cooldown.disable-cooling",
        "bool",
    ),
    (
        "save-cooldown-status",
        "routing.cooldown.save-cooldown-status",
        "bool",
    ),
    (
        "transient-error-cooldown-seconds",
        "routing.cooldown.transient-error-cooldown-seconds",
        "int",
    ),
    (
        "auth-auto-refresh-workers",
        "oauth.auth-auto-refresh-workers",
        "int",
    ),
    ("request-retry", "routing.retry.request-retry", "int"),
    (
        "max-retry-credentials",
        "routing.retry.max-retry-credentials",
        "int",
    ),
    (
        "max-retry-interval",
        "routing.retry.max-retry-interval",
        "int",
    ),
    (
        "quota-exceeded.switch-project",
        "quota-exceeded.switch-project",
        "bool",
    ),
    (
        "quota-exceeded.switch-preview-model",
        "quota-exceeded.switch-preview-model",
        "bool",
    ),
    (
        "quota-exceeded.antigravity-credits",
        "oauth.providers.antigravity.antigravity-credits",
        "bool",
    ),
    ("routing.strategy", "routing.strategy", "string"),
    (
        "routing.session-affinity",
        "routing.session-affinity",
        "bool",
    ),
    (
        "routing.session-affinity-ttl",
        "routing.session-affinity-ttl",
        "string",
    ),
    (
        "routing.session-affinity-subagents",
        "routing.session-affinity-subagents",
        "bool",
    ),
    ("ws-auth", "oauth.providers.aistudio.ws-auth", "bool"),
    (
        "antigravity-signature-cache-enabled",
        "oauth.providers.antigravity.signature-cache-enabled",
        "bool",
    ),
    (
        "antigravity-signature-bypass-strict",
        "oauth.providers.antigravity.signature-bypass-strict",
        "bool",
    ),
    (
        "antigravity.sensitive-words",
        "oauth.providers.antigravity.sensitive-words",
        "[]string",
    ),
    (
        "antigravity.connection-pool.enabled",
        "oauth.providers.antigravity.connection-pool.enabled",
        "bool",
    ),
    (
        "antigravity.connection-pool.idle-conn-timeout",
        "oauth.providers.antigravity.connection-pool.idle-conn-timeout",
        "string",
    ),
    (
        "antigravity.connection-pool.max-idle-conns-per-host",
        "oauth.providers.antigravity.connection-pool.max-idle-conns-per-host",
        "int",
    ),
    (
        "devin.sensitive-words",
        "oauth.providers.devin.sensitive-words",
        "[]string",
    ),
    ("gemini-api-key", "gemini-api-key", "[]GeminiKey"),
    (
        "interactions-api-key",
        "interactions-api-key",
        "[]GeminiKey",
    ),
    ("codex-api-key", "codex-api-key", "[]CodexKey"),
    ("xai-api-key", "xai-api-key", "[]XAIKey"),
    ("meta-api-key", "meta-api-key", "[]MetaKey"),
    (
        "xai.inject-x-search",
        "upstream.xai.inject-x-search",
        "bool",
    ),
    (
        "codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
        "bool",
    ),
    (
        "codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
        "bool",
    ),
    (
        "codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
        "stream-bootstrap-timeout",
    ),
    (
        "codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
        "bool",
    ),
    (
        "codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
        "bool",
    ),
    (
        "codex.live-media-relay.enabled",
        "oauth.providers.codex.live-media-relay.enabled",
        "bool",
    ),
    (
        "codex.live-media-relay.max-sessions",
        "oauth.providers.codex.live-media-relay.max-sessions",
        "int",
    ),
    (
        "codex.live-media-relay.disable-private-remote-ips",
        "oauth.providers.codex.live-media-relay.disable-private-remote-ips",
        "bool",
    ),
    (
        "codex.live-media-relay.public-ip",
        "oauth.providers.codex.live-media-relay.public-ip",
        "string",
    ),
    (
        "codex.live-media-relay.udp-port-min",
        "oauth.providers.codex.live-media-relay.udp-port-min",
        "uint16",
    ),
    (
        "codex.live-media-relay.udp-port-max",
        "oauth.providers.codex.live-media-relay.udp-port-max",
        "uint16",
    ),
    (
        "codex.live-media-relay.ice-servers",
        "oauth.providers.codex.live-media-relay.ice-servers",
        "[]CodexLiveICEServer",
    ),
    (
        "codex.response-steering",
        "upstream.codex.response-steering",
        "bool",
    ),
    (
        "codex-header-defaults.user-agent",
        "oauth.providers.codex.header-defaults.user-agent",
        "string",
    ),
    (
        "codex-header-defaults.beta-features",
        "oauth.providers.codex.header-defaults.beta-features",
        "string",
    ),
    (
        "claude.model-level-cooling",
        "upstream.claude.model-level-cooling",
        "bool",
    ),
    ("claude-api-key", "claude-api-key", "[]ClaudeKey"),
    (
        "claude-header-defaults.user-agent",
        "upstream.claude.header-defaults.user-agent",
        "string",
    ),
    (
        "claude-header-defaults.package-version",
        "upstream.claude.header-defaults.package-version",
        "string",
    ),
    (
        "claude-header-defaults.runtime-version",
        "upstream.claude.header-defaults.runtime-version",
        "string",
    ),
    (
        "claude-header-defaults.os",
        "upstream.claude.header-defaults.os",
        "string",
    ),
    (
        "claude-header-defaults.arch",
        "upstream.claude.header-defaults.arch",
        "string",
    ),
    (
        "claude-header-defaults.timeout",
        "upstream.claude.header-defaults.timeout",
        "string",
    ),
    (
        "claude-header-defaults.timezone",
        "upstream.claude.header-defaults.timezone",
        "string",
    ),
    (
        "claude-header-defaults.stabilize-device-profile",
        "upstream.claude.header-defaults.stabilize-device-profile",
        "bool",
    ),
    (
        "disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
        "bool",
    ),
    (
        "openai-compatibility",
        "openai-compatibility",
        "[]OpenAICompatibility",
    ),
    ("vertex-api-key", "vertex-api-key", "[]VertexCompatKey"),
    (
        "oauth-excluded-models",
        "oauth.excluded-models",
        "map[string][]string",
    ),
    (
        "oauth-model-alias",
        "oauth.model-alias",
        "map[string][]OAuthModelAlias",
    ),
    (
        "oauth-request-scoped-errors",
        "oauth.request-scoped-errors",
        "map[string][]RequestScopedErrorRule",
    ),
    (
        "oauth-settings",
        "oauth.settings",
        "map[string][]OAuthModelSetting",
    ),
    (
        "payload.default",
        "requests.payload.default",
        "[]PayloadRule",
    ),
    (
        "payload.default-raw",
        "requests.payload.default-raw",
        "[]PayloadRule",
    ),
    (
        "payload.override",
        "requests.payload.override",
        "[]PayloadRule",
    ),
    (
        "payload.override-raw",
        "requests.payload.override-raw",
        "[]PayloadRule",
    ),
    (
        "payload.filter",
        "requests.payload.filter",
        "[]PayloadFilterRule",
    ),
    (
        "client.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
        "bool",
    ),
    (
        "client.codex.enable-apply-patch",
        "client.codex.enable-apply-patch",
        "bool",
    ),
    ("proxy-url", "requests.proxy-url", "string"),
    (
        "disable-image-generation",
        "multimedia.disable-image-generation",
        "DisableImageGenerationMode",
    ),
    (
        "gpt-image-2-base-model",
        "multimedia.gpt-image-2-base-model",
        "string",
    ),
    (
        "video-result-auth-cache-ttl",
        "multimedia.video-result-auth-cache-ttl",
        "string",
    ),
    ("force-model-prefix", "routing.force-model-prefix", "bool"),
    ("request-log", "observability.logs.request-log", "bool"),
    (
        "claude-code.disable-cloaking-model-list",
        "upstream.claude.disable-cloaking-model-list",
        "bool",
    ),
    ("api-keys", "access.api-keys", "[]string"),
    (
        "passthrough-headers",
        "requests.passthrough-headers",
        "bool",
    ),
    (
        "streaming.keepalive-seconds",
        "requests.streaming.keepalive-seconds",
        "int",
    ),
    (
        "streaming.bootstrap-retries",
        "requests.streaming.bootstrap-retries",
        "int",
    ),
    (
        "nonstream-keepalive-interval",
        "requests.nonstream-keepalive-interval",
        "int",
    ),
];
const SHARED: &[&str] = &[
    "priority",
    "prefix",
    "proxy-url",
    "headers",
    "models",
    "excluded-models",
    "disable-cooling",
    "request-retry",
    "request-scoped-errors",
];
fn group_keys(entries: Value, family: &str) -> Result<Value, ConfigError> {
    let entries = entries
        .as_array()
        .ok_or_else(|| ConfigError::InvalidConfig(format!("{family} keys must be a list")))?;
    let mut groups = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let mut key = match entry {
            Value::String(key) => {
                let mut map = Map::new();
                map.insert("api-key".into(), Value::String(key.clone()));
                map
            }
            Value::Object(map) => map.clone(),
            _ => return Err(ConfigError::InvalidConfig("key must be a mapping".into())),
        };
        if family == "openai-compatibility" {
            let keys = key
                .remove("api-key-entries")
                .unwrap_or_else(|| Value::Array(Vec::new()));
            key.insert("keys".into(), keys);
            groups.push(Value::Object(key));
            continue;
        }
        let mut group = Map::new();
        group.insert(
            "name".into(),
            Value::String(format!("{family}-{}", index + 1)),
        );
        for field in std::iter::once(&"base-url").chain(SHARED.iter()) {
            if let Some(value) = key.remove(*field) {
                group.insert((*field).into(), value);
            }
        }
        if let Some(Value::String(name)) = key.remove("name") {
            group.insert("name".into(), Value::String(name));
        }
        group.insert("keys".into(), Value::Array(vec![Value::Object(key)]));
        groups.push(Value::Object(group));
    }
    Ok(Value::Array(groups))
}
/// A key written as a bare string is normalized into `{"api-key": ...}` on the way
/// into the v8 tree; expanding it back must restore the bare string, or a restart
/// changes the shape the management API returns for an unchanged setting.
fn collapse_bare_key(mut entry: Map<String, Value>) -> Value {
    if entry
        .get("api-key-entries")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        entry.remove("api-key-entries");
    }
    match (entry.len(), entry.get("api-key")) {
        (1, Some(Value::String(key))) => Value::String(key.clone()),
        _ => Value::Object(entry),
    }
}
fn expand_groups(groups: &Value, family: &str) -> Result<Value, ConfigError> {
    let groups = groups
        .as_array()
        .ok_or_else(|| ConfigError::InvalidConfig(format!("api-keys.{family} must be a list")))?;
    let mut entries = Vec::new();
    for group in groups {
        let group = group
            .as_object()
            .ok_or_else(|| ConfigError::InvalidConfig("group must be a mapping".into()))?;
        if let Some(name) = group.get("name") {
            validate_type(name, "string", "group.name")?;
        }
        let keys = group
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| ConfigError::InvalidConfig("group keys must be a list".into()))?;
        if family == "openai-compatibility" {
            let mut entry = group.clone();
            entry.remove("keys");
            entry.remove("auth_index");
            entry.remove("auth-index");
            entry.insert("api-key-entries".into(), Value::Array(keys.clone()));
            entries.push(collapse_bare_key(entry));
            continue;
        }
        for field in group.keys() {
            if !["name", "base-url", "keys", "auth_index", "auth-index"].contains(&field.as_str())
                && !SHARED.contains(&field.as_str())
            {
                return Err(ConfigError::InvalidConfig(format!(
                    "unsupported group field {field}"
                )));
            }
        }
        for key in keys {
            let key = key
                .as_object()
                .ok_or_else(|| ConfigError::InvalidConfig("key must be a mapping".into()))?;
            let kind = match family {
                "claude" => "ClaudeKey",
                "gemini" | "interactions" => "GeminiKey",
                "vertex" => "VertexCompatKey",
                _ => "CodexKey",
            };
            let mut checked = key.clone();
            checked.remove("auth_index");
            checked.remove("auth-index");
            validate_type(&Value::Object(checked), kind, "group.keys")?;
            if key.contains_key("base-url") {
                return Err(ConfigError::InvalidConfig(
                    "base-url belongs to the group".into(),
                ));
            }
            let mut entry = Map::new();
            for field in std::iter::once(&"base-url").chain(SHARED.iter()) {
                if let Some(value) = group.get(*field) {
                    entry.insert((*field).into(), value.clone());
                }
            }
            for (name, value) in key {
                if name != "auth_index" && name != "auth-index" && !value.is_null() {
                    entry.insert(name.clone(), value.clone());
                }
            }
            entries.push(collapse_bare_key(entry));
        }
    }
    Ok(Value::Array(entries))
}

#[derive(Debug)]
pub enum ConfigError {
    InvalidBody(String),
    InvalidConfig(String),
    InvalidPath,
    NotFound,
    ReadOnly(&'static str),
    Store(SettingsError),
}
impl From<SettingsError> for ConfigError {
    fn from(error: SettingsError) -> Self {
        Self::Store(error)
    }
}
#[derive(Clone, Copy)]
pub enum Mutation {
    Put,
    Patch,
    Delete,
}

pub fn parse(raw: &str) -> Result<Value, ConfigError> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
    let value: Value =
        serde_json::to_value(yaml).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
    if !value.is_object() {
        return Err(ConfigError::InvalidConfig(
            "config must be a mapping".into(),
        ));
    }
    if value
        .get("config-version")
        .is_some_and(|v| v != &Value::from(8))
    {
        return Err(ConfigError::InvalidConfig(
            "unsupported config-version".into(),
        ));
    }
    Ok(value)
}
fn pointer(path: &str) -> String {
    format!(
        "/{}",
        path.split('.')
            .map(|s| s.replace('~', "~0").replace('/', "~1"))
            .collect::<Vec<_>>()
            .join("/")
    )
}
fn node<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    root.pointer(&pointer(path))
}
fn set(root: &mut Value, parts: &[&str], value: Value) -> Result<(), ConfigError> {
    let Some((key, rest)) = parts.split_first() else {
        *root = value;
        return Ok(());
    };
    let map = root.as_object_mut().ok_or(ConfigError::InvalidPath)?;
    if rest.is_empty() {
        map.insert((*key).into(), value);
        return Ok(());
    }
    set(
        map.entry((*key).to_string())
            .or_insert_with(|| Value::Object(Map::new())),
        rest,
        value,
    )
}
fn remove(root: &mut Value, parts: &[&str]) -> bool {
    let Some((key, rest)) = parts.split_first() else {
        return false;
    };
    let Some(map) = root.as_object_mut() else {
        return false;
    };
    if rest.is_empty() {
        return map.remove(*key).is_some();
    }
    let Some(child) = map.get_mut(*key) else {
        return false;
    };
    if !remove(child, rest) {
        return false;
    }
    if child.as_object().is_some_and(Map::is_empty) {
        map.remove(*key);
    }
    true
}
fn merge(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(dst), Value::Object(src)) => {
            for (key, value) in src {
                if let Some(old) = dst.get_mut(&key) {
                    merge(old, value);
                } else {
                    dst.insert(key, value);
                }
            }
        }
        (dst, src) => *dst = src,
    }
}
fn move_path(root: &mut Value, old: &str, new: &str) -> Result<(), ConfigError> {
    if let Some(value) = node(root, old).cloned() {
        remove(root, &old.split('.').collect::<Vec<_>>());
        if node(root, new).is_none() {
            set(root, &new.split('.').collect::<Vec<_>>(), value)?;
        }
    }
    Ok(())
}
fn move_leaves(root: &mut Value, old: &str, new: &str, value: Value) -> Result<(), ConfigError> {
    if let Value::Object(map) = &value {
        if !map.is_empty() {
            for (key, value) in map {
                move_leaves(
                    root,
                    &format!("{old}.{key}"),
                    &format!("{new}.{key}"),
                    value.clone(),
                )?;
            }
            return Ok(());
        }
    }
    if new.split('.').enumerate().skip(1).any(|(end, _)| {
        node(
            root,
            &new.split('.').take(end).collect::<Vec<_>>().join("."),
        )
        .is_some_and(|value| value.is_null() || value.as_object().is_some_and(Map::is_empty))
    }) {
        remove(root, &old.split('.').collect::<Vec<_>>());
        return Ok(());
    }
    move_path(root, old, new)
}
fn normalize_aliases(root: &mut Value) -> Result<(), ConfigError> {
    for (old, new) in &ALIASES[21..] {
        if node(root, old).is_some_and(Value::is_null) {
            for (field, target) in &ALIASES[..21] {
                if field.starts_with(&format!("{old}.")) && node(root, target).is_none() {
                    set(root, &target.split('.').collect::<Vec<_>>(), Value::Null)?;
                }
            }
        }
        if node(root, old).is_some_and(|v| v.is_null() || v.as_object().is_some_and(Map::is_empty))
        {
            remove(root, &old.split('.').collect::<Vec<_>>());
            if node(root, new).is_none() {
                set(
                    root,
                    &new.split('.').collect::<Vec<_>>(),
                    Value::Object(Map::new()),
                )?;
            }
        }
    }
    for (old, new) in &ALIASES[..21] {
        move_path(root, old, new)?;
    }
    Ok(())
}
fn private_ip_alias(root: &mut Value) -> Result<(), ConfigError> {
    let old = "codex.live-media-relay.allow-private-remote-ips";
    let target = "oauth.providers.codex.live-media-relay.disable-private-remote-ips";
    if let Some(value) = node(root, old).cloned() {
        if node(root, target).is_none()
            && node(root, "codex.live-media-relay.disable-private-remote-ips").is_none()
        {
            let allow = value
                .as_bool()
                .ok_or_else(|| ConfigError::InvalidConfig(format!("{old} must be boolean")))?;
            set(
                root,
                &target.split('.').collect::<Vec<_>>(),
                Value::Bool(!allow),
            )?;
        }
        remove(root, &old.split('.').collect::<Vec<_>>());
    }
    Ok(())
}
/// Migrate only present leaves; a present canonical empty container is a reset.
pub fn normalize(mut root: Value) -> Result<Value, ConfigError> {
    private_ip_alias(&mut root)?;
    normalize_aliases(&mut root)?;
    if root
        .get("api-keys")
        .is_some_and(|v| v.is_array() || v.is_null())
    {
        let keys = root
            .get("api-keys")
            .cloned()
            .ok_or(ConfigError::InvalidPath)?;
        remove(&mut root, &["api-keys"]);
        if node(&root, "access.api-keys").is_none() {
            set(&mut root, &["access", "api-keys"], keys)?;
        }
    }
    let canonical = root.clone();
    for (old, new, _) in LEAVES
        .iter()
        .filter(|(old, new, _)| old != new && *old != "api-keys")
    {
        if node(&root, old).is_some() {
            let canonical_present = node(&canonical, new).is_some();
            let blocked = new.split('.').enumerate().skip(1).any(|(end, _)| {
                node(
                    &canonical,
                    &new.split('.').take(end).collect::<Vec<_>>().join("."),
                )
                .is_some_and(|v| v.is_null() || v.as_object().is_some_and(Map::is_empty))
            });
            if canonical_present || blocked {
                remove(&mut root, &old.split('.').collect::<Vec<_>>());
            } else {
                move_path(&mut root, old, new)?;
            }
        }
    }
    for (old, new) in PATHS.iter().filter(|(old, _)| *old != "api-keys") {
        if let Some(value) = node(&root, old).cloned() {
            if node(&canonical, new).is_none() {
                move_leaves(&mut root, old, new, value)?;
            } else {
                remove(&mut root, &old.split('.').collect::<Vec<_>>());
            }
        }
    }
    for family in FAMILIES {
        let old = if *family == "openai-compatibility" {
            (*family).to_string()
        } else {
            format!("{family}-api-key")
        };
        if let Some(entries) = root.get(&old).cloned() {
            remove(&mut root, &[&old]);
            if node(&root, &format!("api-keys.{family}")).is_none() {
                set(
                    &mut root,
                    &["api-keys", family],
                    group_keys(entries, family)?,
                )?;
            }
        }
    }
    set(&mut root, &["config-version"], Value::from(8))?;
    Ok(root)
}
/// Flatten a view for the existing v0-shaped typed runtime. No disk mutation.
pub fn runtime(root: &Value) -> Result<Settings, ConfigError> {
    let mut flat = root.clone();
    let map = flat.as_object_mut().ok_or(ConfigError::InvalidPath)?;
    for key in [
        "config-version",
        "server",
        "management",
        "access",
        "credentials",
        "requests",
        "oauth",
        "observability",
        "multimedia",
        "api-keys",
    ] {
        map.remove(key);
    }
    let defaults = serde_json::to_value(Settings::default())
        .map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
    for (old, new) in PATHS.iter().filter(|(old, _)| !old.contains('.')) {
        if let Some(value) = node(root, new).cloned() {
            let value = if value.is_null() {
                defaults.get(*old).cloned().unwrap_or(Value::Null)
            } else {
                value
            };
            set(&mut flat, &old.split('.').collect::<Vec<_>>(), value)?;
        }
    }
    for field in ["disable-cloaking-model-list", "model-level-cooling"] {
        if let Some(value) = node(root, &format!("upstream.claude.{field}")).cloned() {
            set(&mut flat, &["claude-code", field], value)?;
        }
    }
    if let Some(value) = node(root, "upstream.claude.header-defaults").cloned() {
        set(&mut flat, &["claude-header-defaults"], value)?;
    }
    if let Some(value) = node(root, "upstream.claude.disable-claude-cloak-mode").cloned() {
        set(&mut flat, &["disable-claude-cloak-mode"], value)?;
    }
    for (old, new, _) in LEAVES
        .iter()
        .filter(|(old, new, _)| old != new && *old != "api-keys")
    {
        if let Some(value) = node(root, new).cloned() {
            let parts: Vec<_> = old.split('.').collect();
            if !value.is_null() {
                set(&mut flat, &parts, value)?;
            } else {
                remove(&mut flat, &parts);
            }
        }
    }
    for family in FAMILIES {
        if let Some(value) = node(root, &format!("api-keys.{family}")).cloned() {
            let old = if *family == "openai-compatibility" {
                (*family).to_string()
            } else {
                format!("{family}-api-key")
            };
            set(&mut flat, &[&old], expand_groups(&value, family)?)?;
        }
    }
    for field in [
        "disable-codex-cloaking",
        "stream-bootstrap-buffering",
        "stream-bootstrap-timeout",
        "orphan-delegation-compatibility",
        "model-level-cooling",
        "response-steering",
    ] {
        if let Some(value) = node(root, &format!("upstream.codex.{field}")).cloned() {
            set(&mut flat, &["codex", field], value)?;
        }
    }
    // `claude` and `claude-code` both alias the whole upstream.claude subtree, which
    // also holds keys owned by dedicated top-level fields. Drop those copies (and
    // the v8-only `upstream` root, whose fields were all mapped above from
    // `root`) so they never leak into passthrough `extra` and change the
    // rendered YAML on every load.
    if let Some(map) = flat.as_object_mut() {
        map.remove("upstream");
        for alias in ["claude", "claude-code"] {
            if let Some(section) = map.get_mut(alias).and_then(Value::as_object_mut) {
                section.remove("disable-claude-cloak-mode");
                section.remove("header-defaults");
            }
            if map
                .get(alias)
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty)
            {
                map.remove(alias);
            }
        }
    }
    serde_yaml::from_value(
        serde_yaml::to_value(flat).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?,
    )
    .map_err(|e| ConfigError::InvalidConfig(e.to_string()))
}
pub fn load_runtime(raw: &str) -> Result<Settings, ConfigError> {
    runtime(&normalize(parse(raw)?)?)
}

pub fn redact(mut root: Value) -> Value {
    if let Some(servers) = root
        .pointer_mut(&pointer(ICE))
        .and_then(Value::as_array_mut)
    {
        for server in servers {
            if let Some(map) = server.as_object_mut() {
                map.remove("username");
                map.remove("credential");
            }
        }
    }
    root
}
fn preserve_turn(root: &mut Value, before: &Value) {
    let Some(old) = node(before, ICE).and_then(Value::as_array) else {
        return;
    };
    let Some(next) = root
        .pointer_mut(&pointer(ICE))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let mut matched = vec![false; old.len()];
    for server in next {
        let Some(urls) = server.get("urls") else {
            continue;
        };
        if !urls.is_array() {
            continue;
        }
        if let Some((i, prior)) = old
            .iter()
            .enumerate()
            .find(|(i, prior)| !matched[*i] && prior.get("urls") == Some(urls))
        {
            matched[i] = true;
            if let Some(map) = server.as_object_mut() {
                for name in ["username", "credential"] {
                    if !map.contains_key(name) {
                        if let Some(secret) = prior.get(name) {
                            map.insert(name.into(), secret.clone());
                        }
                    }
                }
            }
        }
    }
}
fn reject_legacy(update: &Value) -> Result<(), ConfigError> {
    for (old, _) in PATHS
        .iter()
        .filter(|(old, _)| !old.contains('.') && *old != "api-keys")
    {
        if update.get(*old).is_some() {
            return Err(ConfigError::InvalidBody(format!("legacy field: {old}")));
        }
    }
    for family in FAMILIES {
        let old = if *family == "openai-compatibility" {
            (*family).to_string()
        } else {
            format!("{family}-api-key")
        };
        if update.get(&old).is_some() {
            return Err(ConfigError::InvalidBody(format!("legacy field: {old}")));
        }
    }
    if update
        .get("api-keys")
        .is_some_and(|value| !value.is_object())
    {
        return Err(ConfigError::InvalidBody(
            "api-keys must be a provider mapping".into(),
        ));
    }
    Ok(())
}
pub fn edit(
    before: &Value,
    path: &str,
    mutation: Mutation,
    update: Option<Value>,
    json: bool,
) -> Result<Value, ConfigError> {
    let path = path.trim_matches('/');
    let alias = path.to_string();
    if alias.contains("//") {
        return Err(ConfigError::InvalidPath);
    }
    let parts: Vec<_> = alias.split('/').filter(|s| !s.is_empty()).collect();
    let mut root = before.clone();
    if !path.is_empty() {
        project_aliases(&mut root, &path.replace('/', "."))?;
    }
    match mutation {
        Mutation::Delete => {
            if parts.is_empty() {
                return Err(ConfigError::InvalidPath);
            }
            if !remove(&mut root, &parts) {
                return Err(ConfigError::NotFound);
            }
        }
        Mutation::Put | Mutation::Patch => {
            let mut update =
                update.ok_or_else(|| ConfigError::InvalidBody("missing body".into()))?;
            if parts.is_empty() {
                if !update.is_object() {
                    return Err(ConfigError::InvalidBody("config must be an object".into()));
                }
                reject_legacy(&update)?;
                normalize_aliases(&mut update)?;
            }
            match mutation {
                Mutation::Patch => {
                    let ptr = if parts.is_empty() {
                        String::new()
                    } else {
                        format!("/{}", parts.join("/"))
                    };
                    if let Some(dst) = root.pointer_mut(&ptr) {
                        merge(dst, update);
                    } else {
                        set(&mut root, &parts, update)?;
                    }
                }
                Mutation::Put => set(&mut root, &parts, update)?,
                Mutation::Delete => unreachable!(),
            }
        }
    }
    normalize_aliases(&mut root)?;
    strip_auth_indexes(&mut root);
    validate(&root)?;
    for field in REVISIONS {
        if node(before, field) != node(&root, field) {
            return Err(ConfigError::ReadOnly(field));
        }
    }
    if json && !matches!(mutation, Mutation::Delete) {
        preserve_turn(&mut root, before);
    }
    runtime(&root)?;
    Ok(root)
}
#[derive(Clone)]
struct YamlEntry {
    path: String,
    head: String,
    key: String,
    spacing: String,
    value: String,
    tail: String,
    body: String,
    indent: usize,
    value_start: usize,
    value_end: usize,
}
fn delimiter(text: &str, wanted: char) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch == wanted && depth == 0 {
            return Some(index);
        }
        match ch {
            '[' | '{' => depth += 1,
            ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}
fn lexical_entries(
    raw: &str,
    path: &str,
    indent: usize,
    entries: &mut Vec<YamlEntry>,
) -> (String, String) {
    lexical_at(raw, path, indent, 0, entries)
}
fn flow_entries(raw: &str, path: &str, base: usize, indent: usize, entries: &mut Vec<YamlEntry>) {
    if !raw.starts_with('{') || !raw.ends_with('}') {
        return;
    }
    let inner = &raw[1..raw.len() - 1];
    let mut start = 0usize;
    while start < inner.len() {
        let remaining = &inner[start..];
        let len = delimiter(remaining, ',').unwrap_or(remaining.len());
        let field = &remaining[..len];
        if let Some(colon) = delimiter(field, ':') {
            let key_raw = field[..colon].trim();
            let key: String = match serde_yaml::from_str(key_raw) {
                Ok(key) => key,
                Err(_) => return,
            };
            let after = &field[colon + 1..];
            let spacing = after.len() - after.trim_start().len();
            let value = after.trim();
            let child = format!("{path}.{key}");
            let value_start = base + 1 + start + colon + 1 + spacing;
            entries.push(YamlEntry {
                path: child.clone(),
                head: String::new(),
                key: key_raw.into(),
                spacing: " ".into(),
                value: value.into(),
                tail: "\n".into(),
                body: String::new(),
                indent,
                value_start,
                value_end: value_start + value.len(),
            });
            flow_entries(value, &child, value_start, indent + 2, entries);
        }
        start += len + 1;
    }
}
fn lexical_at(
    raw: &str,
    path: &str,
    indent: usize,
    base: usize,
    entries: &mut Vec<YamlEntry>,
) -> (String, String) {
    let lines: Vec<_> = raw.split_inclusive('\n').collect();
    let mut starts = Vec::new();
    let mut offset = 0usize;
    for line in &lines {
        let text = line.trim_end_matches(['\r', '\n']);
        let spaces = text.len() - text.trim_start_matches(' ').len();
        let rest = &text[spaces..];
        if spaces == indent && !rest.starts_with(['#', '-']) {
            if let Some(colon) = delimiter(rest, ':') {
                if rest[colon + 1..].is_empty()
                    || rest[colon + 1..].starts_with(char::is_whitespace)
                {
                    starts.push((offset, colon + spaces, text.len(), line.len()));
                }
            }
        }
        offset += line.len();
    }
    if starts.is_empty() {
        return (String::new(), raw.to_string());
    }
    let head = raw[..starts[0].0].to_string();
    for (i, (start, colon, line_len, physical_len)) in starts.iter().copied().enumerate() {
        let end = starts.get(i + 1).map(|s| s.0).unwrap_or(raw.len());
        let line = &raw[start..start + line_len];
        let key_raw = line[indent..colon].trim_end();
        let key: String = serde_yaml::from_str(key_raw).unwrap_or_else(|_| key_raw.to_string());
        let child = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let after = &line[colon + 1..];
        let spacing_len = after.len() - after.trim_start_matches(' ').len();
        let content = &after[spacing_len..];
        let comment = delimiter(content, '#')
            .filter(|index| *index == 0 || content[..*index].ends_with(char::is_whitespace));
        let value_end = comment
            .map(|index| content[..index].trim_end().len())
            .unwrap_or(content.trim_end().len());
        let tail = content[value_end..].to_string() + &raw[start + line_len..start + physical_len];
        let inline = &content[..value_end];
        let body = &raw[start + physical_len..end];
        let value = inline.to_string();
        entries.push(YamlEntry {
            path: child.clone(),
            head: if i == 0 && !path.is_empty() {
                head.clone()
            } else {
                String::new()
            },
            key: key_raw.to_string(),
            spacing: after[..spacing_len].to_string(),
            value: value.clone(),
            tail,
            body: body.to_string(),
            indent,
            value_start: base + start + colon + 1 + spacing_len,
            value_end: base + start + colon + 1 + spacing_len + value_end,
        });
        flow_entries(
            inline,
            &child,
            base + start + colon + 1 + spacing_len,
            indent + 2,
            entries,
        );
        if inline.is_empty() {
            let next_indent = body
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                .map(|line| line.len() - line.trim_start_matches(' ').len())
                .min();
            if let Some(next_indent) = next_indent.filter(|next| *next > indent) {
                lexical_at(
                    body,
                    &child,
                    next_indent,
                    base + start + physical_len,
                    entries,
                );
            }
        }
    }
    (head, String::new())
}
fn source_path(path: &str) -> String {
    if path == "api-keys" {
        return "access.api-keys".into();
    }
    canonical_path(path)
}
fn indent_text(raw: &str, old: usize, new: usize) -> String {
    if old == new {
        return raw.to_string();
    }
    let mut out = String::new();
    for line in raw.split_inclusive('\n') {
        let count = line.len() - line.trim_start_matches(' ').len();
        if line.trim().is_empty() {
            out.push_str(line);
            continue;
        }
        let target = (count as isize + (new as isize - old as isize)).max(0) as usize;
        out.push_str(&" ".repeat(target));
        out.push_str(&line[count..]);
    }
    out
}
fn yaml_value(value: &Value) -> Result<String, ConfigError> {
    Ok(serde_yaml::to_string(value)
        .map_err(|e| ConfigError::InvalidConfig(e.to_string()))?
        .trim_end()
        .to_string())
}
fn scalar_style(value: &Value, old: &str) -> Result<String, ConfigError> {
    if (value.is_array() && old.trim_start().starts_with('['))
        || (value.is_object() && old.trim_start().starts_with('{'))
    {
        return serde_json::to_string(value).map_err(|e| ConfigError::InvalidConfig(e.to_string()));
    }
    if let Some(text) = value.as_str() {
        if old.starts_with('\'') {
            return Ok(format!("'{}'", text.replace('\'', "''")));
        }
        if old.starts_with('"') {
            return serde_json::to_string(text)
                .map_err(|e| ConfigError::InvalidConfig(e.to_string()));
        }
    }
    yaml_value(value)
}
fn render_mapping(
    value: &Value,
    path: &str,
    indent: usize,
    original: &Value,
    entries: &[YamlEntry],
    used: &mut Vec<String>,
) -> Result<String, ConfigError> {
    let map = value.as_object().ok_or(ConfigError::InvalidPath)?;
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort_by_key(|key| {
        let target = if path.is_empty() {
            (*key).clone()
        } else {
            format!("{path}.{key}")
        };
        entries
            .iter()
            .position(|entry| {
                source_path(&entry.path) == target
                    || source_path(&entry.path).starts_with(&format!("{target}."))
            })
            .unwrap_or(usize::MAX)
    });
    let mut output = String::new();
    for key in keys {
        let target = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let value = &map[key];
        let entry = entries
            .iter()
            .find(|entry| entry.path == target)
            .or_else(|| {
                entries
                    .iter()
                    .find(|entry| source_path(&entry.path) == target)
            });
        if let Some(entry) = entry {
            if !entry.head.is_empty() && !used.contains(&entry.path) {
                output.push_str(&indent_text(&entry.head, entry.indent, indent));
            }
            used.push(entry.path.clone());
            let same_key = entry.path.rsplit('.').next() == Some(key.as_str());
            let key_text = if same_key {
                entry.key.clone()
            } else {
                key.clone()
            };
            output.push_str(&" ".repeat(indent));
            output.push_str(&key_text);
            output.push(':');
            let old = node(original, &entry.path);
            if old == Some(value) {
                output.push_str(&entry.spacing);
                output.push_str(&entry.value);
                output.push_str(&entry.tail);
                output.push_str(&indent_text(&entry.body, entry.indent + 2, indent + 2));
                for child in entries
                    .iter()
                    .filter(|child| child.path.starts_with(&format!("{}.", entry.path)))
                {
                    used.push(child.path.clone());
                }
            } else if target.starts_with("api-keys.")
                && value.is_array()
                && old.is_some_and(Value::is_array)
            {
                output.push_str(&entry.spacing);
                output.push_str(&entry.tail);
                if !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&render_groups(
                    value,
                    entry,
                    target.trim_start_matches("api-keys."),
                    indent + 2,
                )?);
            } else if value.as_object().is_some_and(|map| !map.is_empty())
                && !entry.value.trim_start().starts_with('{')
            {
                output.push_str(&entry.spacing);
                output.push_str(&entry.tail);
                if !output.ends_with('\n') {
                    output.push('\n');
                }
                let child_indent = entries
                    .iter()
                    .filter(|child| child.path.starts_with(&format!("{}.", entry.path)))
                    .map(|child| child.indent.saturating_sub(entry.indent))
                    .filter(|width| *width > 0)
                    .min()
                    .unwrap_or(2);
                output.push_str(&render_mapping(
                    value,
                    &target,
                    indent + child_indent,
                    original,
                    entries,
                    used,
                )?);
                for line in entry
                    .body
                    .split_inclusive('\n')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    if !output.ends_with(line) {
                        output.push_str(&indent_text(line, entry.indent, indent));
                    }
                }
            } else {
                output.push_str(if entry.spacing.is_empty() {
                    " "
                } else {
                    &entry.spacing
                });
                let encoded = scalar_style(value, &entry.value)?;
                if encoded.contains('\n') {
                    output.push_str(&entry.tail);
                    if !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str(&indent_text(&encoded, 0, indent + 2));
                    output.push('\n');
                } else {
                    output.push_str(&encoded);
                    output.push_str(&entry.tail);
                }
                for line in entry
                    .body
                    .split_inclusive('\n')
                    .filter(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
                {
                    output.push_str(&indent_text(line, entry.indent, indent));
                }
            }
        } else {
            output.push_str(&" ".repeat(indent));
            output.push_str(key);
            output.push(':');
            if value.as_object().is_some_and(|map| !map.is_empty()) {
                for parent in entries
                    .iter()
                    .filter(|entry| {
                        target == source_path(&entry.path) && !used.contains(&entry.path)
                    })
                    .collect::<Vec<_>>()
                {
                    if !parent.head.is_empty() {
                        output.push_str(&indent_text(&parent.head, parent.indent, indent));
                    }
                    used.push(parent.path.clone());
                }
                output.push('\n');
                output.push_str(&render_mapping(
                    value,
                    &target,
                    indent + 2,
                    original,
                    entries,
                    used,
                )?);
            } else {
                let encoded = yaml_value(value)?;
                if encoded.contains('\n') {
                    output.push('\n');
                    output.push_str(&indent_text(&encoded, 0, indent + 2));
                    output.push('\n');
                } else {
                    output.push(' ');
                    output.push_str(&encoded);
                    output.push('\n');
                }
            }
        }
    }
    Ok(output)
}
fn render_groups(
    value: &Value,
    entry: &YamlEntry,
    family: &str,
    indent: usize,
) -> Result<String, ConfigError> {
    let lines: Vec<_> = entry.body.split_inclusive('\n').collect();
    let source_indent = lines
        .iter()
        .filter(|line| line.trim_start().starts_with("- "))
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min();
    let Some(source_indent) = source_indent else {
        return Ok(indent_text(&yaml_value(value)?, 0, indent) + "\n");
    };
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.len() - line.trim_start_matches(' ').len() == source_indent
                && line.trim_start().starts_with("- ")
        })
        .map(|(index, _)| index)
        .collect();
    let groups = value.as_array().ok_or(ConfigError::InvalidPath)?;
    if groups.len() != starts.len() {
        return Ok(indent_text(&yaml_value(value)?, 0, indent) + "\n");
    }
    let mut output = indent_text(&lines[..starts[0]].concat(), source_indent, indent);
    for (index, start) in starts.iter().copied().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(lines.len());
        let block = lines[start..end].concat();
        let raw = format!(
            "{}{}",
            " ".repeat(source_indent + 2),
            &block[source_indent + 2..]
        );
        let source: Value =
            serde_yaml::from_str(&raw).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
        let mut fields = Vec::new();
        lexical_entries(&raw, "", source_indent + 2, &mut fields);
        let group = &groups[index];
        let migrated = !entry.path.starts_with("api-keys.");
        let text = if migrated && family != "openai-compatibility" {
            let mut common = group.clone();
            common
                .as_object_mut()
                .ok_or(ConfigError::InvalidPath)?
                .remove("keys");
            let mut text =
                render_mapping(&common, "", indent + 2, &source, &fields, &mut Vec::new())?;
            text.push_str(&" ".repeat(indent + 2));
            text.push_str("keys:\n");
            for key in group["keys"].as_array().ok_or(ConfigError::InvalidPath)? {
                let key_text =
                    render_mapping(key, "", indent + 6, &source, &fields, &mut Vec::new())?;
                let first = key_text.trim_start_matches(' ');
                text.push_str(&" ".repeat(indent + 4));
                text.push_str("- ");
                text.push_str(first);
            }
            text
        } else if migrated {
            let mut source = source.clone();
            if let Some(keys) = source
                .as_object_mut()
                .and_then(|map| map.remove("api-key-entries"))
            {
                source
                    .as_object_mut()
                    .ok_or(ConfigError::InvalidPath)?
                    .insert("keys".into(), keys);
            }
            for field in &mut fields {
                if field.path == "api-key-entries" {
                    field.path = "keys".into();
                    field.key = "keys".into();
                }
            }
            render_mapping(group, "", indent + 2, &source, &fields, &mut Vec::new())?
        } else {
            render_mapping(group, "", indent + 2, &source, &fields, &mut Vec::new())?
        };
        output.push_str(&" ".repeat(indent));
        output.push_str("- ");
        output.push_str(text.trim_start_matches(' '));
    }
    Ok(output)
}
/// Reuse original node text and scalar trivia; only changed values are encoded.
pub fn render(root: &Value, original: &str, retain_comments: bool) -> Result<String, ConfigError> {
    if !retain_comments {
        return Ok(yaml_value(root)? + "\n");
    }
    let before = parse(original)?;
    if &before == root {
        return Ok(original.to_string());
    }
    let mut entries = Vec::new();
    let (head, _) = lexical_entries(original, "", 0, &mut entries);
    fn replacements(
        before: &Value,
        after: &Value,
        path: &str,
        entries: &[YamlEntry],
        edits: &mut Vec<(usize, usize, String)>,
    ) -> Result<bool, ConfigError> {
        if before == after {
            return Ok(true);
        }
        match (before, after) {
            (Value::Object(before), Value::Object(after)) if before.keys().eq(after.keys()) => {
                for (key, value) in after {
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    if !replacements(&before[key], value, &child, entries, edits)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (_, after) if !after.is_object() && !after.is_array() => {
                let Some(entry) = entries
                    .iter()
                    .find(|entry| entry.path == path && !entry.value.is_empty())
                else {
                    return Ok(false);
                };
                edits.push((
                    entry.value_start,
                    entry.value_end,
                    scalar_style(after, &entry.value)?,
                ));
                Ok(true)
            }
            _ => Ok(false),
        }
    }
    let mut edits = Vec::new();
    if replacements(&before, root, "", &entries, &mut edits)? {
        let mut output = original.to_string();
        edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        for (start, end, value) in edits {
            output.replace_range(start..end, &value);
        }
        if parse(&output)? == *root {
            return Ok(output);
        }
    }
    let mut used = Vec::new();
    let mut rendered = render_mapping(root, "", 0, &before, &entries, &mut used)?;
    if !head.is_empty() && !rendered.starts_with(&head) {
        rendered = head + &rendered;
    }
    for entry in &entries {
        let target = source_path(&entry.path);
        if used.contains(&entry.path)
            || entries.iter().any(|parent| {
                used.contains(&parent.path)
                    && entry.path.starts_with(&format!("{}.", parent.path))
                    && node(root, &source_path(&parent.path)) == node(&before, &parent.path)
            })
        {
            continue;
        }
        if node(root, &target).is_none()
            && !structural(&target)
            && !local_leaf(&target)
            && v8_type(&target).is_none()
            && !entries.iter().any(|parent| {
                entry.path.starts_with(&format!("{}.", parent.path))
                    && node(root, &source_path(&parent.path)).is_none()
                    && !structural(&source_path(&parent.path))
                    && v8_type(&source_path(&parent.path)).is_none()
            })
        {
            let raw = format!(
                "{}{}:{}{}{}{}",
                entry.head,
                " ".repeat(entry.indent) + &entry.key,
                entry.spacing,
                entry.value,
                entry.tail,
                entry.body
            );
            for line in raw.split_inclusive('\n') {
                rendered.push_str("# ");
                rendered.push_str(line);
            }
        } else if structural(&target) && node(root, &target).is_none() {
            for line in std::iter::once(entry.tail.as_str()).chain(
                entry
                    .body
                    .split_inclusive('\n')
                    .filter(|line| line.trim().is_empty() || line.trim_start().starts_with('#')),
            ) {
                if let Some(index) = delimiter(line, '#') {
                    let comment = &line[index..];
                    if !rendered.contains(comment.trim_end()) {
                        rendered.push_str(comment);
                        if !comment.ends_with('\n') {
                            rendered.push('\n');
                        }
                    }
                }
            }
        }
    }
    if original.contains("\r\n") {
        rendered = rendered.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    let parsed = parse(&rendered)?;
    if &parsed != root {
        return Err(ConfigError::InvalidConfig(
            "lossless YAML edit did not preserve configuration values".into(),
        ));
    }
    Ok(rendered)
}

/// The runtime settings of a v8-shaped document, or `None` for a legacy one.
/// Lets the boot loader read back what a v8 write persisted.
pub fn runtime_of_v8_document(raw: &str) -> Result<Option<Settings>, ConfigError> {
    let original = parse(raw)?;
    if !is_v8_root(&original) {
        return Ok(None);
    }
    let mut root = normalize(original)?;
    archive_unknown(&mut root);
    Ok(Some(runtime(&root)?))
}

fn is_v8_root(original: &Value) -> bool {
    original.get("config-version").is_some()
        || [
            "server",
            "access",
            "management",
            "requests",
            "oauth",
            "observability",
            "credentials",
            "upstream",
        ]
        .iter()
        .any(|key| original.get(*key).is_some())
        || original.get("api-keys").is_some_and(Value::is_object)
        || node(original, "routing.retry").is_some()
}

pub fn update_runtime_document(raw: &str, next: &Settings) -> Result<Option<String>, ConfigError> {
    let original = parse(raw)?;
    let is_v8 = original.get("config-version").is_some()
        || [
            "server",
            "access",
            "management",
            "requests",
            "oauth",
            "observability",
            "credentials",
            "upstream",
        ]
        .iter()
        .any(|key| original.get(*key).is_some())
        || original.get("api-keys").is_some_and(Value::is_object)
        || node(&original, "routing.retry").is_some();
    if !is_v8 {
        return Ok(None);
    }
    let mut root = normalize(original)?;
    archive_unknown(&mut root);
    let before = serde_json::to_value(runtime(&root)?)
        .map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
    let after =
        serde_json::to_value(next).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
    for (key, value) in after.as_object().ok_or(ConfigError::InvalidPath)? {
        if before.get(key) == Some(value) {
            continue;
        }
        let target = PATHS
            .iter()
            .find(|(old, _)| *old == key.as_str())
            .map(|(_, new)| (*new).to_string())
            .unwrap_or_else(|| {
                FAMILIES
                    .iter()
                    .find(|family| {
                        key == &format!("{family}-api-key")
                            || (**family == "openai-compatibility" && key == "openai-compatibility")
                    })
                    .map(|family| format!("api-keys.{family}"))
                    .unwrap_or_else(|| key.clone())
            });
        let value = if target.starts_with("api-keys.") {
            group_keys(value.clone(), target.trim_start_matches("api-keys."))?
        } else {
            value.clone()
        };
        set(&mut root, &target.split('.').collect::<Vec<_>>(), value)?;
    }
    normalize_aliases(&mut root)?;
    Ok(Some(render(&root, raw, true)?))
}

fn schema_fields(kind: &str) -> Option<&'static [(&'static str, &'static str)]> {
    SCHEMA
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, fields)| *fields)
}
fn validate_type(value: &Value, kind: &str, path: &str) -> Result<(), ConfigError> {
    if value.is_null() {
        return Ok(());
    }
    let kind = kind.trim_start_matches('*');
    let invalid = || ConfigError::InvalidConfig(format!("{path} must be {kind}"));
    if let Some(item) = kind.strip_prefix("[]") {
        for (index, value) in value.as_array().ok_or_else(invalid)?.iter().enumerate() {
            validate_type(value, item, &format!("{path}[{index}]"))?;
        }
    } else if let Some(item) = kind.strip_prefix("map[string]") {
        for (key, value) in value.as_object().ok_or_else(invalid)? {
            validate_type(value, item, &format!("{path}.{key}"))?;
        }
    } else if kind == "PluginInstanceConfig" {
        if let Some(map) = value.as_object() {
            for (key, kind) in [("enabled", "bool"), ("priority", "int")] {
                if let Some(value) = map.get(key) {
                    validate_type(value, kind, &format!("{path}.{key}"))?;
                }
            }
        } else if !value.is_boolean() {
            return Err(invalid());
        }
    } else if let Some(fields) = schema_fields(kind) {
        for (key, value) in value.as_object().ok_or_else(invalid)? {
            let kind = fields
                .iter()
                .find(|(field, _)| *field == key)
                .map(|(_, kind)| *kind)
                .ok_or_else(|| ConfigError::InvalidConfig(format!("unknown field {path}.{key}")))?;
            validate_type(value, kind, &format!("{path}.{key}"))?;
        }
    } else {
        let valid = match kind {
            "any" => true,
            "bool" => value.is_boolean(),
            "string" => value.is_string(),
            "int" | "int64" => value.as_i64().is_some(),
            "uint16" => value.as_u64().is_some_and(|v| v <= u64::from(u16::MAX)),
            "time.Duration" => value.as_str().is_some_and(valid_duration),
            "stream-bootstrap-timeout" => valid_bootstrap_timeout(value),
            "DisableImageGenerationMode" => {
                value.is_boolean()
                    || value
                        .as_str()
                        .is_some_and(|v| matches!(v, "chat" | "passthrough"))
            }
            _ => false,
        };
        if !valid {
            return Err(invalid());
        }
    }
    Ok(())
}
fn valid_bootstrap_timeout(value: &Value) -> bool {
    if value.as_i64().is_some_and(|v| v >= 0) {
        return true;
    }
    if let Some(s) = value.as_str() {
        let rest = s
            .strip_suffix("ms")
            .or_else(|| s.strip_suffix('s'))
            .unwrap_or(s);
        return rest.parse::<u64>().is_ok();
    }
    false
}
fn valid_duration(raw: &str) -> bool {
    if raw == "0" {
        return true;
    }
    let raw = raw.trim_start_matches(['+', '-']);
    let mut rest = raw;
    while !rest.is_empty() {
        let number = rest
            .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
            .unwrap_or(rest.len());
        if number == 0 || rest[..number].parse::<f64>().is_err() {
            return false;
        }
        rest = &rest[number..];
        let Some(unit) = ["ns", "us", "µs", "μs", "ms", "s", "m", "h"]
            .iter()
            .find(|unit| rest.starts_with(**unit))
        else {
            return false;
        };
        rest = &rest[unit.len()..];
    }
    !raw.is_empty()
}
fn local_leaf(path: &str) -> bool {
    [
        "model-catalog",
        "warmup",
        "proxy-providers",
        "scoped-api-keys",
        "api-key-bindings",
        "credit-codex-account-ids",
        "codex-fast-mode",
        "logs-segment-size-mb",
    ]
    .iter()
    .any(|p| path == *p || path.starts_with(&format!("{p}.")))
        || path == "routing.strategy"
        || [
            "session-affinity-lcp",
            "lcp-max-turns",
            "lcp-max-groups",
            "lcp-max-prefixes",
        ]
        .iter()
        .any(|p| path == format!("routing.{p}"))
}
fn v8_type(path: &str) -> Option<&'static str> {
    if structural(path) {
        return None;
    }
    LEAVES
        .iter()
        .find(|(_, current, _)| *current == path)
        .map(|(_, _, kind)| *kind)
        .or_else(|| {
            ALIASES
                .iter()
                .find(|(_, target)| *target == path)
                .map(|(old, _)| {
                    if old.contains("header-defaults") {
                        if old.ends_with("stabilize-device-profile") {
                            "*bool"
                        } else {
                            "string"
                        }
                    } else {
                        "bool"
                    }
                })
        })
        .or(match path {
            "upstream.claude.disable-claude-cloak-mode" => Some("bool"),
            "upstream.claude.disable-cloaking-model-list" => Some("bool"),
            "client.codex.optimize-multi-agent-v2" => Some("bool"),
            _ => None,
        })
}
fn structural(path: &str) -> bool {
    LEAVES
        .iter()
        .any(|(_, current, _)| current.starts_with(&format!("{path}.")))
        || ALIASES
            .iter()
            .any(|(_, current)| current.starts_with(&format!("{path}.")))
}
fn validate_tree(value: &Value, path: &str) -> Result<(), ConfigError> {
    if local_leaf(path) {
        return Ok(());
    }
    if path == "api-keys" && value.is_null() {
        return Ok(());
    }
    if path == "config-version" {
        return if value == &Value::from(8) {
            Ok(())
        } else {
            Err(ConfigError::InvalidConfig(
                "unsupported config-version".into(),
            ))
        };
    }
    if path == "api-keys" {
        for (family, groups) in value
            .as_object()
            .ok_or_else(|| ConfigError::InvalidConfig("api-keys must be a mapping".into()))?
        {
            let kind = match family.as_str() {
                "codex" | "xai" | "meta" => "CodexKey",
                "gemini" | "interactions" => "GeminiKey",
                "claude" => "ClaudeKey",
                "vertex" => "VertexCompatKey",
                "openai-compatibility" => "OpenAICompatibility",
                _ => {
                    return Err(ConfigError::InvalidConfig(format!(
                        "unknown API-key provider {family}"
                    )))
                }
            };
            let expanded = expand_groups(groups, family)?;
            validate_type(
                &expanded,
                &format!("[]{kind}"),
                &format!("api-keys.{family}"),
            )?;
            for group in groups.as_array().ok_or(ConfigError::InvalidPath)? {
                for key in group
                    .get("keys")
                    .and_then(Value::as_array)
                    .ok_or(ConfigError::InvalidPath)?
                {
                    if let Some(weight) = key.get("weight") {
                        if !weight.is_null() && weight.as_i64().is_none_or(|v| v > 1_000_000) {
                            return Err(ConfigError::InvalidConfig(
                                "weight must be an integer <= 1000000".into(),
                            ));
                        }
                    }
                }
            }
        }
        return Ok(());
    }
    if let Some(kind) = v8_type(path) {
        return validate_type(value, kind, path);
    }
    if path.is_empty() || structural(path) {
        if value.is_null() && !path.is_empty() {
            return Ok(());
        }
        for (key, value) in value
            .as_object()
            .ok_or_else(|| ConfigError::InvalidConfig(format!("{path} must be a mapping")))?
        {
            validate_tree(
                value,
                &if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                },
            )?;
        }
        return Ok(());
    }
    Err(ConfigError::InvalidConfig(format!(
        "unknown v8 field {path}"
    )))
}
pub fn validate(root: &Value) -> Result<(), ConfigError> {
    validate_tree(root, "")?;
    if let Some(models) = root.get("models").and_then(Value::as_object) {
        for (key, value) in models {
            if let Some(source) = value
                .as_str()
                .filter(|v| !v.is_empty() && !std::path::Path::new(v).is_absolute())
            {
                let url = reqwest::Url::parse(source).map_err(|_| {
                    ConfigError::InvalidConfig(format!(
                        "models.{key} must be an http(s) URL or absolute path"
                    ))
                })?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(ConfigError::InvalidConfig(format!(
                        "models.{key} must be an http(s) URL or absolute path"
                    )));
                }
            }
        }
    }
    Ok(())
}
pub fn archive_unknown(root: &mut Value) {
    fn walk(value: &mut Value, path: &str) {
        if v8_type(path).is_some() || local_leaf(path) || path == "api-keys" {
            return;
        }
        if let Some(map) = value.as_object_mut() {
            map.retain(|key, value| {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if child != "config-version"
                    && !structural(&child)
                    && v8_type(&child).is_none()
                    && !local_leaf(&child)
                    && child != "api-keys"
                {
                    return false;
                }
                walk(value, &child);
                true
            });
        }
    }
    walk(root, "");
}
fn canonical_path(path: &str) -> String {
    let mut mappings: Vec<_> = ALIASES.iter().chain(PATHS.iter()).collect();
    mappings.sort_by_key(|(old, _)| std::cmp::Reverse(old.len()));
    for (old, new) in mappings {
        if path == *old || path.starts_with(&format!("{old}.")) {
            return format!("{new}{}", &path[old.len()..]);
        }
    }
    path.to_string()
}
pub fn project_aliases(root: &mut Value, path: &str) -> Result<(), ConfigError> {
    for (old, new) in ALIASES {
        if path == *old
            || old.starts_with(&format!("{path}."))
            || path.starts_with(&format!("{old}."))
        {
            if let Some(value) = node(root, new).cloned() {
                if value.as_object().is_some_and(|map| !map.is_empty()) {
                    continue;
                }
                set(root, &old.split('.').collect::<Vec<_>>(), value)?;
                remove(root, &new.split('.').collect::<Vec<_>>());
            }
        }
    }
    Ok(())
}
fn strip_auth_indexes(root: &mut Value) {
    if let Some(groups) = root.get_mut("api-keys").and_then(Value::as_object_mut) {
        for groups in groups.values_mut().filter_map(Value::as_array_mut) {
            for group in groups {
                if let Some(map) = group.as_object_mut() {
                    map.remove("auth_index");
                    map.remove("auth-index");
                    if let Some(keys) = map.get_mut("keys").and_then(Value::as_array_mut) {
                        for key in keys {
                            if let Some(map) = key.as_object_mut() {
                                map.remove("auth_index");
                                map.remove("auth-index");
                            }
                        }
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn present_zero_false_and_empty_win_over_legacy() {
        let input = json!({"request-retry":3,"debug":true,"api-keys":["old"],
            "routing":{"retry":{"request-retry":0}}, "observability":{"logs":{"debug":false}},
            "access":{"api-keys":[]}});
        let settings = runtime(&normalize(input).expect("normalize")).expect("runtime");
        assert_eq!(settings.request_retry, 0);
        assert!(!settings.debug);
        assert!(settings.api_keys.is_empty());
    }

    #[test]
    fn provider_groups_expand_null_overrides_from_group() {
        let root = json!({"api-keys":{"codex":[{"name":"group","base-url":"http://127.0.0.1:18841","request-retry":3,
            "keys":[{"api-key":"a","request-retry":null},{"api-key":"b","request-retry":0}]}]}});
        let settings = runtime(&root).expect("decode");
        assert_eq!(settings.codex_api_key[0].retry_for_key("a"), Some(Some(3)));
        assert_eq!(settings.codex_api_key[1].retry_for_key("b"), Some(Some(0)));
    }

    #[test]
    fn explicit_null_does_not_reactivate_legacy_true() {
        let root = normalize(json!({"debug":true,"observability":{"logs":{"debug":null}}}))
            .expect("normalize");
        assert!(!runtime(&root).expect("runtime").debug);
        assert!(node(&root, "observability.logs.debug")
            .expect("present")
            .is_null());
    }

    #[test]
    fn patch_merges_maps_and_replaces_lists_retaining_null() {
        let before = json!({"plugins":{"configs":{"p":{"keep":1,"list":[1,2],"override":3}}}});
        let after = edit(
            &before,
            "plugins/configs/p",
            Mutation::Patch,
            Some(json!({"list":[],"override":null})),
            true,
        )
        .expect("patch");
        assert_eq!(
            after["plugins"]["configs"]["p"],
            json!({"keep":1,"list":[],"override":null})
        );
    }

    #[test]
    fn delete_prunes_only_removed_ancestors_and_rejects_indexes() {
        let before =
            json!({"plugins":{"configs":{"p":{"opaque":{},"list":[1],"branch":{"leaf":1}}}}});
        let after = edit(
            &before,
            "plugins/configs/p/branch/leaf",
            Mutation::Delete,
            None,
            true,
        )
        .expect("delete");
        assert!(after["plugins"]["configs"]["p"].get("branch").is_none());
        assert_eq!(after["plugins"]["configs"]["p"]["opaque"], json!({}));
        assert!(matches!(
            edit(
                &before,
                "plugins/configs/p/list/0",
                Mutation::Put,
                Some(json!(2)),
                true
            ),
            Err(ConfigError::InvalidPath)
        ));
    }

    #[test]
    fn home_revisions_cannot_be_mutated_or_removed() {
        for field in REVISIONS {
            let mut before = json!({});
            set(&mut before, &field.split('.').collect::<Vec<_>>(), json!(7)).expect("fixture");
            assert!(matches!(
                edit(
                    &before,
                    &field.replace('.', "/"),
                    Mutation::Put,
                    Some(json!(8)),
                    true
                ),
                Err(ConfigError::ReadOnly(_))
            ));
            assert!(matches!(
                edit(&before, "", Mutation::Put, Some(json!({})), true),
                Err(ConfigError::ReadOnly(_))
            ));
        }
    }

    #[test]
    fn turn_json_roundtrip_preserves_secrets_by_urls_not_position() {
        let before = json!({"oauth":{"providers":{"codex":{"live-media-relay":{"ice-servers":[
            {"urls":["turn:a"],"username":"u","credential":"s"}, {"urls":["turn:b"],"credential":"b"}
        ]}}}}});
        let redacted = redact(before.clone());
        assert!(node(&redacted, ICE).expect("ice")[0]
            .get("credential")
            .is_none());
        let after = edit(&before, &ICE.replace('.',"/"), Mutation::Put,
            Some(json!([{"urls":["turn:b"]},{"urls":["turn:c"]},{"urls":["turn:a"],"credential":""}])),true).expect("write");
        let servers = node(&after, ICE).expect("ice");
        assert_eq!(servers[0]["credential"], "b");
        assert!(servers[1].get("credential").is_none());
        assert_eq!(servers[2]["credential"], "");
    }

    #[test]
    fn migration_is_idempotent_and_keeps_unrelated_nodes() {
        let before = json!({"request-retry":3,"extension":{"opaque":[]}});
        let once = normalize(before).expect("normalize");
        let twice = normalize(once.clone()).expect("normalize");
        assert_eq!(once, twice);
        assert_eq!(twice["extension"], json!({"opaque":[]}));
        assert_eq!(twice["config-version"], 8);
    }

    #[test]
    fn invalid_typed_values_are_rejected_before_persistence() {
        assert!(matches!(
            load_runtime("server: {port: invalid}"),
            Err(ConfigError::InvalidConfig(_))
        ));
        assert!(matches!(
            edit(
                &json!({}),
                "",
                Mutation::Patch,
                Some(json!({"request-retry":0})),
                true
            ),
            Err(ConfigError::InvalidBody(_))
        ));
        assert!(edit(
            &json!({"config-version":8}),
            "",
            Mutation::Put,
            Some(json!({"config-version":8,"api-keys":[]})),
            false,
        )
        .is_err());
    }

    #[test]
    fn scalar_save_keeps_exact_comment_position_quotes_and_crlf() {
        let raw = "# head\r\nconfig-version: 8\r\nserver: {port: 18840, host: '127.0.0.1'} # inline\r\n# foot\r\n";
        let mut root = parse(raw).expect("parse");
        root["server"]["port"] = json!(18842);
        let saved = render(&root, raw, true).expect("render");
        assert_eq!(saved, raw.replace("18840", "18842"));
    }

    #[test]
    fn migrated_leaf_carries_inline_comment_and_keeps_unrelated_style() {
        let raw =
            "# document\nrequest-retry: 3 # retry\nserver:\n  host: '127.0.0.1' # host\n# foot\n";
        let root = normalize(parse(raw).expect("parse")).expect("normalize");
        let saved = render(&root, raw, true).expect("render");
        assert!(saved.contains("    request-retry: 3 # retry\n"));
        assert!(saved.contains("server:\n  host: '127.0.0.1' # host\n# foot\n"));
        assert_eq!(parse(&saved).expect("parse"), root);
        assert_eq!(render(&root, &saved, true).expect("second save"), saved);
    }

    #[test]
    fn mixed_structs_migrate_missing_leaves_without_overwriting_present_values() {
        let input = json!({"tls":{"enable":true,"cert":"legacy-cert","key":"legacy-key"},
            "server":{"tls":{"enable":false,"cert":""}},
            "codex":{"response-steering":true,"live-media-relay":{"enabled":true}},
            "upstream":{"codex":{"response-steering":false}},
            "oauth":{"providers":{"codex":{"live-media-relay":{"max-sessions":0}}}}});
        let root = normalize(input).expect("normalize");
        assert_eq!(
            root["server"]["tls"],
            json!({"enable":false,"cert":"","key":"legacy-key"})
        );
        assert_eq!(root["upstream"]["codex"]["response-steering"], false);
        assert_eq!(
            root["oauth"]["providers"]["codex"]["live-media-relay"],
            json!({"enabled":true,"max-sessions":0})
        );
        assert!(root.get("tls").is_none() && root.get("codex").is_none());
    }

    #[test]
    fn all_historical_aliases_project_and_update_canonical_leaves() {
        for (old, target) in &ALIASES[..21] {
            let value =
                if old.contains("header-defaults") && !old.ends_with("stabilize-device-profile") {
                    json!("baseline")
                } else if old.contains("stream-bootstrap-timeout") {
                    json!("15s")
                } else {
                    json!(false)
                };
            let mut before = json!({});
            set(
                &mut before,
                &target.split('.').collect::<Vec<_>>(),
                value.clone(),
            )
            .expect("fixture");
            let after = edit(
                &before,
                &old.replace('.', "/"),
                Mutation::Put,
                Some(value.clone()),
                true,
            )
            .expect("alias put");
            assert_eq!(node(&after, target), Some(&value));
            assert!(node(&after, old).is_none());
        }
    }

    #[test]
    fn private_ip_alias_inverts_and_present_canonical_false_wins() {
        let root = normalize(json!({"codex":{"live-media-relay":{"allow-private-remote-ips":true}},
            "oauth":{"providers":{"codex":{"live-media-relay":{"disable-private-remote-ips":false}}}}})).expect("normalize");
        assert_eq!(
            node(
                &root,
                "oauth.providers.codex.live-media-relay.disable-private-remote-ips"
            ),
            Some(&json!(false))
        );
        assert!(node(
            &root,
            "oauth.providers.codex.live-media-relay.allow-private-remote-ips"
        )
        .is_none());
    }

    #[test]
    fn strict_schema_rejects_unknown_fields_and_invalid_provider_shapes() {
        for invalid in [
            json!({"server":{"typo":true}}),
            json!({"oauth":{"providers":{"codex":{"live-media-relay":{"udp-port-min":65536}}}}}),
            json!({"api-keys":{"unknown":[]}}),
            json!({"api-keys":{"codex":[{"keys":[{"api-key":"fixture","weight":1000001}]}]}}),
            json!({"models":{"catalog":"relative.json"}}),
            json!({"plugins":{"configs":{"p":{"enabled":"invalid"}}}}),
        ] {
            assert!(validate(&invalid).is_err(), "invalid fixture: {invalid}");
        }
        validate(&json!({"plugins":{"configs":{"p":{"enabled":false,"opaque":{"free":[]}}}},"requests":{"payload":{"override":[{"models":[{"name":"*"}],"params":{"opaque.key":null}}]}}})).expect("opaque user maps accepted");
    }

    #[test]
    fn unknown_legacy_nodes_are_archived_once_without_losing_comments() {
        let raw =
            "request-retry: 3\n# extension head\nextension: {opaque: true} # extension inline\n";
        let mut root = normalize(parse(raw).expect("parse")).expect("normalize");
        archive_unknown(&mut root);
        let saved = render(&root, raw, true).expect("render");
        assert!(saved.contains("# extension: {opaque: true} # extension inline\n"));
        assert!(parse(&saved).expect("parse").get("extension").is_none());
        assert_eq!(render(&root, &saved, true).expect("repeat"), saved);
    }
}
