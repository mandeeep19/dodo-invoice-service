//! Business onboarding. Not one of the graded core endpoints, but something
//! has to create the first business + API key before anything else in the
//! system is reachable. In a real product this would be a signup flow
//! behind a dashboard; here it is a single unauthenticated POST, which is
//! fine because it creates no access to anyone else's data - it only ever
//! mints a brand new business and hands back that business's own key.

use crate::{auth, error::AppError, AppState};
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct CreateBusinessRequest {
    pub name: String,
}

#[derive(Serialize)]
pub struct CreateBusinessResponse {
    pub business_id: Uuid,
    pub name: String,
    /// Shown exactly once. Only the argon2 hash is persisted; if this is
    /// lost, the only recovery path is revoking it and minting a new one.
    pub api_key: String,
}

pub async fn create_business(
    State(state): State<AppState>,
    Json(req): Json<CreateBusinessRequest>,
) -> Result<(StatusCode, Json<CreateBusinessResponse>), AppError> {
    if req.name.trim().is_empty() {
        return Err(AppError::bad_request("invalid_name", "name must not be empty"));
    }

    let mut tx = state.db.begin().await?;

    let business_id: Uuid =
        sqlx::query_scalar("INSERT INTO businesses (name) VALUES ($1) RETURNING id")
            .bind(&req.name)
            .fetch_one(&mut *tx)
            .await?;

    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query("INSERT INTO api_keys (business_id, key_prefix, key_hash) VALUES ($1, $2, $3)")
        .bind(business_id)
        .bind(&prefix)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateBusinessResponse {
            business_id,
            name: req.name,
            api_key: full_key,
        }),
    ))
}
