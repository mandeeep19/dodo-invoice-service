use crate::{auth::AuthedBusiness, error::AppError, models::webhook::*, AppState};
use axum::{extract::State, http::StatusCode, Json};
use rand::RngCore;

pub async fn register_webhook(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Json(req): Json<RegisterWebhookRequest>,
) -> Result<(StatusCode, Json<RegisterWebhookResponse>), AppError> {
    if !(req.url.starts_with("http://") || req.url.starts_with("https://")) {
        return Err(AppError::bad_request(
            "invalid_webhook_url",
            "url must start with http:// or https://",
        ));
    }

    let mut secret_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret_bytes);
    let secret = format!("whsec_{}", hex::encode(secret_bytes));

    let row = sqlx::query_as::<_, WebhookEndpointRow>(
        "INSERT INTO webhook_endpoints (business_id, url, secret)
         VALUES ($1, $2, $3)
         RETURNING id, business_id, url, secret, created_at, disabled_at",
    )
    .bind(business_id)
    .bind(&req.url)
    .bind(&secret)
    .fetch_one(&state.db)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(RegisterWebhookResponse {
            id: row.id,
            url: row.url,
            secret: row.secret,
            created_at: row.created_at,
        }),
    ))
}
