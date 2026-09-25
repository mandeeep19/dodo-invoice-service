//! POST /invoices/{id}/pay and GET .../payment-attempts/{id}.
//!
//! This is the section DESIGN.md calls "the hard section" and it is worth
//! reading alongside this file. Summary of the mechanism:
//!
//!   1. Idempotency-Key is reserved first (before touching the invoice at
//!      all), so a byte-identical retry never re-enters any of the logic
//!      below - it just replays the cached response.
//!   2. A short transaction locks the invoice row (`SELECT ... FOR UPDATE`),
//!      checks it is `open`, and inserts a `payment_attempts` row with
//!      status='pending'. A partial unique index guarantees at most one
//!      'pending' attempt can exist per invoice, so a second concurrent
//!      request - even one that arrives after this transaction commits -
//!      is rejected by the database itself, not by application logic that
//!      could race.
//!   3. That short transaction commits immediately - the row lock is not
//!      held for the external PSP call.
//!   4. The PSP call + its finalization run on a detached tokio task. The
//!      HTTP handler races that task against a short "sync wait" timeout:
//!      if the PSP answers quickly (the common case - ~100ms), the handler
//!      returns the final result synchronously. If not (tok_timeout), the
//!      handler returns 202 with the attempt id and the task keeps running
//!      in the background regardless, finalizing the attempt whenever the
//!      PSP eventually answers.

use crate::{
    auth::AuthedBusiness,
    error::AppError,
    idempotency::{self, IdempotencyCheck},
    models::payment_attempt::{PayRequest, PaymentAttemptRow},
    psp_client::PspOutcome,
    state_machine::InvoiceState,
    webhook_dispatcher, AppState,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

#[derive(Serialize, Clone)]
struct PayResultBody {
    attempt_id: Uuid,
    invoice_id: Uuid,
    status: String,
    failure_code: Option<String>,
    psp_ref: Option<String>,
}

const IDEMPOTENCY_HEADER: &str = "Idempotency-Key";

pub async fn pay_invoice(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(req): Json<PayRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let idem_key = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            AppError::bad_request(
                "idempotency_key_required",
                "an Idempotency-Key header is required for POST /pay",
            )
        })?;

    if req.card_token.trim().is_empty() {
        return Err(AppError::bad_request("invalid_card_token", "card_token is required"));
    }

    let request_hash = idempotency::hash_body(
        json!({ "invoice_id": invoice_id, "card_token": req.card_token }).to_string().as_bytes(),
    );

    match idempotency::reserve_or_get(&state.db, business_id, &idem_key, &request_hash).await? {
        IdempotencyCheck::UseCached { status, body } => {
            return Ok((status, Json(body)));
        }
        IdempotencyCheck::Proceed => {}
    }

    // From here on, any early return must finalize the idempotency row so a
    // retry with the same key replays this exact outcome instead of
    // re-running the claim logic.
    let result = try_claim_and_pay(&state, business_id, invoice_id, &req.card_token).await;

    match &result {
        Ok((status, body)) => {
            idempotency::finalize(
                &state.db,
                business_id,
                &idem_key,
                *status,
                body,
                Some(invoice_id),
                None,
            )
            .await?;
        }
        Err(app_err) => {
            let body = json!({ "error": { "code": app_err.code, "message": app_err.message } });
            idempotency::finalize(
                &state.db,
                business_id,
                &idem_key,
                app_err.status,
                &body,
                Some(invoice_id),
                None,
            )
            .await?;
        }
    }

    result.map(|(status, body)| (status, Json(body)))
}

async fn try_claim_and_pay(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    card_token: &str,
) -> Result<(StatusCode, serde_json::Value), AppError> {
    // Step 1: short claim transaction. Locks the invoice row, validates
    // state, inserts the 'pending' attempt, commits fast.
    let attempt_id = claim_payment_attempt(state, business_id, invoice_id, card_token).await?;

    // Step 2: spawn the actual PSP call + finalization, detached from this
    // request. tokio::spawn means this keeps running even if we stop
    // awaiting it below.
    let psp = state.psp.clone();
    let pool = state.db.clone();
    let total_cents = fetch_total_cents(state, invoice_id).await?;
    let card_token_owned = card_token.to_string();

    let handle = tokio::spawn(async move {
        let outcome = psp.charge(total_cents, &card_token_owned).await;
        finalize_payment_attempt(&pool, invoice_id, attempt_id, outcome).await
    });

    match tokio::time::timeout(state.psp_sync_wait, handle).await {
        Ok(Ok(Ok(body))) => {
            let status = StatusCode::OK;
            Ok((status, serde_json::to_value(body).unwrap()))
        }
        Ok(Ok(Err(app_err))) => Err(app_err),
        Ok(Err(join_err)) => {
            tracing::error!(error = %join_err, "payment finalization task panicked");
            Err(AppError::internal("payment processing failed unexpectedly"))
        }
        Err(_elapsed) => {
            // Still in flight. The spawned task above is NOT cancelled by
            // this timeout - it owns its own clone of the pool and PSP
            // client and will finalize the attempt whenever the PSP
            // eventually responds (or the PSP client's own hard timeout
            // gives up). The caller polls GET .../payment-attempts/{id} or
            // watches for the invoice.paid / invoice.payment_failed webhook.
            Ok((
                StatusCode::ACCEPTED,
                json!({
                    "attempt_id": attempt_id,
                    "invoice_id": invoice_id,
                    "status": "pending",
                    "message": "the payment processor has not responded yet; poll GET /invoices/{invoice_id}/payment-attempts/{attempt_id} for the final result"
                }),
            ))
        }
    }
}

async fn claim_payment_attempt(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    card_token: &str,
) -> Result<Uuid, AppError> {
    let mut tx = state.db.begin().await?;

    let row = sqlx::query("SELECT state FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE")
        .bind(invoice_id)
        .bind(business_id)
        .fetch_optional(&mut *tx)
        .await?;

    let Some(row) = row else {
        return Err(AppError::not_found("invoice_not_found", "no such invoice"));
    };
    let state_str: String = row.get("state");
    let invoice_state = InvoiceState::parse(&state_str)
        .ok_or_else(|| AppError::internal("invoice has an unrecognized state"))?;

    if !invoice_state.can_accept_payment() {
        let code = if invoice_state == InvoiceState::Paid {
            "invoice_already_paid"
        } else {
            "invoice_not_payable"
        };
        return Err(AppError::new(
            StatusCode::CONFLICT,
            code,
            format!("invoice is '{invoice_state}' and cannot accept a payment"),
        ));
    }

    let attempt_id: Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO payment_attempts (invoice_id, status, card_token)
         VALUES ($1, 'pending', $2) RETURNING id",
    )
    .bind(invoice_id)
    .bind(card_token)
    .fetch_one(&mut *tx)
    .await;

    let attempt_id = match attempt_id {
        Ok(id) => id,
        Err(sqlx::Error::Database(db_err)) if db_err.constraint() == Some("uniq_payment_attempts_pending_per_invoice") => {
            return Err(AppError::conflict(
                "payment_attempt_in_progress",
                "another payment attempt for this invoice is already in flight",
            ));
        }
        Err(e) => return Err(e.into()),
    };

    tx.commit().await?;
    Ok(attempt_id)
}

async fn fetch_total_cents(state: &AppState, invoice_id: Uuid) -> Result<i64, AppError> {
    let total: i64 = sqlx::query_scalar("SELECT total_cents FROM invoices WHERE id = $1")
        .bind(invoice_id)
        .fetch_one(&state.db)
        .await?;
    Ok(total)
}

async fn finalize_payment_attempt(
    pool: &sqlx::PgPool,
    invoice_id: Uuid,
    attempt_id: Uuid,
    outcome: PspOutcome,
) -> Result<PayResultBody, AppError> {
    let mut tx = pool.begin().await?;

    let (status, failure_code, psp_ref, invoice_target_state, event_type) = match &outcome {
        PspOutcome::Succeeded { psp_ref } => (
            "succeeded",
            None,
            Some(psp_ref.clone()),
            Some("paid"),
            "invoice.paid",
        ),
        PspOutcome::Declined { code } => (
            "failed",
            Some(code.clone()),
            None,
            None,
            "invoice.payment_failed",
        ),
        PspOutcome::Unavailable { detail } => {
            tracing::warn!(invoice_id = %invoice_id, attempt_id = %attempt_id, %detail, "psp unavailable");
            (
                "failed",
                Some("psp_unavailable".to_string()),
                None,
                None,
                "invoice.payment_failed",
            )
        }
    };

    // Status-conditional: only finalize an attempt that is still 'pending'.
    // Guards against this ever running twice for the same attempt.
    let updated = sqlx::query(
        "UPDATE payment_attempts SET status = $2, failure_code = $3, psp_ref = $4, updated_at = now()
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(attempt_id)
    .bind(status)
    .bind(&failure_code)
    .bind(&psp_ref)
    .execute(&mut *tx)
    .await?;

    if updated.rows_affected() == 0 {
        // Already finalized by a previous call - nothing further to do.
        tx.rollback().await.ok();
        return Err(AppError::internal("payment attempt was already finalized"));
    }

    if let Some(target) = invoice_target_state {
        // Status-conditional: only move open -> paid. If the invoice is no
        // longer 'open' (shouldn't happen given the pending-attempt lock,
        // but defended anyway) this simply does nothing rather than
        // corrupting state.
        sqlx::query("UPDATE invoices SET state = $2, updated_at = now() WHERE id = $1 AND state = 'open'")
            .bind(invoice_id)
            .bind(target)
            .execute(&mut *tx)
            .await?;
    }

    let payload = json!({
        "event": event_type,
        "invoice_id": invoice_id,
        "attempt_id": attempt_id,
        "status": status,
        "failure_code": failure_code,
        "psp_ref": psp_ref,
    });
    let business_id: Uuid = sqlx::query_scalar("SELECT business_id FROM invoices WHERE id = $1")
        .bind(invoice_id)
        .fetch_one(&mut *tx)
        .await?;
    webhook_dispatcher::enqueue_event(&mut tx, business_id, event_type, payload).await?;

    tx.commit().await?;

    Ok(PayResultBody {
        attempt_id,
        invoice_id,
        status: status.to_string(),
        failure_code,
        psp_ref,
    })
}

pub async fn get_payment_attempt(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Path((invoice_id, attempt_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PaymentAttemptRow>, AppError> {
    let attempt = sqlx::query_as::<_, PaymentAttemptRow>(
        "SELECT pa.id, pa.invoice_id, pa.status, pa.card_token, pa.failure_code, pa.psp_ref,
                pa.created_at, pa.updated_at
         FROM payment_attempts pa
         JOIN invoices i ON i.id = pa.invoice_id
         WHERE pa.id = $1 AND pa.invoice_id = $2 AND i.business_id = $3",
    )
    .bind(attempt_id)
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::not_found("payment_attempt_not_found", "no such payment attempt"))?;

    Ok(Json(attempt))
}
