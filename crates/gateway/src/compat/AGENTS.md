# PROTOCOL COMPATIBILITY SUBSYSTEM

## OVERVIEW
Bi-directional translation layer converting between OpenAI, Anthropic Claude, and Google Gemini request and response schemas, including SSE streaming event formats.

## STRUCTURE
```
crates/gateway/src/compat/
├── mod.rs        # Compatibility layer interface & schema exports
├── request.rs    # Inbound payload parsing & target schema transformation
├── events.rs     # SSE event stream normalization & parser
├── render.rs     # Response body & streaming SSE payload rendering
├── claude.rs     # Anthropic Messages format translation adapter
└── gemini.rs     # Google Gemini format translation adapter
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Claude Messages translation | `crates/gateway/src/compat/claude.rs` | Translates between Anthropic and OpenAI schemas |
| Gemini format adaptation | `crates/gateway/src/compat/gemini.rs` | Translates between Gemini and OpenAI schemas |
| Stream rendering & SSE events | `crates/gateway/src/compat/render.rs` | Emits protocol-compliant SSE delta frames |
| Request normalizer | `crates/gateway/src/compat/request.rs` | Parses common model invocation parameters |

## CONVENTIONS
- **Pure Transformations**: Compatibility functions should remain pure and deterministic where possible, separating format transformation from network I/O.
- **Field Passthrough**: Unknown top-level client fields should be safely ignored or mapped to standard fallback parameters without breaking deserialization.

## ANTI-PATTERNS (THIS SUBTREE)
- **DO NOT** drop role markers or message ordering during conversation history transformation.
- **DO NOT** panic on unexpected or malformed tool call payloads; yield structured translation errors.
- **NEVER** re-encode SSE events without preserving the exact framing delimiters (`data: ...\n\n`).
