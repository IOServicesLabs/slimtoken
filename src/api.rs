use crate::control::TaskProgress;
use crate::runner::{RunDetails, RunManager, RunRequest, RunSummary};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn router(manager: RunManager) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/runs", get(list_runs).post(submit_run))
        .route("/v1/runs/{id}", get(get_run).delete(cancel_run))
        .route("/v1/runs/{id}/cancel", post(cancel_run))
        .route("/v1/runs/{id}/steer", post(steer_run))
        .route("/v1/runs/{id}/tasks", get(get_tasks))
        .route("/v1/runs/{id}/plan", get(get_plan))
        .route("/v1/runs/{id}/report", get(get_report))
        .with_state(manager)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "slimtoken"}))
}

async fn list_runs(State(manager): State<RunManager>) -> Json<Vec<RunSummary>> {
    Json(manager.list())
}

async fn submit_run(
    State(manager): State<RunManager>,
    Json(request): Json<RunRequest>,
) -> Result<(StatusCode, Json<RunSummary>), ApiError> {
    request.validate().map_err(ApiError::bad_request)?;
    let summary = manager.submit(request).await.map_err(ApiError::internal)?;
    Ok((StatusCode::ACCEPTED, Json(summary)))
}

async fn get_run(
    State(manager): State<RunManager>,
    Path(id): Path<String>,
) -> Result<Json<RunDetails>, ApiError> {
    manager
        .get(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&id))
}

#[derive(Debug, Deserialize)]
struct SteerRequest {
    instruction: String,
}

async fn steer_run(
    State(manager): State<RunManager>,
    Path(id): Path<String>,
    Json(request): Json<SteerRequest>,
) -> Result<(StatusCode, Json<RunDetails>), ApiError> {
    let details = manager
        .steer(&id, &request.instruction)
        .map_err(ApiError::bad_request)?
        .ok_or_else(|| ApiError::not_found(&id))?;
    Ok((StatusCode::ACCEPTED, Json(details)))
}

async fn cancel_run(
    State(manager): State<RunManager>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<RunDetails>), ApiError> {
    let details = manager
        .cancel(&id)
        .ok_or_else(|| ApiError::not_found(&id))?;
    Ok((StatusCode::ACCEPTED, Json(details)))
}

async fn get_tasks(
    State(manager): State<RunManager>,
    Path(id): Path<String>,
) -> Result<Json<BTreeMap<String, TaskProgress>>, ApiError> {
    manager
        .get(&id)
        .map(|details| Json(details.tasks))
        .ok_or_else(|| ApiError::not_found(&id))
}

async fn get_plan(
    State(manager): State<RunManager>,
    Path(id): Path<String>,
) -> Result<Json<crate::dag::Plan>, ApiError> {
    if manager.get(&id).is_none() {
        return Err(ApiError::not_found(&id));
    }
    manager
        .plan(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_ready("plan", &id))
}

async fn get_report(
    State(manager): State<RunManager>,
    Path(id): Path<String>,
) -> Result<Json<crate::scheduler::RunReport>, ApiError> {
    if manager.get(&id).is_none() {
        return Err(ApiError::not_found(&id));
    }
    manager
        .report(&id)
        .map(Json)
        .ok_or_else(|| ApiError::not_ready("report", &id))
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: format!("{error:#}"),
        }
    }

    fn internal(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{error:#}"),
        }
    }

    fn not_found(id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("run '{id}' was not found"),
        }
    }

    fn not_ready(artifact: &str, id: &str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: format!("{artifact} for run '{id}' is not ready"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}
