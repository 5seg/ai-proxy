use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::Extension;
use axum::Json;
use serde_json::{json, Value};

use crate::proxy::auth::AuthContext;
use crate::proxy::{ApiError, AppState};

fn is_allowlisted(state: &AppState, id: &str) -> bool {
    state.config.models.iter().any(|model| model.id == id)
}

/// The upstream `data` array, keeping only the entries that carry a string `id`.
fn entries(body: &Value) -> Vec<Value> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter(|entry| entry["id"].as_str().is_some())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The catalogue entries a restricted key may see: the allowlist members the upstream
/// advertises, in catalogue order. An allowlisted id the upstream does not advertise is left
/// out rather than synthesised.
fn restricted_entries(state: &AppState, upstream: &[Value]) -> Vec<Value> {
    upstream
        .iter()
        .filter(|entry| {
            entry["id"]
                .as_str()
                .is_some_and(|id| is_allowlisted(state, id))
        })
        .cloned()
        .collect()
}

fn list_response(data: Vec<Value>) -> Json<Value> {
    Json(json!({ "object": "list", "data": data }))
}

/// A restricted key sees its allowlist filtered out of the upstream catalogue. An unrestricted
/// key sees everything the upstream advertises, because the allowlist cannot describe what that
/// key may use. The catalogue is the only source of model entries, so an unreachable upstream is
/// reported as `502 upstream_unavailable` instead of a synthesised answer.
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Value>, ApiError> {
    match catalog(&state).await {
        Ok(body) => {
            if auth.allow_all_models {
                return Ok(Json(body));
            }
            Ok(list_response(restricted_entries(&state, &entries(&body))))
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "the upstream model list is unavailable"
            );
            Err(ApiError::upstream_unavailable())
        }
    }
}

/// The catalogue entry for `id`, returned verbatim. An id the catalogue does not advertise is
/// `404 model_not_found` for either key kind; a restricted key is also rejected earlier when the
/// id is outside its allowlist.
pub async fn get(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = id.trim_start_matches('/');
    // An unrestricted key may use models outside the allowlist, so the answer cannot depend on it.
    if !auth.allow_all_models && !is_allowlisted(&state, id) {
        return Err(ApiError::model_not_found(id));
    }
    match catalog(&state).await {
        Ok(body) => entries(&body)
            .into_iter()
            .find(|entry| entry["id"].as_str() == Some(id))
            .map(Json)
            .ok_or_else(|| ApiError::model_not_found(id)),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "the upstream model list is unavailable"
            );
            Err(ApiError::upstream_unavailable())
        }
    }
}

/// The upstream catalogue, fetched and cached as it arrives. It is the only source of model
/// entries: a restricted allowlist is filtered out of it, never rebuilt, so callers keep the
/// extra fields (`context_length`, `supported_endpoints`) and see any fetch failure as an error.
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
