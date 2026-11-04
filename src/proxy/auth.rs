use axum::extract::{Request, State};
use axum::http::{header, HeaderMap};
use axum::middleware::Next;
use axum::response::Response;

use crate::config::Config;
use crate::db;
use crate::key;
use crate::proxy::{ApiError, AppState};

#[derive(Clone)]
pub struct AuthContext {
    pub api_key_id: i64,
    pub device_id: i64,
}

pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if request.uri().path() == "/healthz" {
        return Ok(next.run(request).await);
    }
    let token = bearer_token(request.headers()).ok_or_else(ApiError::unauthorized)?;
    let auth = active_key(&state, token)?;
    request.extensions_mut().insert(AuthContext {
        api_key_id: auth.id,
        device_id: auth.device_id,
    });
    Ok(next.run(request).await)
}

fn active_key(state: &AppState, token: &str) -> Result<db::KeyAuth, ApiError> {
    let hash = key::hash(token);
    let conn = state.db.lock().map_err(|_| ApiError::internal())?;
    let found = db::find_key(&conn, &hash).map_err(|err| {
        tracing::error!(error = %err, "failed to query api keys");
        ApiError::internal()
    })?;
    match found {
        Some(auth) if !auth.revoked => Ok(auth),
        _ => Err(ApiError::unauthorized()),
    }
}

pub fn is_model_allowed(config: &Config, model: &str) -> bool {
    config.models.iter().any(|entry| entry.id == model)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token)
    } else {
        None
    }
}
