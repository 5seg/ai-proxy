use axum::extract::{Path, State};
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

pub async fn list(State(state): State<AppState>) -> Json<ModelListResponse> {
    let data = state
        .config
        .models
        .iter()
        .map(|model| ModelResponse::new(&model.id))
        .collect();
    Json(ModelListResponse {
        object: "list",
        data,
    })
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
