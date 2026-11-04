use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde::Serialize;

use crate::proxy::auth::AuthContext;
use crate::proxy::{ApiError, AppState};

const CREATED: i64 = 1_735_689_600;

#[derive(Serialize)]
pub struct ModelResponse {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

#[derive(Serialize)]
pub struct ModelListResponse {
    object: &'static str,
    data: Vec<ModelResponse>,
}

impl ModelResponse {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "model",
            created: CREATED,
            owned_by: "ai-proxy",
        }
    }
}

fn configured(state: &AppState) -> ModelListResponse {
    let data = state
        .config
        .models
        .iter()
        .map(|model| ModelResponse::new(&model.id))
        .collect();
    ModelListResponse {
        object: "list",
        data,
    }
}

/// A restricted key sees the allowlist. An unrestricted key sees everything the upstream
/// advertises, because the allowlist cannot describe what that key may use.
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    if !auth.allow_all_models {
        return Json(configured(&state)).into_response();
    }
    match catalog(&state).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to fetch the upstream model list, answering with the allowlist"
            );
            Json(configured(&state)).into_response()
        }
    }
}

pub async fn get(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ModelResponse>, ApiError> {
    let id = id.trim_start_matches('/');
    // An unrestricted key may use models outside the allowlist, so the answer cannot depend on it.
    if auth.allow_all_models {
        return Ok(Json(ModelResponse::new(id)));
    }
    match state.config.models.iter().find(|model| model.id == id) {
        Some(model) => Ok(Json(ModelResponse::new(&model.id))),
        None => Err(ApiError::model_not_found(id)),
    }
}

/// The upstream list is passed through as it arrives, so clients keep the extra fields
/// (`context_length`, `supported_endpoints`) that a synthesised entry would drop.
async fn catalog(state: &AppState) -> anyhow::Result<serde_json::Value> {
    let ttl = Duration::from_secs(state.config.models_ttl_secs);
    if !ttl.is_zero() {
        if let Some((fetched_at, body)) = cached(state) {
            if fetched_at.elapsed() < ttl {
                return Ok(body);
            }
        }
    }

    let url = format!(
        "{}/models",
        state.config.upstream_base_url.trim_end_matches('/')
    );
    let response = state
        .client
        .get(&url)
        .bearer_auth(&state.config.upstream_api_key)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("upstream {} returned {}", url, status.as_u16());
    }
    let text = response.text().await?;
    let body: serde_json::Value = serde_json::from_str(&text)?;
    if body.get("data").and_then(|data| data.as_array()).is_none() {
        anyhow::bail!("upstream {} returned no `data` array", url);
    }
    if let Ok(mut slot) = state.models_cache.lock() {
        *slot = Some((Instant::now(), body.clone()));
    }
    Ok(body)
}

fn cached(state: &AppState) -> Option<(Instant, serde_json::Value)> {
    state.models_cache.lock().ok().and_then(|slot| slot.clone())
}
