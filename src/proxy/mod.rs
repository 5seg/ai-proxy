pub mod auth;
pub mod models;
pub mod relay;
pub mod usage;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use serde_json::json;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Mutex<Connection>>,
    pub client: reqwest::Client,
    /// Fetched upstream `/models` bodies, keyed by the instant the fetch finished.
    pub models_cache: Arc<Mutex<Option<(Instant, serde_json::Value)>>>,
}

pub fn router(config: Config, conn: Connection, client: reqwest::Client) -> Router {
    let max_body_bytes = config.max_body_bytes;
    let state = AppState {
        config: Arc::new(config),
        db: Arc::new(Mutex::new(conn)),
        client,
        models_cache: Arc::new(Mutex::new(None)),
    };

    let v1 = Router::new()
        .route("/models", get(models::list))
        .route("/models/{*id}", get(models::get))
        .route("/chat/completions", post(relay::chat_completions))
        .route("/responses", post(relay::responses));

    Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", v1)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    www_authenticate: bool,
    retry_after: Option<i64>,
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_api_key",
            message: "invalid API key".to_string(),
            www_authenticate: true,
            retry_after: None,
        }
    }

    pub fn model_not_allowed(model: &str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "model_not_allowed",
            message: format!("model `{model}` is not allowed"),
            www_authenticate: false,
            retry_after: None,
        }
    }

    pub fn model_not_found(model: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "model_not_found",
            message: format!("model `{model}` is not found"),
            www_authenticate: false,
            retry_after: None,
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "not found".to_string(),
            www_authenticate: false,
            retry_after: None,
        }
    }

    pub fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.to_string(),
            www_authenticate: false,
            retry_after: None,
        }
    }

    pub fn insufficient_quota(retry_after_seconds: i64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "insufficient_quota",
            message: "monthly token limit reached".to_string(),
            www_authenticate: false,
            retry_after: Some(retry_after_seconds),
        }
    }

    pub fn upstream_unavailable() -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_unavailable",
            message: "upstream unavailable".to_string(),
            www_authenticate: false,
            retry_after: None,
        }
    }

    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: "internal error".to_string(),
            www_authenticate: false,
            retry_after: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let Self {
            status,
            code,
            message,
            www_authenticate,
            retry_after,
        } = self;
        let body = Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": code,
            }
        }));
        let mut response = (status, body).into_response();
        if www_authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(seconds) = retry_after {
            if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}
