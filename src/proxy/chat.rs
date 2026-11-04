use std::net::SocketAddr;
use std::pin::Pin;
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

const ENDPOINT: &str = "chat_completions";
const PROMPT_HASH_LIMIT: usize = 4096;

pub async fn handle(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    body: Bytes,
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
            endpoint: ENDPOINT,
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

    let mut guard = RequestGuard::new(state.clone(), request_id, started);

    if !auth::is_model_allowed(&state.config, model) {
        guard.status_code = Some(StatusCode::FORBIDDEN.as_u16() as i64);
        guard.finish(Some("model_denied"), None, false);
        return ApiError::model_not_allowed(model).into_response();
    }

    let url = format!(
        "{}/chat/completions",
        state.config.upstream_base_url.trim_end_matches('/')
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
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        Self {
            stream: payload
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            message_count: messages.len() as i64,
            prompt_chars: count_prompt_chars(messages),
            has_tools: payload.get("tools").is_some(),
            has_image: has_image(messages),
            temperature: payload.get("temperature").and_then(Value::as_f64),
            max_tokens: payload.get("max_tokens").and_then(Value::as_i64),
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

fn has_image(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("image_url"))
            })
    })
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

struct RequestGuard {
    state: AppState,
    id: i64,
    started: Instant,
    status_code: Option<i64>,
    ttft_ms: Option<i64>,
    finished: bool,
}

impl RequestGuard {
    fn new(state: AppState, id: i64, started: Instant) -> Self {
        Self {
            state,
            id,
            started,
            status_code: None,
            ttft_ms: None,
            finished: false,
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
        let latency_ms = self.started.elapsed().as_millis() as i64;
        if let Err(err) = db::finish_request(
            &conn,
            self.id,
            crate::now_ms(),
            self.status_code,
            Some(latency_ms),
            self.ttft_ms,
            usage,
            error_kind,
            aborted,
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
