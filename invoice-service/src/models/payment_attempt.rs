use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(sqlx::FromRow, Serialize, Clone)]
pub struct PaymentAttemptRow {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub status: String,
    pub card_token: String,
    pub failure_code: Option<String>,
    pub psp_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct PayRequest {
    pub card_token: String,
}
