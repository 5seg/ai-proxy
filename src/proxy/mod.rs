pub mod auth;
pub mod chat;
pub mod models;
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
}

pub fn router(config: Config, conn: Connection, client: reqwest::Client) -> Router {
    let state = AppState {
        config: Arc::new(config),
        db: Arc::new(Mutex::new(conn)),
        client,
    };

    let v1 = Router::new()
        .route("/models", get(models::list))
        .route("/models/{*id}", get(models::get))
        .route("/chat/completions", post(chat::handle));

    Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", v1)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    www_authenticate: bool,
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_api_key",
            message: "invalid API key".to_string(),
            www_authenticate: true,
        }
    }

    pub fn model_not_allowed(model: &str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "model_not_allowed",
            message: format!("model `{model}` is not allowed"),
            www_authenticate: false,
        }
    }

    pub fn model_not_found(model: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "model_not_allowed",
            message: format!("model `{model}` is not found"),
            www_authenticate: false,
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "not found".to_string(),
            www_authenticate: false,
        }
    }

    pub fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.to_string(),
            www_authenticate: false,
        }
    }

    pub fn upstream_unavailable() -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_unavailable",
            message: "upstream unavailable".to_string(),
            www_authenticate: false,
        }
    }

    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: "internal error".to_string(),
            www_authenticate: false,
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
