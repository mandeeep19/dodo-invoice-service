use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(sqlx::FromRow)]
pub struct WebhookEndpointRow {
    pub id: Uuid,
    pub business_id: Uuid,
    pub url: String,
    pub secret: String,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
pub struct RegisterWebhookRequest {
    pub url: String,
}

#[derive(Serialize)]
pub struct RegisterWebhookResponse {
    pub id: Uuid,
    pub url: String,
    /// Shown once, at creation, same as the API key. Not retrievable again.
    pub secret: String,
    pub created_at: DateTime<Utc>,
}
