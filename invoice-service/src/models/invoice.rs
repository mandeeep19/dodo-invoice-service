use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(sqlx::FromRow)]
pub struct InvoiceRow {
    pub id: Uuid,
    pub business_id: Uuid,
    pub customer_id: Uuid,
    pub state: String,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow, Serialize, Clone)]
pub struct LineItemRow {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

#[derive(Deserialize)]
pub struct LineItemInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Deserialize)]
pub struct CreateInvoiceRequest {
    pub customer_id: Uuid,
    pub line_items: Vec<LineItemInput>,
    pub due_date: Option<NaiveDate>,
}

#[derive(Serialize)]
pub struct InvoiceResponse {
    pub id: Uuid,
    pub business_id: Uuid,
    pub customer_id: Uuid,
    pub state: String,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: Option<NaiveDate>,
    pub line_items: Vec<LineItemRow>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl InvoiceResponse {
    pub fn from_row(row: InvoiceRow, line_items: Vec<LineItemRow>) -> Self {
        Self {
            id: row.id,
            business_id: row.business_id,
            customer_id: row.customer_id,
            state: row.state,
            currency: row.currency,
            total_cents: row.total_cents,
            due_date: row.due_date,
            line_items,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Deserialize)]
pub struct ListInvoicesQuery {
    pub state: Option<String>,
}
