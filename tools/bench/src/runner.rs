use crate::stats::{compute_report, format_summary_line, BenchReport, StatsInput};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

const DEFAULT_BODY: &str = r#"{"model":"bench","stream":true}"#;

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub url: String,
    pub concurrency: usize,
    pub total: usize,
    pub out: PathBuf,
    pub headers: Vec<String>,
    pub body_json: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug)]
pub enum SampleResult {
    Success {
        ttft_ms: f64,
        ttfb_ms: f64,
        completion_ms: f64,
        semantic: bool,
    },
    Error {
        kind: String,
    },
}

/// Splits the benchmark-only `bench_expect` block off the request body: the
/// expectation drives the semantic gate and never reaches the target.
pub fn split_expectation(body: &str) -> (String, Option<ExpectedOutput>) {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (body.to_string(), None);
    };
    let Some(expect) = value.as_object_mut().and_then(|o| o.remove("bench_expect")) else {
        return (body.to_string(), None);
    };
    let expected = ExpectedOutput {
        text: expect["text"].as_str().unwrap_or_default().to_string(),
        input_tokens: expect["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: expect["output_tokens"].as_u64().unwrap_or(0),
        cached_tokens: expect["cached_tokens"].as_u64(),
        tool_arguments: expect["tool_arguments"].as_str().map(str::to_string),
    };
    if let Some(object) = value.as_object_mut() {
        object
            .entry("stream")
            .or_insert(serde_json::Value::Bool(true));
    }
    (value.to_string(), Some(expected))
}

pub fn parse_headers(
    raw_headers: &[String],
) -> Result<HeaderMap, Box<dyn std::error::Error + Send + Sync>> {
    let mut map = HeaderMap::new();
    for h in raw_headers {
        let (k, v) = h
            .split_once(':')
            .ok_or_else(|| format!("invalid header format (expected 'Key: Value'): {h}"))?;
        let name = HeaderName::from_bytes(k.trim().as_bytes())?;
        let val = HeaderValue::from_str(v.trim())?;
        map.append(name, val);
    }
    if !map.contains_key(CONTENT_TYPE) {
        map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    Ok(map)
}

fn map_reqwest_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "io:timeout".to_string()
    } else if err.is_connect() {
        "io:connect".to_string()
    } else if err.is_body() || err.is_decode() {
        "io:body".to_string()
    } else {
        "io:error".to_string()
    }
}

pub fn validate_bench_url(raw: &str) -> Result<(), String> {
    let url =
        reqwest::Url::parse(raw).map_err(|error| format!("invalid benchmark URL: {error}"))?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url
            .port()
            .is_none_or(|port| !(18840..=18899).contains(&port))
    {
        return Err("benchmark URL must use loopback HTTP and a port in 18840-18899".into());
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ExpectedOutput {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: Option<u64>,
    pub tool_arguments: Option<String>,
}

#[derive(Debug, Default)]
pub struct SemanticGate {
    buffer: Vec<u8>,
    text: String,
    tool_arguments: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    terminal: bool,
    done_marker: bool,
}

impl SemanticGate {
    pub fn content_len(&self) -> usize {
        self.text.len() + self.tool_arguments.len()
    }

    pub fn feed(&mut self, bytes: &[u8], _elapsed_ms: f64) -> Result<(), String> {
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let frame: Vec<u8> = self.buffer.drain(..end + 2).collect();
            let frame = std::str::from_utf8(&frame)
                .map_err(|error| format!("SSE frame is not UTF-8: {error}"))?;
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                // OpenAI ends with a finish_reason chunk and then [DONE]; a
                // second [DONE] is the duplicate.
                if self.done_marker {
                    return Err("duplicate SSE terminal marker".into());
                }
                self.done_marker = true;
                self.terminal = true;
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&data)
                .map_err(|error| format!("invalid SSE JSON: {error}"))?;
            if value.get("error").is_some() || value["type"] == "error" {
                return Err("SSE error event".into());
            }
            if value["object"] == "response"
                || value["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("response."))
            {
                match value["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" => self
                        .text
                        .push_str(value["delta"].as_str().unwrap_or_default()),
                    "response.function_call_arguments.delta" => self
                        .tool_arguments
                        .push_str(value["delta"].as_str().unwrap_or_default()),
                    "response.completed" => {
                        if self.terminal {
                            return Err("duplicate SSE terminal event".into());
                        }
                        self.terminal = true;
                        self.input_tokens = value
                            .pointer("/response/usage/input_tokens")
                            .and_then(serde_json::Value::as_u64);
                        self.output_tokens = value
                            .pointer("/response/usage/output_tokens")
                            .and_then(serde_json::Value::as_u64);
                    }
                    "response.failed" | "response.incomplete" => {
                        return Err("SSE terminal response was not completed".into());
                    }
                    _ => {}
                }
            } else if value["type"].as_str().is_some_and(|kind| {
                kind.starts_with("message_") || kind.starts_with("content_block_")
            }) {
                // Anthropic Messages: usage is split across message_start
                // (input) and message_delta (output); message_stop terminates.
                match value["type"].as_str().unwrap_or("") {
                    "message_start" => {
                        if let Some(input) = value
                            .pointer("/message/usage/input_tokens")
                            .and_then(serde_json::Value::as_u64)
                        {
                            self.input_tokens = Some(input);
                        }
                    }
                    "content_block_delta" => {
                        if let Some(text) = value
                            .pointer("/delta/text")
                            .and_then(serde_json::Value::as_str)
                        {
                            self.text.push_str(text);
                        }
                        if let Some(args) = value
                            .pointer("/delta/partial_json")
                            .and_then(serde_json::Value::as_str)
                        {
                            self.tool_arguments.push_str(args);
                        }
                    }
                    "message_delta" => {
                        if let Some(output) = value
                            .pointer("/usage/output_tokens")
                            .and_then(serde_json::Value::as_u64)
                        {
                            self.output_tokens = Some(output);
                        }
                        if let Some(input) = value
                            .pointer("/usage/input_tokens")
                            .and_then(serde_json::Value::as_u64)
                        {
                            self.input_tokens = Some(input);
                        }
                    }
                    "message_stop" => {
                        if self.terminal {
                            return Err("duplicate SSE terminal event".into());
                        }
                        self.terminal = true;
                    }
                    _ => {}
                }
            } else if value.get("candidates").is_some() || value.get("usageMetadata").is_some() {
                // Gemini: finishReason and usageMetadata may arrive in
                // separate frames; usage after the STOP frame is legitimate.
                for candidate in value["candidates"].as_array().into_iter().flatten() {
                    for part in candidate
                        .pointer("/content/parts")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if part["thought"].as_bool() != Some(true) {
                            if let Some(text) = part["text"].as_str() {
                                self.text.push_str(text);
                            }
                        }
                    }
                    if candidate["finishReason"].is_string() {
                        self.terminal = true;
                    }
                }
                if let Some(usage) = value.get("usageMetadata") {
                    if let Some(input) = usage["promptTokenCount"].as_u64() {
                        self.input_tokens = Some(input);
                    }
                    if let Some(output) = usage["candidatesTokenCount"].as_u64() {
                        self.output_tokens = Some(output);
                    }
                }
            } else {
                // OpenAI chat: a usage-only chunk may follow the
                // finish_reason chunk; content after the terminal may not.
                if self.terminal
                    && value["choices"]
                        .as_array()
                        .is_some_and(|choices| !choices.is_empty())
                {
                    return Err("SSE data after terminal marker".into());
                }
                if let Some(choices) = value["choices"].as_array() {
                    for choice in choices {
                        self.text.push_str(
                            choice
                                .pointer("/delta/content")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default(),
                        );
                        if let Some(calls) = choice
                            .pointer("/delta/tool_calls")
                            .and_then(serde_json::Value::as_array)
                        {
                            for call in calls {
                                self.tool_arguments.push_str(
                                    call.pointer("/function/arguments")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default(),
                                );
                            }
                        }
                        if choice["finish_reason"].is_string() {
                            self.terminal = true;
                        }
                    }
                }
                if let Some(usage) = value.get("usage") {
                    self.input_tokens = usage["prompt_tokens"]
                        .as_u64()
                        .or_else(|| usage["input_tokens"].as_u64());
                    self.output_tokens = usage["completion_tokens"]
                        .as_u64()
                        .or_else(|| usage["output_tokens"].as_u64());
                    self.cached_tokens = usage
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(serde_json::Value::as_u64);
                }
            }
        }
        Ok(())
    }

    pub fn finish(&self, expected: &ExpectedOutput) -> Result<(), String> {
        if !self.buffer.iter().all(u8::is_ascii_whitespace) {
            return Err("truncated SSE frame".into());
        }
        if !self.terminal {
            return Err("missing terminal SSE event".into());
        }
        if self.text != expected.text {
            return Err(format!(
                "text mismatch: expected {:?}, got {:?}",
                expected.text, self.text
            ));
        }
        if self.input_tokens != Some(expected.input_tokens)
            || self.output_tokens != Some(expected.output_tokens)
        {
            return Err(format!(
                "usage mismatch: expected {}/{}, got {:?}/{:?}",
                expected.input_tokens,
                expected.output_tokens,
                self.input_tokens,
                self.output_tokens
            ));
        }
        if expected
            .cached_tokens
            .is_some_and(|count| self.cached_tokens != Some(count))
        {
            return Err("cached token usage mismatch".into());
        }
        if expected
            .tool_arguments
            .as_ref()
            .is_some_and(|arguments| &self.tool_arguments != arguments)
        {
            return Err("tool arguments mismatch".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct RequestParams<'a> {
    pub client: &'a reqwest::Client,
    pub url: &'a str,
    pub headers: HeaderMap,
    pub body: Arc<String>,
    pub timeout: Duration,
    pub expected: Option<Arc<ExpectedOutput>>,
}

pub async fn run_single_request(params: RequestParams<'_>) -> SampleResult {
    let start = Instant::now();
    let resp = match params
        .client
        .post(params.url)
        .headers(params.headers)
        .body(params.body.as_str().to_string())
        .timeout(params.timeout)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return SampleResult::Error {
                kind: map_reqwest_error(&e),
            };
        }
    };

    let status = resp.status();
    if !status.is_success() {
        return SampleResult::Error {
            kind: format!("status:{}", status.as_u16()),
        };
    }

    let mut stream = resp.bytes_stream();
    let first = match stream.next().await {
        Some(Ok(bytes)) => bytes,
        Some(Err(e)) => {
            return SampleResult::Error {
                kind: map_reqwest_error(&e),
            };
        }
        None => {
            return SampleResult::Error {
                kind: "io:empty_body".to_string(),
            };
        }
    };

    let ttfb_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut gate = SemanticGate::default();
    // Content TTFT is the first frame that carries meaningful text/tool
    // content, distinct from the first byte (Task 35).
    let mut ttft_ms: Option<f64> = None;
    let feed = |gate: &mut SemanticGate, bytes: &[u8], ttft: &mut Option<f64>| {
        let before = gate.content_len();
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        let result = gate.feed(bytes, elapsed);
        if ttft.is_none() && gate.content_len() > before {
            *ttft = Some(elapsed);
        }
        result
    };
    if let Err(kind) = feed(&mut gate, &first, &mut ttft_ms) {
        return SampleResult::Error {
            kind: format!("semantic:{kind}"),
        };
    }
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if let Err(kind) = feed(&mut gate, &bytes, &mut ttft_ms) {
                    return SampleResult::Error {
                        kind: format!("semantic:{kind}"),
                    };
                }
            }
            Err(e) => {
                return SampleResult::Error {
                    kind: map_reqwest_error(&e),
                };
            }
        }
    }
    let completion_ms = start.elapsed().as_secs_f64() * 1000.0;
    let semantic = match params.expected.as_deref() {
        Some(expected) => {
            if let Err(kind) = gate.finish(expected) {
                return SampleResult::Error {
                    kind: format!("semantic:{kind}"),
                };
            }
            true
        }
        None => false,
    };
    SampleResult::Success {
        ttft_ms: ttft_ms.unwrap_or(ttfb_ms),
        ttfb_ms,
        completion_ms,
        semantic,
    }
}

pub async fn run_benchmark(
    cfg: BenchConfig,
) -> Result<BenchReport, Box<dyn std::error::Error + Send + Sync>> {
    validate_bench_url(&cfg.url)?;
    // Benchmark traffic stays on the local fixture: no environment proxy and
    // no redirect can carry it anywhere else.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let parsed_headers = parse_headers(&cfg.headers)?;
    let (body, expected) =
        split_expectation(&cfg.body_json.unwrap_or_else(|| DEFAULT_BODY.to_string()));
    let body = Arc::new(body);
    let expected = expected.map(Arc::new);
    let timeout = Duration::from_millis(cfg.timeout_ms);
    let concurrency = cfg.concurrency.max(1);

    let wall_start = Instant::now();
    let mut set = JoinSet::new();
    let mut spawned = 0;
    let mut results = Vec::with_capacity(cfg.total);

    while spawned < cfg.total || !set.is_empty() {
        while spawned < cfg.total && set.len() < concurrency {
            let cl = client.clone();
            let u = cfg.url.clone();
            let hdrs = parsed_headers.clone();
            let b = Arc::clone(&body);
            let e = expected.clone();
            set.spawn(async move {
                run_single_request(RequestParams {
                    client: &cl,
                    url: &u,
                    headers: hdrs,
                    body: b,
                    timeout,
                    expected: e,
                })
                .await
            });
            spawned += 1;
        }

        if let Some(res) = set.join_next().await {
            match res {
                Ok(sample) => results.push(sample),
                Err(_) => results.push(SampleResult::Error {
                    kind: "io:task_join".to_string(),
                }),
            }
        }
    }

    let wall_time_secs = wall_start.elapsed().as_secs_f64();

    let mut ttft_samples = Vec::new();
    let mut ttfb_samples = Vec::new();
    let mut completion_samples = Vec::new();
    let mut semantic_samples = 0usize;
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();

    for r in results {
        match r {
            SampleResult::Success {
                ttft_ms,
                ttfb_ms,
                completion_ms,
                semantic,
            } => {
                ttft_samples.push(ttft_ms);
                ttfb_samples.push(ttfb_ms);
                completion_samples.push(completion_ms);
                semantic_samples += usize::from(semantic);
            }
            SampleResult::Error { kind } => {
                *errors.entry(kind).or_insert(0) += 1;
            }
        }
    }

    ttft_samples.sort_by(f64::total_cmp);

    let report = compute_report(StatsInput {
        total: cfg.total,
        concurrency: cfg.concurrency,
        wall_time_secs,
        sorted_ttft_ms: &ttft_samples,
        errors,
    });
    // Correctness first: every sample must pass the semantic gate when an
    // expectation was given; a failed gate publishes no timing/throughput.
    let correctness_passed = report.failed == 0
        && report.successful == report.total
        && (expected.is_none() || semantic_samples == report.total);
    let mut json = serde_json::to_value(&report)?;
    if let Some(object) = json.as_object_mut() {
        object.insert("correctness_passed".into(), correctness_passed.into());
        object.insert("semantic_samples".into(), semantic_samples.into());
        if correctness_passed {
            object.insert("ttfb_ms".into(), serde_json::json!(ttfb_samples));
            object.insert(
                "completion_ms".into(),
                serde_json::json!(completion_samples),
            );
        } else {
            object.remove("ttft_ms");
            object.remove("rps");
        }
    }
    tokio::fs::write(&cfg.out, serde_json::to_vec_pretty(&json)?).await?;

    let summary_line = format_summary_line(&report);
    println!("{summary_line}");
    if !correctness_passed {
        return Err(format!("benchmark correctness gate failed: {:?}", report.errors).into());
    }

    Ok(report)
}
