use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde_json::{json, Value};

use crate::proxy::auth::AuthContext;
use crate::proxy::{ApiError, AppState};

const CREATED: i64 = 1_735_689_600;

fn synthesized(id: &str) -> Value {
    json!({
        "id": id,
        "object": "model",
        "created": CREATED,
        "owned_by": "ai-proxy",
    })
}

fn synthesized_allowlist(state: &AppState) -> Vec<Value> {
    let mut data: Vec<Value> = state
        .config
        .models
        .iter()
        .map(|model| synthesized(&model.id))
        .collect();
    sort_by_id(&mut data);
    data
}

fn is_allowlisted(state: &AppState, id: &str) -> bool {
    state.config.models.iter().any(|model| model.id == id)
}

fn sort_by_id(data: &mut [Value]) {
    data.sort_by(|left, right| {
        left["id"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["id"].as_str().unwrap_or_default())
    });
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

/// The allowlist as catalogue entries: an id takes the upstream entry verbatim when the
/// catalogue has it, and falls back to a synthesised entry when it does not.
fn merged_allowlist(state: &AppState, upstream: &[Value]) -> Vec<Value> {
    let mut data: Vec<Value> = state
        .config
        .models
        .iter()
        .map(|model| {
            upstream
                .iter()
                .find(|entry| entry["id"].as_str() == Some(model.id.as_str()))
                .cloned()
                .unwrap_or_else(|| synthesized(&model.id))
        })
        .collect();
    sort_by_id(&mut data);
    data
}

fn list_response(data: Vec<Value>) -> Json<Value> {
    Json(json!({ "object": "list", "data": data }))
}

/// A restricted key sees the allowlist, filled in from the upstream catalogue. An unrestricted
/// key sees everything the upstream advertises, because the allowlist cannot describe what that
/// key may use. When the upstream is unreachable, both fall back to what they can synthesise.
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    match catalog(&state).await {
        Ok(body) => {
            if auth.allow_all_models {
                return Json(body).into_response();
            }
            list_response(merged_allowlist(&state, &entries(&body))).into_response()
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to fetch the upstream model list, answering with the allowlist"
            );
            list_response(synthesized_allowlist(&state)).into_response()
        }
    }
}

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
        Ok(body) => {
            let found = entries(&body)
                .into_iter()
                .find(|entry| entry["id"].as_str() == Some(id));
            Ok(Json(found.unwrap_or_else(|| synthesized(id))))
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to fetch the upstream model list, answering with a synthesised entry"
            );
            Ok(Json(synthesized(id)))
        }
    }
}

/// The upstream catalogue, fetched and cached as it arrives. Both endpoints answer from it, so
/// callers keep the extra fields (`context_length`, `supported_endpoints`) that a synthesised
/// entry would drop; a restricted allowlist is filtered out of it instead of being rebuilt.
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
