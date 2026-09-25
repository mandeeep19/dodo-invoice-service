use crate::{
    auth::AuthedBusiness,
    error::AppError,
    models::invoice::*,
    state_machine::{reject_invalid_transition, InvoiceState},
    webhook_dispatcher, AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

pub async fn create_invoice(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Json(req): Json<CreateInvoiceRequest>,
) -> Result<(StatusCode, Json<InvoiceResponse>), AppError> {
    if req.line_items.is_empty() {
        return Err(AppError::bad_request(
            "invalid_invoice",
            "at least one line item is required",
        ));
    }
    for item in &req.line_items {
        if item.quantity <= 0 {
            return Err(AppError::bad_request("invalid_line_item", "quantity must be positive"));
        }
        if item.unit_amount_cents < 0 {
            return Err(AppError::bad_request(
                "invalid_line_item",
                "unit_amount_cents must not be negative",
            ));
        }
    }

    let mut tx = state.db.begin().await?;

    // The customer must belong to the authenticated business - this is the
    // only thing standing between one business and another business's data.
    let customer_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)",
    )
    .bind(req.customer_id)
    .bind(business_id)
    .fetch_one(&mut *tx)
    .await?;
    if !customer_exists {
        return Err(AppError::bad_request("invalid_customer", "no such customer for this business"));
    }

    // Server computes the total. The client-supplied total, if any, is never
    // read - `CreateInvoiceRequest` does not even have a `total_cents` field.
    let total_cents: i64 = req
        .line_items
        .iter()
        .map(|i| i.quantity * i.unit_amount_cents)
        .sum();

    let invoice_id: Uuid = sqlx::query_scalar(
        "INSERT INTO invoices (business_id, customer_id, state, total_cents, due_date)
         VALUES ($1, $2, 'draft', $3, $4) RETURNING id",
    )
    .bind(business_id)
    .bind(req.customer_id)
    .bind(total_cents)
    .bind(req.due_date)
    .fetch_one(&mut *tx)
    .await?;

    let mut line_items = Vec::with_capacity(req.line_items.len());
    for item in &req.line_items {
        let amount_cents = item.quantity * item.unit_amount_cents;
        let row = sqlx::query_as::<_, LineItemRow>(
            "INSERT INTO invoice_line_items (invoice_id, description, quantity, unit_amount_cents, amount_cents)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, invoice_id, description, quantity, unit_amount_cents, amount_cents",
        )
        .bind(invoice_id)
        .bind(&item.description)
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .bind(amount_cents)
        .fetch_one(&mut *tx)
        .await?;
        line_items.push(row);
    }

    let payload = json!({
        "event": "invoice.created",
        "invoice_id": invoice_id,
        "customer_id": req.customer_id,
        "total_cents": total_cents,
    });
    webhook_dispatcher::enqueue_event(&mut tx, business_id, "invoice.created", payload).await?;

    let invoice_row = sqlx::query_as::<_, InvoiceRow>(
        "SELECT id, business_id, customer_id, state, currency, total_cents, due_date, created_at, updated_at
         FROM invoices WHERE id = $1",
    )
    .bind(invoice_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(InvoiceResponse::from_row(invoice_row, line_items)),
    ))
}

pub async fn get_invoice(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, AppError> {
    let invoice_row = sqlx::query_as::<_, InvoiceRow>(
        "SELECT id, business_id, customer_id, state, currency, total_cents, due_date, created_at, updated_at
         FROM invoices WHERE id = $1 AND business_id = $2",
    )
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::not_found("invoice_not_found", "no such invoice"))?;

    let line_items = sqlx::query_as::<_, LineItemRow>(
        "SELECT id, invoice_id, description, quantity, unit_amount_cents, amount_cents
         FROM invoice_line_items WHERE invoice_id = $1",
    )
    .bind(invoice_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(InvoiceResponse::from_row(invoice_row, line_items)))
}

pub async fn list_invoices(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Query(query): Query<ListInvoicesQuery>,
) -> Result<Json<Vec<InvoiceResponse>>, AppError> {
    if let Some(state_filter) = &query.state {
        if InvoiceState::parse(state_filter).is_none() {
            return Err(AppError::bad_request(
                "invalid_state_filter",
                "state must be one of draft, open, paid, void, uncollectible",
            ));
        }
    }

    let rows = sqlx::query_as::<_, InvoiceRow>(
        "SELECT id, business_id, customer_id, state, currency, total_cents, due_date, created_at, updated_at
         FROM invoices
         WHERE business_id = $1 AND ($2::text IS NULL OR state = $2)
         ORDER BY created_at DESC",
    )
    .bind(business_id)
    .bind(&query.state)
    .fetch_all(&state.db)
    .await?;

    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let line_items = sqlx::query_as::<_, LineItemRow>(
            "SELECT id, invoice_id, description, quantity, unit_amount_cents, amount_cents
             FROM invoice_line_items WHERE invoice_id = $1",
        )
        .bind(row.id)
        .fetch_all(&state.db)
        .await?;
        result.push(InvoiceResponse::from_row(row, line_items));
    }

    Ok(Json(result))
}

async fn transition(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    action: &str,
    allowed: impl Fn(InvoiceState) -> bool,
    target: InvoiceState,
) -> Result<InvoiceResponse, AppError> {
    let mut tx = state.db.begin().await?;

    let row = sqlx::query("SELECT state FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE")
        .bind(invoice_id)
        .bind(business_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        return Err(AppError::not_found("invoice_not_found", "no such invoice"));
    };
    let current_state = InvoiceState::parse(row.get::<String, _>("state").as_str())
        .ok_or_else(|| AppError::internal("invoice has an unrecognized state"))?;

    if !allowed(current_state) {
        return Err(reject_invalid_transition(action, current_state));
    }

    sqlx::query("UPDATE invoices SET state = $2, updated_at = now() WHERE id = $1")
        .bind(invoice_id)
        .bind(target.as_str())
        .execute(&mut *tx)
        .await?;

    let invoice_row = sqlx::query_as::<_, InvoiceRow>(
        "SELECT id, business_id, customer_id, state, currency, total_cents, due_date, created_at, updated_at
         FROM invoices WHERE id = $1",
    )
    .bind(invoice_id)
    .fetch_one(&mut *tx)
    .await?;
    let line_items = sqlx::query_as::<_, LineItemRow>(
        "SELECT id, invoice_id, description, quantity, unit_amount_cents, amount_cents
         FROM invoice_line_items WHERE invoice_id = $1",
    )
    .bind(invoice_id)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(InvoiceResponse::from_row(invoice_row, line_items))
}

pub async fn finalize_invoice(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, AppError> {
    let resp = transition(
        &state,
        business_id,
        invoice_id,
        "finalize",
        InvoiceState::can_finalize,
        InvoiceState::Open,
    )
    .await?;
    Ok(Json(resp))
}

pub async fn void_invoice(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, AppError> {
    let resp = transition(
        &state,
        business_id,
        invoice_id,
        "void",
        InvoiceState::can_void,
        InvoiceState::Void,
    )
    .await?;
    Ok(Json(resp))
}

pub async fn mark_uncollectible(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>, AppError> {
    let resp = transition(
        &state,
        business_id,
        invoice_id,
        "mark-uncollectible",
        InvoiceState::can_mark_uncollectible,
        InvoiceState::Uncollectible,
    )
    .await?;
    Ok(Json(resp))
}
