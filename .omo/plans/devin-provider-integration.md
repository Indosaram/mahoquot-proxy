# mahoquot-proxy Devin 제공자 구현 계획

작성일: 2026-09-11  
상태: 분석 및 구현 계획 완료. 제품 코드 구현과 실계정 호출은 수행하지 않음.

## 1. 결론

**Devin을 네이티브 LLM 제공자로 추가할 수 있다. `dsh-plugin-devin-bridge`의 Connect RPC 계약을 참고하여 Rust 어댑터를 구현하는 방식을 권장한다. `devin-sse-proxy`는 MCP 도구 연결용이므로 LLM 제공자의 전송 계층으로 사용하지 않는다.**

우리 프록시에는 이미 `reqwest`, `prost`, Cursor Connect 처리, 제공자별 계정 풀, 모델 레지스트리, 공통 스트림 이벤트와 응답 렌더러가 있다. 별도 Python/Node 서버를 띄우는 대신 이 구조에 Devin 계정 및 프로토콜 분기를 추가한다.

다만 “URL과 토큰만 추가”하는 작업은 아니다. Connect의 HTTP 200 내부 오류, protobuf 요청 본문에도 들어가는 인증 토큰, 계정별 모델 권한, Responses API의 현재 원문 전달 경로까지 처리해야 한다. 이 문서는 인증, 모델 라우팅, 요청·응답, 스트리밍, 사용량, UI, 테스트를 모두 구현 범위에 포함한다.

### 확인 수준

- **소스로 확인:** 두 참조 저장소의 인증·요청·응답 코드, 로컬 프록시의 실제 변경 지점.
- **공식 문서로 확인:** Devin CLI 인증 파일 위치와 기본 비만료 특성, Devin MCP의 현재 인증 방식과 `/sse` 폐기, Connect 프레이밍 규칙.
- **미검증:** 사용자 계정의 Devin CLI 접근 권한, 현재 실서버에서 참조 protobuf가 그대로 동작하는지, 계정별 모델·이미지·출력 한도, 토큰 사용량과 실제 청구 금액의 관계.
- 따라서 판단은 **구현 가능성 확인**이며, **실계정 연동 성공 확인**은 아니다.

## 2. 분석 기준과 저장소 소유권

| 대상 | 분석 기준 |
| --- | --- |
| `Arborsm/dsh-plugin-devin-bridge` | `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4` |
| `sotayamashita/devin-sse-proxy` | `7672e2ddf6eaf4aac7bda8f74071e804e7efbadd` |
| 프록시 | `/Users/indo/code/project/mahoquot-proxy`, HEAD `49478507d47e7feb5e1baa619172b1599b1e316d` 및 분석 당시 작업 트리 |
| 데스크톱·콘솔 | `/Users/indo/code/project/quotio-rs`, HEAD `bce1c4aca3320177a682bf272dbe33057f6f96a6` |

`quotio-rs`와 `mahoquot`는 이 환경에서 모두 `/Volumes/T9-Mac/project/mahoquot`로 해석된다. 서로 다른 구현 대상으로 취급하지 않는다. `quotio-desktop`은 참고용 업스트림이며 우리 콘솔의 수정 대상이 아니다.

프록시에는 분석 이전부터 `account.rs`, `relay.rs`, `management/creds.rs`, `quota.rs`, `usage.rs`, providers의 갱신 코드, `ui/index.html` 등의 미커밋 변경이 있었다. 아래 줄 번호는 분석 시점의 위치다. 구현 시작 시 해당 파일을 다시 읽고 작업 소유권을 확인하며, 기존 변경을 되돌리거나 stash하지 않는다.

인증 저장 위치는 실행 중인 gateway의 설정된 auth 디렉터리를 사용한다. 과거 메모의 `~/.cli-proxy-api`와 계약 문서의 `~/.mahoquot/auth` 중 하나를 추정으로 하드코딩하지 않는다.

## 3. 두 저장소 비교

| 구분 | dsh-plugin-devin-bridge | devin-sse-proxy |
| --- | --- | --- |
| 목적 | dsh 에이전트가 Devin 호스팅 모델을 LLM으로 호출 | MCP 클라이언트의 JSON-RPC를 Devin 도구 서버로 전달 |
| 업스트림 | `https://server.codeium.com` | 기본 `https://mcp.devin.ai/sse` |
| 프로토콜 | Connect RPC, protobuf, 서버 스트리밍 | stdio JSON-RPC + SSE GET + HTTP POST |
| 인증 | CLI session token, 특수 Basic 헤더와 protobuf metadata | API key를 Bearer 헤더로 전달 |
| 주요 메서드 | `GetChatMessage`, `GetCascadeModelConfigs` | MCP 요청을 변경 없이 전달 |
| 결과 | 텍스트, reasoning, tool call, token usage | MCP 도구 응답 |
| 우리 구현에서 역할 | 주된 wire 계약 참고 | MCP와 모델 제공자 구분, 인증 전달·세션 처리 참고 |
| 라이선스 | MIT, Copyright 2026 Arborsm | MIT, Copyright 2025 Sam Yamashita |

### 3.1 dsh 플러그인에서 확인한 계약

주요 파일:

- `src/adapter/transport.ts`: Connect client와 인증 헤더.
- `src/adapter/credentials.ts`: `credentials.toml`에서 `windsurf_api_key`, `api_server_url` 읽기.
- `src/adapter/devin.ts`: 모델 발견, 요청 메타데이터, 메시지·도구·이미지 변환.
- `src/adapter/decoder.ts`: 응답 delta, usage, stop reason 처리.
- `src/proto/devin.proto`: 필드 번호가 포함된 축약 protobuf 정의.
- `src/index.ts`: 실제 기본 모델, 설정, 자격증명 해석과 캐시.

인증은 일반적인 HTTP Basic과 다르다.

```text
Authorization: Basic <token>-<token>
```

여기서 `<token>`은 참조 구현의 `devin-session-token$...` 값이다. `username:password`를 base64 인코딩하는 `basic_auth()`를 쓰면 다른 헤더가 된다. 동일 토큰이 protobuf `metadata.api_key`에도 들어간다. MCP용 `cog_...` 키와 교환 가능한 것으로 취급하지 않는다.

RPC 경로:

```text
POST /exa.api_server_pb.ApiServerService/GetChatMessage
POST /exa.api_server_pb.ApiServerService/GetCascadeModelConfigs
```

첫 번째는 서버 스트리밍, 두 번째는 unary이다. 둘 다 같은 인증을 사용하지만 본문 프레이밍과 Content-Type은 다르다.

### 3.2 README를 그대로 구현 사양으로 삼으면 안 되는 부분

1. **모델 캐시:** README는 5분 TTL과 동적 모델 병합을 설명하지만, 현재 `listModels()`는 설정 모델을 반환하고 `fetchModelCatalog()`는 직접 RPC를 호출한다. 해당 파일에 5분 모델 캐시는 없다. 5분 TTL을 채택한다면 우리 설계 결정이다.
2. **모델 한도:** README 예시는 128,000/16,384이지만 `src/index.ts`의 GLM 기본값은 context/output 모두 200,000, SWE 기본값은 모두 262,000이다. `fetchModelCatalog()`도 `max_tokens`를 context와 output에 동시에 넣는다. 이 값을 실제 최대 출력으로 확정하지 않는다.
3. **모델 정체·가격:** SWE의 기반 모델 설명, promo, credit multiplier는 제3자 코드의 값이다. 공식 모델 계보·현재 무료 이용·실제 가격의 증거로 사용하지 않는다.
4. **메시지 손실:** 과거 이미지가 `[Image omitted from history]`로 바뀌며, system prompt가 정규식으로 재작성되고 도구 schema annotation도 제거된다. 우리 프록시에서는 기본적으로 원문 의미를 보존한다.
5. **tool call 손실:** 디코더는 잘못된 최종 arguments를 `{}`로 바꾼다. ID 없는 delta는 마지막 도구에 붙인다. 손상된 호출을 정상 호출로 위장할 수 있으므로 그대로 포팅하지 않는다.
6. **reasoning:** proto에는 signature/redaction 필드가 있으나 dsh 디코더는 이를 충분히 보존하지 않는다. 우리 공통 이벤트 및 렌더러까지 보존 여부를 검증해야 한다.
7. **취소·갱신:** `getChatMessage()`에 요청 signal이 전달되지 않고, CLI 자격증명 캐시는 파일 교체를 자동 감지하지 않는다. 우리 구현은 요청 취소와 명시적 재가져오기를 별도로 설계한다.

### 3.3 MCP 프록시의 현재 유효 범위

`main.py`는 SSE의 `endpoint` 이벤트를 받아 `urljoin()`으로 POST 주소를 바꾸고, POST 응답의 `Mcp-Session-Id`를 이후 요청에 붙인다. Bearer 인증을 GET과 POST에 모두 적용한다.

하지만 현재 공식 Devin 문서는 **Streamable HTTP `/mcp` 사용을 권장하고 `/sse`를 deprecated로 명시**한다. 현재 MCP 키는 `cog_` 계열이며 enterprise key/PAT에는 `X-Org-Id`가 필요하다. 참조 스크립트에는 해당 조직 헤더 옵션이 없다.

추가로 스크립트는 청크마다 UTF-8을 `errors="ignore"`로 디코딩하고, endpoint의 다른 origin을 제한하지 않으며, POST 실패를 로그로만 남긴다. GET 응답에서의 세션 헤더 수집도 보이지 않는다. 이는 코드상 제약이며 실환경 장애 재현 결과는 아니다.

결론적으로 MCP가 별도로 필요해져도 이 legacy SSE 서버를 신규 LLM 경로에 붙이지 않는다. Devin 원격 에이전트 세션 생성·관리도 이번 모델 제공자와 다른 기능이다.

## 4. 권장 아키텍처

```text
클라이언트
  OpenAI Chat / Responses / Anthropic Messages / Gemini
        |
        v
기존 인증, scoped key, 모델 Registry, 계정 선택
        |
        v
ProviderKind::Devin + 해당 계정의 모델 허용 목록
        |
        v
Devin 요청 변환 -> protobuf -> Connect 서버 스트리밍 POST
        |
        v
Connect frame parser -> DevinDecoder -> CodexEvent
        |
        +-> 기존 Chat / Anthropic / Gemini 렌더러
        +-> Responses용 변환 추가
        +-> non-stream 집계, usage/history, 계정 상태
```

- `CodexEvent`라는 기존 이름을 유지한다. 이 작업을 공통 이벤트 전면 개명 작업으로 확대하지 않는다.
- Devin은 Cursor의 bidirectional heartbeat, KV 응답, exec 응답 채널을 사용하지 않는다.
- Rust `reqwest`와 기존 `prost`를 사용한다. TS SDK와 Python 프로세스는 제품 의존성으로 추가하지 않는다.
- provider-specific decoder는 분리한다. 공유하는 것은 검증된 Connect envelope 처리뿐이며, Cursor의 관대한 오류 처리까지 재사용하지 않는다.
- registry crate의 순수 도메인 경계를 유지한다. 네트워크·토큰·파일 I/O를 registry에 넣지 않는다.
- 기본 host는 `server.codeium.com`, HTTP/1.1을 명시한다. 기존 outbound proxy/TLS 정책을 사용하고 인증서 검증을 끄지 않는다.

대안인 sidecar는 빠른 일회성 비교 실험에는 가능하지만 계정 풀·장애 상태·프로세스 관리가 이중화되므로 최종 구현안에서 제외한다. `generic + openai-chat`도 본문이 protobuf가 아니므로 설정만으로 해결되지 않는다.

## 5. 현재 코드의 구체적인 변경 지점

아래 경로는 별도 표시가 없으면 프록시 저장소 기준이다. “추가”는 아직 없는 제안 파일이다.

| 영역 | 현재 근거 / 변경 대상 | 구현 내용 |
| --- | --- | --- |
| provider ID | `crates/registry/src/lib.rs:58`, `ProviderId` | `devin()` 추가. `codeium`/`windsurf` 전체를 Devin으로 강제 별칭 처리하지 않음 |
| 모델 SST | `crates/registry/catalog/models-v1.json` | Devin provider policy와 명시적 public/upstream ID binding |
| 자격증명 | 추가 `crates/providers/src/devin.rs`; `crates/providers/src/lib.rs` | 계정 schema, TOML 읽기, token 검증, endpoint 상수, redacted Debug |
| 계정 풀 | `crates/gateway/src/account.rs:16`, `:27`, `:71` | `ProviderKind`/`ProviderAccount` variant, 모든 match, 로딩·identity·header·refresh 정책 |
| 관리 API | `crates/gateway/src/management/creds.rs:291` | validator, import, 중복 판정, 토큰 교체, 삭제·disable·rescan |
| 모델 합성 | `crates/gateway/src/runtime_state.rs:14`, `registry_with_account_contributions` | Devin discovered contribution과 계정별 허용 목록을 같은 generation에 반영 |
| 관리 모델 조회 | `crates/gateway/src/management/registry.rs`, `crates/gateway/src/registry/` | signed 전역 catalog와 계정별 Devin discovery를 구분 |
| 요청 계획 | `crates/gateway/src/relay.rs:384`, `resolve_target` | Devin RPC target와 protobuf body 생성 |
| 실제 전송 | `crates/gateway/src/relay.rs:655`, `send_upstream` | provider header 우선권, HTTP/1.1, 일반 one-shot request body |
| 입력 정상화 | `crates/gateway/src/relay.rs:957`, `build_plan` | Responses 원본 입력, stream flag, 토큰 한도 보존 |
| 라우팅 | `crates/gateway/src/relay.rs:1057`, `member_provider_id`; `:1180`, `resolve_route` | Devin binding 선택, 계정별 모델 권한, scoped key와 모델 제외 |
| URL | `crates/gateway/src/url.rs`, `build_provider_url` | Devin base URL과 RPC 경로 결합 |
| protobuf | 추가 `crates/gateway/src/compat/devin_proto.rs`, `devin.proto` | 축약 schema와 prost 타입, 필드 번호·proto2 presence 보존 |
| Connect 디코더 | 추가 `crates/gateway/src/compat/devin.rs` | 요청 변환, frame/parser, stop reason, 도구 delta, usage |
| 프로토콜 합류 | `crates/gateway/src/compat/mod.rs:124`, `:154` | `Protocol::Devin`, `ProtocolParser`, binary stream 허용 |
| 공통 이벤트 | `crates/gateway/src/compat/events.rs:3` | 기존 text/reasoning/signature/tools/usage 활용, redaction 등 필요한 최소 확장 |
| 응답 표면 | `crates/gateway/src/compat/render.rs`, `compat/claude.rs`, `compat/gemini.rs`; 추가 `compat/responses.rs` | Chat·Messages·Gemini 보존 확인, Responses 입력/렌더링 구현 |
| 종료 처리 | `crates/gateway/src/relay.rs:1421`, `finish_success` | Devin을 Native raw passthrough에서 제외, 프로토콜 완료 후 성공 판정 |
| 사용량·상태 | `usage.rs`, `quota.rs`, `telemetry.rs`, `request_history.rs` | 토큰 usage와 구독 quota 분리, Connect 오류와 종료 상태 기록 |
| 계약·회귀 | `tests/t20_reference_parity.rs`, `tests/data/reference-parity.json`, `docs/reference-parity.md` | 기존 항목을 삭제하지 않고 Devin 추가분의 인증/adapter/test owner 기록 |

### 반드시 처리할 현재 구조의 함정

1. `send_upstream()`은 계정 헤더를 넣은 뒤 전달받은 Content-Type을 추가한다. Devin wire에는 `application/connect+proto`가 정확히 한 번 들어가도록 최종 header 구성을 검증한다. 클라이언트의 JSON Content-Type이나 inbound API key가 upstream 인증을 덮지 못하게 한다.
2. `open_stream()`은 현재 Kiro/Cursor만 binary 예외다. Devin을 추가하지 않으면 정상 protobuf를 “not an event stream”으로 거부한다.
3. `build_plan(Native)`는 `client_stream=true`, `openai_body=None`이고 `finish_success()`는 Native를 원문 전달한다. `/v1/responses`에 Devin binary가 새지 않도록 별도 변환이 필수다.
4. `finish_success()`에는 HTTP 응답을 받은 시점의 성공 기록이 있다. Connect의 마지막 error envelope가 HTTP 200이어도 요청 실패·계정 상태에 반영되게 Devin 완료 신호를 연결한다.
5. `ModelCapability::Image`는 이미지 생성 표면에서도 사용된다. `supports_images=true`를 곧바로 이 capability로 매핑하면 생성 API까지 잘못 열린다. vision 입력 지원은 별도 metadata로 표현하고 채팅 입력에서 검증한다.
6. `AccountUsage`는 quota window·잔액·관측 시각 모델이다. 요청별 input/output token은 `compat::events::Usage`와 telemetry/history에 기록한다. 모델 목록을 quota 값으로 해석하지 않는다.

## 6. 구현 계약

### 6.1 인증과 계정 생명주기

초기 지원 인증 방법은 두 가지다.

1. Devin CLI session token 수동 입력.
2. **프록시가 실행되는 Mac**의 Devin CLI 자격증명 가져오기.

공식 문서의 `devin auth login`을 안내하되, 확인되지 않은 OAuth URL·callback·refresh endpoint를 만들어내지 않는다. 공식 enterprise 인증 문서는 Enterprise 및 `Use Devin CLI` 권한을 요구한다. 이를 모든 개인 계정에서 사용 가능하다고 안내하지 않는다.

정규화된 저장 형식 제안:

```json
{
  "type": "devin",
  "identity_slug": "devin-work",
  "label": "Devin Work",
  "access_token": "<devin-session-token>",
  "api_server_url": "https://server.codeium.com",
  "disabled": false
}
```

- 필수: provider type, 비어 있지 않은 안정적 identity, 검증된 session token.
- `email`, `refresh_token`, 임의의 만료 시각을 요구하지 않는다. token을 JWT라고 가정하지 않는다.
- CLI 필드 `windsurf_api_key`를 내부 `access_token`으로 정규화한다. 경로 우선순위는 명시된 `DEVIN_CREDENTIALS_PATH`, `XDG_DATA_HOME`, 기본 `~/.local/share/devin/credentials.toml`이다.
- TOML은 정식 parser를 사용한다. 의존성은 구현 당시 workspace에 있는 것을 먼저 확인한다. 참조의 줄 단위 scanner를 복제하지 않는다.
- 원본 CLI 파일은 수정·삭제하지 않는다. 가져온 결과만 기존 atomic writer로 gateway auth 디렉터리에 저장한다.
- 제안 관리 API: 기존 auth-files 업로드를 수동 입력에 재사용하고, `POST /v0/management/devin/import-cli`를 추가한다. 입력은 label/identity만 받고 웹 요청으로 임의 파일 경로를 읽지 않는다.
- 만료·폐기 시 UI를 “재인증/다시 가져오기 필요”로 전환한다. 자동 OAuth refresh는 지원하지 않는 것으로 명시하고, 기존 refresh fallback으로 Codex 등의 token endpoint를 호출하지 않는다.
- 토큰 교체는 같은 identity를 유지하며 새 요청부터 적용한다. 요청 body의 metadata와 header는 **같은 계정 credential snapshot**으로 만든다.
- 자격증명 변경 시 discovery cache를 무효화하고 계정 상태를 재평가한다. 삭제·disable·재시작·rescan에도 identity와 카운터 정책을 유지한다.
- 토큰은 header뿐 아니라 protobuf body에도 있으므로 debug, HTTP dump, 오류 본문, request history에서 모두 제외한다. URL 변경 시 인증을 타 origin으로 redirect하지 않는다.

### 6.2 Connect 전송

| 항목 | GetChatMessage | GetCascadeModelConfigs |
| --- | --- | --- |
| Content-Type | `application/connect+proto` | `application/proto` |
| body | `flags(1) + length(4, BE) + protobuf` | framing 없는 protobuf |
| 응답 | Connect envelope 스트림 | framing 없는 protobuf |
| 공통 | `Connect-Protocol-Version: 1`, 계정 인증, bounded timeout | 동일 |

- 일반 data flag는 `0x00`, EndStreamResponse는 `0x02`이며 **마지막 payload는 JSON**이다. gRPC-Web의 `0x80` trailer와 혼동하지 않는다.
- 기본 압축은 identity로 협상한다. 협상하지 않은 compressed frame은 명시적 오류로 처리한다. gzip을 추가하려면 frame별 압축과 압축 해제 후 크기 제한을 함께 테스트한다.
- 헤더·payload가 여러 TCP chunk로 쪼개지거나 여러 frame이 합쳐지는 경우를 처리한다.
- 제안 초기 제한: 단일 response frame 최대 16 MiB. 초과 길이, 잘린 EOF, 잘못된 protobuf/JSON, 중복 terminal은 성공으로 끝내지 않는다.
- `te: trailers`는 Connect 자체 필수 헤더가 아니다. Cursor의 헤더를 통째로 복사하지 않는다.
- 취소되면 upstream body가 drop되고 in-flight 계수도 정리되어야 한다. heartbeat·영구 재연결 task는 만들지 않는다.
- 모든 버퍼는 bounded로 둔다. streaming은 전체 응답을 모으지 않으며 non-stream은 기존 응답 제한 안에서 집계한다.

### 6.3 요청 변환

참조 proto의 주요 필드:

| 입력 의미 | protobuf 매핑 |
| --- | --- |
| system/developer instructions | `GetChatMessageRequest.prompt = 2` |
| 대화 기록 | `chat_message_prompts = 3` |
| 생성 옵션 | `configuration = 8` |
| 도구 정의 | `tools = 10` |
| 모델의 실제 UID | `chat_model_uid = 21` |
| 메타데이터 | `metadata = 1`, 내부 `api_key = 3` |

참조 코드상 메시지 source 숫자는 user=1, assistant history=2(`SYSTEM`이라는 enum 이름), tool result=4다. 이 이름 때문에 assistant를 최상위 system prompt로 합치지 않는다.

- 텍스트·역할 순서, tool call ID/name/JSON arguments, tool result/error, reasoning과 signature를 보존한다.
- system prompt 재작성, 모델 정체 문구 제거, schema description 삭제는 기본 동작에 넣지 않는다.
- 참조 metadata의 `chisel`, `3000.2.17`, `os=win`, 366바이트 random fingerprint, trajectory/cascade/execution UUID는 서버 호환성 확인용 기준값이다. 실제 wire 비교 후 필요한 필드만 확정하고 host OS와 wire metadata를 구분한다.
- 매 요청의 ID 생성은 I/O 계층에서 하고 변환 함수에는 주입해 deterministic fixture를 만든다.
- `max_tokens`/`max_completion_tokens`/Responses `max_output_tokens`를 의미에 맞게 보존한다. Codex 전용 normalizer가 출력 제한을 제거한 body를 Devin 입력으로 재사용하지 않는다.
- `temperature`, `top_p`는 전달하고 `n>1`, 강제 tool choice, strict structured output 등 매핑 근거가 없는 옵션은 명시적으로 거부한다. `tool_choice=none`은 도구를 전달하지 않는 방식으로 지원하고 `auto`는 기본 동작으로 처리한다.
- 이미지는 확인된 vision 모델에만 제공한다. 초기 지원은 base64/data URL이며 remote image URL은 명시적 미지원 오류로 처리한다. 과거 이미지를 조용히 생략하지 않는다. 전체 이미지 history 지원 여부는 실제 wire fixture로 판정한다.
- audio/video/image generation/embedding/realtime/countTokens는 LLM 텍스트·vision 입력과 별개이며 검증 전에는 capability를 열지 않는다.

### 6.4 모델 발견과 라우팅

- public model ID는 `devin/<정확한 model_uid>`로 한다. 예: `devin/glm-5-2`, `devin/swe-1-7`. upstream에는 접두어 없는 원래 UID를 전달한다.
- `-max`, `-none`, `-medium`, `-1m` 등을 잘라 합치지 않는다. UID 자체로 variant를 보존한다.
- `reasoning_effort` 매핑은 discovery로 확인된 variant에만 허용한다. 없으면 base model로 몰래 fallback하지 말고 지원하지 않는 effort를 오류로 반환한다.
- `disabled` 모델은 제외한다. premium/promo/capacity limited/credit multiplier는 원본 관측 metadata이며 권한·가격 확정값이 아니다.
- `max_tokens`의 의미가 확인되기 전 context와 output limit 양쪽에 복제하지 않는다. 기본 출력 요청은 보수적인 4,096을 제안하며, 이 수치를 서버 최대 한도라고 표시하지 않는다.
- Devin provider는 `Discovered` 정책으로 계정이 관측한 모델만 추가한다. 실패 시 기존 정상 계정별 목록은 stale 표시로 유지하되 최초 discovery 실패에는 가짜 모델을 라우팅 가능 상태로 만들지 않는다.
- discovery cache key는 계정 identity, credential revision, base URL이다. **제안 TTL은 5분**이고 로그인/import·명시적 새로고침·stale 목록 조회에서 비동기 갱신한다. 주기 daemon이나 매 추론 요청의 동기 RPC는 추가하지 않는다.
- 제안 관리 API: `POST /v0/management/devin/models/refresh`와 계정별 결과 조회. 기존 `/model-registry`는 signed catalog refresh이므로 Devin discovery와 혼동하지 않는다.
- 전역 모델 목록은 계정별 목록의 합집합이지만 계정 선택은 해당 모델을 제공하는 계정으로 제한한다. 한 계정의 premium 모델을 모든 Devin 계정이 제공한다고 추정하지 않는다.
- 계정·모델·binding·capability는 기존 `PoolSnapshot`의 단일 generation으로 발행한다. 변경 도중 새 모델과 옛 계정의 조합이 보이지 않아야 한다.
- `devin/*`가 미등록 상태일 때 Codex의 open fallback으로 흘러가지 않게 `serves_model`뿐 아니라 `resolve_model`의 unknown-model 경로도 막는다.
- signed catalog, exclusions, scoped key, account pinning, unsupported model cache를 우회하지 않는다.

### 6.5 응답·오류·사용량

| upstream | 내부 처리 |
| --- | --- |
| `delta_text` | `TextDelta` |
| `delta_thinking` | `ReasoningDelta` |
| `delta_signature` | `ReasoningSignature`, 필요한 opaque 보존 |
| `thinking_redacted` | redacted 상태를 명시적으로 보존하는 최소 이벤트 확장 |
| `delta_tool_calls` | call별 고정 output index, `ToolCallBegin`/`ToolArgsDelta` |
| stop reason 3 / 1 / 9 | output limit/incomplete 의미를 구분해 표면별 종료 |
| stop reason 10 | tool-call 종료 |
| stop reason 13 | 실패 |
| EndStreamResponse error | HTTP 200이어도 실패 |
| usage | input/output/cache read/cache write, 마지막 authoritative snapshot |

- ID 없는 tool delta는 활성 호출이 하나라서 모호하지 않을 때만 연결한다. 여러 호출 사이에서 ID가 생략되어 식별할 수 없으면 protocol error다.
- 함수 이름과 ID가 완성되기 전에 잘못된 `ToolCallBegin`을 발행하지 않는다. malformed 최종 arguments를 `{}`로 고치지 않는다.
- 정상 EndStreamResponse를 확인하기 전에 terminal success를 발행하지 않는다. 이후 사용량 frame이 있을 수 있으므로 stop reason만으로 스트림을 조기 종료하지 않는다.
- usage 누락을 실제 0-token 사용으로 표시하지 않는다. 반복된 누적 usage는 더하지 않고 마지막 값을 사용하며, total은 input+output으로 계산하고 cache를 다시 더하지 않는다.
- `actual_model_uid`가 요청 UID와 다르면 관측 정보에 남긴다. 클라이언트 public model 이름을 임의 교체하거나 잘못된 가격표를 적용하지 않는다.
- `unauthenticated`는 재인증 필요, `permission_denied`는 계정/모델 접근 거부, `resource_exhausted`는 기존 cooldown 정책, `unavailable`은 transient 실패로 분류한다. permission denied를 전부 token 만료로 취급하지 않는다.
- HTTP 오류뿐 아니라 Connect JSON의 `error.code`를 보존한다. 기존 `Failed { message }`만으로 부족한 계정 피드백은 Devin 전용 typed outcome으로 연결한다.
- 첫 downstream header/byte 이전의 확실한 거절에 한해서 기존 retry budget을 적용한다. timeout이나 연결 단절처럼 처리 여부가 모호한 요청은 자동 재전송하지 않는다. downstream commit 이후에는 계정 전환·재시도를 절대 하지 않는다.
- 사전 오류를 판별할 때 bounded 첫 유효 frame까지만 확인한다. 스트리밍 전체를 버퍼링하지 않는다. 중간 실패는 해당 표면의 error/failed event로 종료하고 성공 terminal을 추가하지 않는다.
- quota endpoint는 두 저장소에서 확인되지 않았다. 초기 quota는 **알 수 없음**으로 표시하며 0%, 무제한, 무료라고 표현하지 않는다. ACU/credit 비용과 token 개수는 별개다.

### 6.6 지원할 클라이언트 표면

| 표면 | 완료 조건 |
| --- | --- |
| `/v1/chat/completions` | stream/non-stream, tools, usage, reasoning |
| `/v1/responses` | input/instructions/tools 변환, `stream` 준수, response/item/content/tool argument 이벤트와 JSON 집계 |
| `/v1/messages` | text/thinking/signature/tool_use/tool_result와 stop reason 보존 |
| 기존 Gemini generate/stream 표면 | 동일 Devin 모델 선택, text/tool/usage, 지원하지 않는 input의 명시적 거부 |
| legacy completions | 기존 표면이 Devin 모델을 허용하면 text completion shape 유지 |
| 이미지 생성·임베딩·실시간 등 | capability 검사로 upstream 호출 없이 거부 |

Responses는 순수 client-side 대화 기록을 포함한 요청을 우선 계약으로 한다. 서버 저장소가 필요한 `previous_response_id`, background/store 기능은 근거 없이 지원한다고 광고하지 않고 명시적으로 거부한다. 공개 endpoint alias도 같은 처리를 거쳐야 한다.

### 6.7 콘솔 UI

소스 루트: `quotio-rs/crates/monitor-ui/frontend`.

- `src/lib/provider-catalog.ts`: `devin`, `authKind: local`, 전용 adapter identity.
- `src/lib/onboarding.ts`: “세션 토큰 입력”, “프록시 호스트 CLI에서 가져오기”.
- `src/App.tsx`, 기존 온보딩 component: 제출·성공·오류·재가져오기, label 기반 계정 표시.
- `src/lib/api.ts`, `src/lib/schemas.ts`: 새 관리 API와 타입 검증.
- `src/lib/accounts.ts`: email 없는 계정의 identity, 중복 방지, 정렬.
- `src/components/ProviderGlyph.tsx`: 기존 component를 재사용하고 검증된 브랜드 자산을 추가한다.
- 잔여 quota는 unknown, 모델 목록은 discovery 상태, 인증 실패는 재인증 필요로 구분한다.
- 원격 gateway에서는 “이 브라우저의 파일”이 아니라 “프록시 호스트의 파일”을 읽는다는 점을 분명히 한다.
- 산출물은 기존 `build` 후 `sync:proxy`로 `mahoquot-proxy/ui/index.html`에 반영한다. HTML을 직접 수정하지 않는다.

## 7. 실행 순서와 검증 기준

각 단계는 실패하는 테스트를 먼저 확보하고 최소 구현으로 통과시킨다. 아래 테스트 파일명 중 `devin_*`는 새로 만들 제안이다. 이 계획 자체는 커밋·배포를 승인하지 않는다.

| 단계 | 선행 조건 | 산출물 | 통과 기준 |
| --- | --- | --- | --- |
| P0. wire 기준 확정 | 없음 | commit 고정 proto, 인증/모델 응답 fixture, 미검증 항목 기록 | unary와 streaming framing 구분, 실제 검증이 없으면 experimental 유지 |
| P1. 계정·인증 | P0 | `providers/devin.rs`, 계정 enum·검증·import | `devin_credentials` 테스트: 파싱, 비밀값 비노출, atomic 교체, ID 유지, 잘못된 token 거부 |
| P2. wire codec | P0 | proto/prost, frame decoder, 요청 builder | `devin_wire` 테스트: 고정 바이너리 fixture와 양방향 비교, 모든 청크 분할 통과 |
| P3. Chat 릴레이 | P1 + P2 | target/send/parser/Chat renderer 연결 | `devin_relay` 테스트와 실제 gateway HTTP 요청으로 stream/non-stream·취소·오류 확인 |
| P4. 모델·계정 라우팅 | P1 + P2 | discovery, TTL, snapshot 합성, account filter | `devin_catalog` 테스트: 다계정 권한·동시 refresh·Codex 누출 방지·signed catalog 공존 |
| P5. 나머지 API 표면 | P3 + P4 | Responses 입력/렌더러, Messages/Gemini 통합 | `devin_surfaces` 테스트: 각 표면의 2-turn tool 왕복, reasoning, usage, terminal 정확성 |
| P6. 관리·관측·UI | P1 + P4, 최종 검증은 P5 | 온보딩·재가져오기·usage/history·quota unknown | 계정 생성/수정/삭제/disable/재시작, UI 실제 관리 API 연결, desktop/mobile 화면 QA |
| P7. 출시 판정 | P0~P6 | 전체 회귀 결과와 smoke 증거 | 아래 인수 조건 전부 충족, 실서버 미검증이면 그 상태를 유지 |

P1과 P2는 독립 병렬 실행이 가능하다. P3과 P4도 독립 파일 중심으로 병렬화할 수 있다. `relay.rs`, `account.rs`, `runtime_state.rs`, `App.tsx`를 동시에 다른 작업자가 편집하지 않도록 통합 소유자를 하나로 둔다.

### 필수 회귀 시나리오

1. CLI TOML 없음/손상/누락 key/정상 파일, env 경로 우선순위, token 교체, 원본 파일 불변.
2. literal Basic header와 protobuf metadata의 동일 토큰, 잘못된 base64 Basic 방지, Content-Type 중복·덮어쓰기 방지.
3. Connect header 1~4바이트 분할, payload 모든 경계 분할, 여러 frame 병합, 한글 UTF-8, 빈 delta, 알 수 없는 proto 필드.
4. oversized frame, truncated EOF, 잘못된 protobuf, code만 있는 EndStream error, 정상 stop 뒤 terminal error, 중복 terminal.
5. reasoning/text 전환, signature/redaction, 두 tool call의 교차 delta, 이름/ID 지연, malformed arguments.
6. non-stream과 stream의 최종 text/tool/usage 동등성, cache token 중복 집계 방지, usage 없는 응답.
7. HTTP 401/403/429/5xx와 HTTP 200 안의 같은 Connect 오류, 출력 전 거절과 출력 후 실패의 서로 다른 retry 정책.
8. 클라이언트 취소 후 upstream 연결 및 in-flight 종료. oneshot/Notify/연결 close 신호를 사용하고 고정 sleep에 의존하지 않는다.
9. Devin 계정이 없거나 disabled일 때 `devin/*`가 Codex/Generic으로 전달되지 않음.
10. 서로 다른 모델 권한의 두 계정, explicit account pinning, scoped key, excluded 모델, credential 교체와 discovery refresh 경합.
11. Chat/Responses/Messages/Gemini의 user -> tool request -> tool result -> final answer 왕복.
12. vision 미지원 모델·remote URL·강제 tool choice·`n>1`·미지원 API는 upstream 미호출과 명확한 오류.
13. request history·로그·UI·fixture·오류에 session token과 protobuf 인증 본문이 없음.
14. import부터 `/v1/models`, 실제 추론, 삭제까지 **실제 gateway endpoint + local mock upstream**을 이용한 E2E. 관리 API를 전부 UI mock으로 대체하지 않는다.

mock은 독립적으로 만든 golden wire fixture를 사용한다. 구현의 encoder를 그대로 mock decoder의 유일한 정답으로 쓰는 자기검증 테스트는 피한다.

### 구현 시 실행할 명령

```bash
# /Users/indo/code/project/mahoquot-proxy
cargo fmt --all --check
cargo test -p mahoquot-registry
cargo test -p mahoquot-providers
cargo test -p mahoquot-gateway
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace

# /Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend
bun run typecheck
bun run lint
bun run test
bun run build
bun run test:e2e
bun run sync:proxy
```

실행 전 LSP 진단을 확인하고, 현존 테스트 실패는 변경 전후를 구분한다. UI는 desktop 및 mobile viewport에서 실제 화면을 확인하고 공식 자산·폼 overflow·비밀값 노출을 검사한다. `sync:proxy`는 현재 다른 작업자가 변경 중인 산출물을 덮을 수 있으므로 통합 시점에만 실행한다.

실계정 smoke는 Devin CLI 접근이 준비된 계정으로 모델 발견과 소량 텍스트·tool 왕복을 별도로 수행한다. 이 계획 작성 중에는 실행하지 않았다. 접근 권한이 없으면 mock 통과를 실연동 성공으로 보고하지 않는다.

## 8. 인수 조건과 남은 불확실성

### 최종 인수 조건

- Devin 계정을 콘솔에서 등록하고 같은 identity로 재가져올 수 있다.
- `/v1/models`와 실제 선택 가능한 계정의 모델 권한이 일치한다.
- 지원 표면의 stream/non-stream, reasoning, tools, usage가 mock E2E에서 동작한다.
- HTTP 200 Connect 오류가 성공으로 기록되거나 정상 완료로 렌더링되지 않는다.
- token 교체·disable·삭제·재시작·취소·cooldown에 계정 풀과 history가 맞게 반응한다.
- 실측되지 않은 quota/모델 한도/비용은 unknown 또는 출처가 있는 관측값으로 표시한다.
- 기존 Codex/Cursor/Kiro/Claude/Vertex/Generic 경로의 회귀 테스트가 유지된다.
- 실서버 검증 결과를 별도 표시한다. 미검증 상태에서 stable 제공자로 승격하지 않는다.

### 구현 첫 단계에서 확인할 것

| 불확실성 | 확인 방법 | 확인되지 않을 때 |
| --- | --- | --- |
| 사용자 계정의 CLI 접근 | 공식 CLI 로그인과 해당 계정의 model discovery | 인증 기능을 사용 가능하다고 단정하지 않음 |
| 축약 proto의 현재 호환성 | 고정 참조 fixture와 승인된 실계정 RPC 비교 | experimental 유지, 필드/metadata 차이 기록 |
| 모델 context와 output 제한 | 공식 모델별 문서 또는 실제 계정 응답·제한 오류 | 최대값을 꾸며내지 않고 unknown |
| tool delta의 ID 생략 규칙 | 여러 도구를 요청한 실제 wire fixture | 모호한 병합을 거부 |
| signature/redacted reasoning 재전송 | 2-turn reasoning/tool fixture | 지원 범위를 축소 표시하고 opaque 데이터 보존 |
| 과거 이미지 허용 범위 | 이미지 포함 다중 turn fixture | 조용히 생략하지 않고 명시적 미지원 |
| quota/청구 조회 API | CLI 또는 공식 API의 검증된 계약 | token usage만 제공하고 quota unknown |

## 9. 출처

### 고정된 참조 코드

- [dsh-plugin-devin-bridge 전체](https://github.com/Arborsm/dsh-plugin-devin-bridge/tree/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4)
- [인증 transport](https://github.com/Arborsm/dsh-plugin-devin-bridge/blob/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4/src/adapter/transport.ts)
- [CLI credentials](https://github.com/Arborsm/dsh-plugin-devin-bridge/blob/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4/src/adapter/credentials.ts)
- [요청·모델 변환](https://github.com/Arborsm/dsh-plugin-devin-bridge/blob/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4/src/adapter/devin.ts)
- [응답 디코더](https://github.com/Arborsm/dsh-plugin-devin-bridge/blob/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4/src/adapter/decoder.ts)
- [protobuf](https://github.com/Arborsm/dsh-plugin-devin-bridge/blob/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4/src/proto/devin.proto)
- [설정·기본 모델](https://github.com/Arborsm/dsh-plugin-devin-bridge/blob/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4/src/index.ts)
- [devin-sse-proxy main.py](https://github.com/sotayamashita/devin-sse-proxy/blob/7672e2ddf6eaf4aac7bda8f74071e804e7efbadd/main.py)
- [bridge MIT license](https://github.com/Arborsm/dsh-plugin-devin-bridge/blob/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4/LICENSE)
- [SSE proxy MIT license](https://github.com/sotayamashita/devin-sse-proxy/blob/7672e2ddf6eaf4aac7bda8f74071e804e7efbadd/LICENSE)

### 공식 문서

- [Devin CLI enterprise authentication](https://docs.devin.ai/cli/enterprise/devin-auth)
- [Devin MCP: 현재 인증·도구·Streamable HTTP](https://docs.devin.ai/work-with-devin/devin-mcp)
- [Connect protocol: unary, envelopes, EndStreamResponse, 오류 코드](https://connectrpc.com/docs/protocol/)

참조 코드의 주석에는 `devin-2api` 등 다른 프로젝트에서의 번역 출처도 있다. 실제 코드·schema를 가져오면 위 MIT 고지를 보존하고 복사하는 부분의 추가 출처를 확인한다. 이번 계획은 그 프로젝트들까지 분석했다고 주장하지 않는다.

## 10. 이번 작업에서 수행한 검증

- 두 저장소의 README와 핵심 소스를 읽고 commit을 고정했다.
- 공식 Devin·Connect 문서와 제3자 구현을 구분해 대조했다.
- 로컬 provider enum, credential validator, 레지스트리, 릴레이, binary stream gate, 공통 이벤트, 실제 콘솔 소스와 빌드 스크립트를 확인했다.
- LSP로 실제 `resolve_target`, `send_upstream`, `build_plan`, `finish_success` 등 릴레이 심볼을 확인했다.
- 제품 코드·자격증명·운영 설정은 변경하지 않았다. 빌드·제품 테스트·실계정 Devin 호출은 실행하지 않았다.
- 분석 결과물은 이 마크다운 계획서다. 구현 단계의 성공 증거와 혼동하지 않는다.
