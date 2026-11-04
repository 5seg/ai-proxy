use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use serde_json::Value;

use crate::db::{self, NewRequest};
use crate::proxy::auth::{self, AuthContext};
use crate::proxy::usage::{self, SseUsageScanner, Usage};
use crate::proxy::{ApiError, AppState};

const PROMPT_HASH_LIMIT: usize = 4096;

pub async fn chat_completions(
    state: State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    relay(
        state.0,
        addr,
        auth,
        headers,
        body,
        "chat_completions",
        "/chat/completions",
    )
    .await
}

pub async fn responses(
    state: State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    relay(state.0, addr, auth, headers, body, "responses", "/responses").await
}

async fn relay(
    state: AppState,
    addr: SocketAddr,
    auth: AuthContext,
    headers: HeaderMap,
    body: Bytes,
    endpoint: &'static str,
    upstream_path: &str,
) -> Response {
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return ApiError::bad_request("invalid JSON body").into_response(),
    };
    let model = match payload.get("model").and_then(Value::as_str) {
        Some(model) => model,
        None => return ApiError::bad_request("missing model").into_response(),
    };
    let meta = RequestMeta::from_payload(&payload);

    let started = Instant::now();
    let client_ip = addr.ip().to_string();
    let prompt_hash = *blake3::hash(&body[..body.len().min(PROMPT_HASH_LIMIT)]).as_bytes();
    let request_id = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => return ApiError::internal().into_response(),
        };
        let new_request = NewRequest {
            started_at: crate::now_ms(),
            device_id: auth.device_id,
            api_key_id: auth.api_key_id,
            client_ip: &client_ip,
            endpoint,
            model,
            stream: meta.stream,
            prompt_hash: &prompt_hash,
            message_count: meta.message_count,
            prompt_chars: meta.prompt_chars,
            has_tools: meta.has_tools,
            has_image: meta.has_image,
            temperature: meta.temperature,
            max_tokens: meta.max_tokens,
        };
        match db::insert_request(&conn, &new_request) {
            Ok(id) => id,
            Err(err) => {
                tracing::error!(error = %err, "failed to insert request row");
                return ApiError::internal().into_response();
            }
        }
    };

    let mut guard = RequestGuard::new(
        state.clone(),
        request_id,
        auth.api_key_id,
        state.config.capture_ttl_hours * 3_600_000,
        started,
    );
    if auth.capture_bodies {
        guard.capture = Some(Captured::new(state.config.capture_max_bytes, &body));
    }

    if !auth.allow_all_models && !auth::is_model_allowed(&state.config, model) {
        guard.status_code = Some(StatusCode::FORBIDDEN.as_u16() as i64);
        guard.finish(Some("model_denied"), None, false);
        return ApiError::model_not_allowed(model).into_response();
    }

    let now = time::OffsetDateTime::now_utc();
    if let Some(limit) = auth.monthly_token_limit {
        let used = {
            let conn = match state.db.lock() {
                Ok(conn) => conn,
                Err(_) => return ApiError::internal().into_response(),
            };
            match db::month_tokens_used(&conn, auth.api_key_id, crate::quota::month_start_ms(now)) {
                Ok(used) => used,
                Err(err) => {
                    tracing::error!(error = %err, "failed to sum monthly tokens");
                    return ApiError::internal().into_response();
                }
            }
        };
        if used >= limit {
            guard.status_code = Some(StatusCode::TOO_MANY_REQUESTS.as_u16() as i64);
            guard.quota_exceeded = true;
            guard.finish(Some("quota_exceeded"), None, false);
            return ApiError::insufficient_quota(crate::quota::retry_after_seconds(now))
                .into_response();
        }
        guard.quota = Some(QuotaBudget {
            limit,
            used_before: used,
        });
    }

    let url = format!(
        "{}{}",
        state.config.upstream_base_url.trim_end_matches('/'),
        upstream_path
    );
    let mut upstream_request = state.client.post(url).body(body);
    if let Some(value) = headers.get(header::CONTENT_TYPE) {
        upstream_request = upstream_request.header(header::CONTENT_TYPE, value);
    }
    if let Some(value) = headers.get(header::ACCEPT) {
        upstream_request = upstream_request.header(header::ACCEPT, value);
    }
    upstream_request = upstream_request.bearer_auth(&state.config.upstream_api_key);

    let upstream = match upstream_request.send().await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "upstream request failed");
            guard.status_code = Some(StatusCode::BAD_GATEWAY.as_u16() as i64);
            guard.finish(Some("proxy"), None, false);
            return ApiError::upstream_unavailable().into_response();
        }
    };

    let status = upstream.status();
    guard.status_code = Some(status.as_u16() as i64);
    let status_error: Option<&'static str> = if status.is_client_error() {
        Some("upstream_4xx")
    } else if status.is_server_error() {
        Some("upstream_5xx")
    } else {
        None
    };
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();

    if !meta.stream {
        let bytes = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(error = %err, "failed to read upstream body");
                guard.status_code = Some(StatusCode::BAD_GATEWAY.as_u16() as i64);
                guard.finish(Some("proxy"), None, false);
                return ApiError::upstream_unavailable().into_response();
            }
        };
        guard.mark_ttft();
        guard.push_capture(&bytes);
        let usage = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| usage::extract(&value));
        guard.finish(status_error, usage.as_ref(), false);

        let mut response = Response::builder().status(status);
        if let Some(value) = content_type {
            response = response.header(header::CONTENT_TYPE, value);
        }
        return match response.body(Body::from(bytes)) {
            Ok(response) => response,
            Err(_) => ApiError::internal().into_response(),
        };
    }

    let relay = StreamRelay {
        inner: Box::pin(upstream.bytes_stream()),
        scanner: SseUsageScanner::new(),
        guard,
        status_error,
    };
    let mut response = Response::builder().status(status);
    if let Some(value) = content_type {
        response = response.header(header::CONTENT_TYPE, value);
    }
    match response.body(Body::from_stream(relay)) {
        Ok(response) => response,
        Err(_) => ApiError::internal().into_response(),
    }
}

struct RequestMeta {
    stream: bool,
    message_count: i64,
    prompt_chars: i64,
    has_tools: bool,
    has_image: bool,
    temperature: Option<f64>,
    max_tokens: Option<i64>,
}

impl RequestMeta {
    fn from_payload(payload: &Value) -> Self {
        let (message_count, prompt_chars, has_image) = match payload
            .get("messages")
            .and_then(Value::as_array)
        {
            Some(messages) => (
                messages.len() as i64,
                count_prompt_chars(messages),
                has_image(messages),
            ),
            None => match payload.get("input").and_then(Value::as_array) {
                Some(items) => (
                    items.len() as i64,
                    count_prompt_chars(items),
                    has_image(items),
                ),
                None => (
                    0,
                    payload
                        .get("input")
                        .and_then(Value::as_str)
                        .map(|text| text.chars().count() as i64)
                        .unwrap_or(0),
                    false,
                ),
            },
        };
        Self {
            stream: payload
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            message_count,
            prompt_chars,
            has_tools: payload.get("tools").is_some(),
            has_image,
            temperature: payload.get("temperature").and_then(Value::as_f64),
            max_tokens: payload
                .get("max_tokens")
                .and_then(Value::as_i64)
                .or_else(|| payload.get("max_output_tokens").and_then(Value::as_i64)),
        }
    }
}

fn count_prompt_chars(messages: &[Value]) -> i64 {
    let mut total = 0i64;
    for message in messages {
        match message.get("content") {
            Some(Value::String(text)) => total += text.chars().count() as i64,
            Some(Value::Array(items)) => {
                for item in items {
                    if let Value::String(text) = item {
                        total += text.chars().count() as i64;
                    }
                }
            }
            _ => {}
        }
    }
    total
}

fn has_image(items: &[Value]) -> bool {
    items
        .iter()
        .any(|item| match item.get("content") {
            Some(Value::Array(content)) => content.iter().any(is_image_part),
            _ => is_image_part(item),
        })
}

fn is_image_part(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("image_url") | Some("input_image")
    )
}

struct Captured {
    prompt: String,
    response: Arc<Mutex<Vec<u8>>>,
    max: usize,
    truncated: Arc<AtomicBool>,
}

impl Captured {
    fn new(max: usize, body: &Bytes) -> Self {
        let mut prompt = String::from_utf8_lossy(&body[..body.len().min(max)]).into_owned();
        let clipped = body.len() > max;
        if clipped {
            prompt.push_str("\n[truncated]");
        }
        Self {
            prompt,
            response: Arc::new(Mutex::new(Vec::new())),
            max,
            truncated: Arc::new(AtomicBool::new(clipped)),
        }
    }

    fn push(&self, chunk: &[u8]) {
        let Ok(mut buffer) = self.response.lock() else {
            return;
        };
        if buffer.len() >= self.max {
            self.truncated.store(true, Ordering::Relaxed);
            return;
        }
        let room = self.max - buffer.len();
        if chunk.len() > room {
            self.truncated.store(true, Ordering::Relaxed);
        }
        buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    fn truncated(&self) -> bool {
        self.truncated.load(Ordering::Relaxed)
    }

    fn response_text(&self) -> Option<String> {
        let buffer = self.response.lock().ok()?;
        if buffer.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&buffer).into_owned())
    }
}

struct StreamRelay {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    scanner: SseUsageScanner,
    guard: RequestGuard,
    status_error: Option<&'static str>,
}

impl StreamRelay {
    fn finish(&mut self, error_kind: Option<&'static str>, aborted: bool) {
        self.scanner.flush();
        let usage = self.scanner.usage();
        self.guard.finish(error_kind, usage.as_ref(), aborted);
    }
}

impl Stream for StreamRelay {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.guard.mark_ttft();
                this.guard.push_capture(&chunk);
                this.scanner.feed(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                tracing::warn!(error = %err, "upstream stream failed");
                this.finish(Some("proxy"), false);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                let error_kind = this.status_error;
                this.finish(error_kind, false);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for StreamRelay {
    fn drop(&mut self) {
        self.finish(Some("client_abort"), true);
    }
}

#[derive(Clone, Copy)]
struct QuotaBudget {
    limit: i64,
    used_before: i64,
}

struct RequestGuard {
    state: AppState,
    id: i64,
    api_key_id: i64,
    capture_ttl_ms: i64,
    started: Instant,
    status_code: Option<i64>,
    ttft_ms: Option<i64>,
    quota: Option<QuotaBudget>,
    quota_exceeded: bool,
    capture: Option<Captured>,
    finished: bool,
}

impl RequestGuard {
    fn new(
        state: AppState,
        id: i64,
        api_key_id: i64,
        capture_ttl_ms: i64,
        started: Instant,
    ) -> Self {
        Self {
            state,
            id,
            api_key_id,
            capture_ttl_ms,
            started,
            status_code: None,
            ttft_ms: None,
            quota: None,
            quota_exceeded: false,
            capture: None,
            finished: false,
        }
    }

    fn push_capture(&self, chunk: &[u8]) {
        if let Some(capture) = &self.capture {
            capture.push(chunk);
        }
    }

    fn mark_ttft(&mut self) {
        if self.ttft_ms.is_none() {
            self.ttft_ms = Some(self.started.elapsed().as_millis() as i64);
        }
    }

    fn finish(&mut self, error_kind: Option<&str>, usage: Option<&Usage>, aborted: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.write(error_kind, usage, aborted);
    }

    fn write(&self, error_kind: Option<&str>, usage: Option<&Usage>, aborted: bool) {
        let Ok(conn) = self.state.db.lock() else {
            return;
        };
        let mut quota_exceeded = self.quota_exceeded;
        if let (Some(budget), Some(usage)) = (self.quota, usage) {
            quota_exceeded |=
                budget.used_before + usage.input_tokens + usage.output_tokens > budget.limit;
        }
        let latency_ms = self.started.elapsed().as_millis() as i64;
        let now = crate::now_ms();
        if let Some(capture) = &self.capture {
            let response = capture.response_text();
            if let Err(err) = db::insert_capture(
                &conn,
                self.id,
                self.api_key_id,
                now + self.capture_ttl_ms,
                capture.truncated(),
                Some(&capture.prompt),
                response.as_deref(),
            ) {
                tracing::error!(error = %err, "failed to store capture");
            }
        }
        if let Err(err) = db::finish_request(
            &conn,
            db::FinishRequest {
                id: self.id,
                finished_at: now,
                status_code: self.status_code,
                latency_ms: Some(latency_ms),
                ttft_ms: self.ttft_ms,
                usage,
                error_kind,
                aborted,
                quota_exceeded,
            },
        ) {
            tracing::error!(error = %err, "failed to update request row");
        }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.write(Some("client_abort"), None, true);
    }
}
