//! Generic Idempotency-Key handling, used by POST /invoices/{id}/pay.
//!
//! Contract (see DESIGN.md section 3):
//!   - Same key + same request body -> replay the cached response, no new
//!     side effects, no second PSP call.
//!   - Same key + different request body -> 422, reject outright.
//!   - Same key fired twice concurrently -> the second caller waits briefly
//!     for the first to finish, then gets the same cached response.
//!
//! Implementation: a row is reserved with `response_status = NULL` before
//! any work happens, using an INSERT that will conflict if another request
//! already reserved that key. Once the work finishes, the row is finalized
//! with the actual response. A concurrent caller who sees a reserved-but-
//! unfinished row polls briefly rather than racing the PSP a second time.

use crate::error::AppError;
use axum::http::StatusCode;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

pub fn hash_body(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub enum IdempotencyCheck {
    /// A prior call with this exact key + body already completed; here is
    /// its response, verbatim, with no new work performed.
    UseCached {
        status: StatusCode,
        body: serde_json::Value,
    },
    /// No prior attempt exists (or we won the race to reserve it). Proceed
    /// with the real work, then call `finalize`.
    Proceed,
}

pub async fn reserve_or_get(
    pool: &PgPool,
    business_id: Uuid,
    key: &str,
    request_hash: &str,
) -> Result<IdempotencyCheck, AppError> {
    for attempt in 0..15u32 {
        let existing = sqlx::query_as::<_, (String, Option<i32>, Option<serde_json::Value>)>(
            "SELECT request_hash, response_status, response_body
             FROM idempotency_keys WHERE business_id = $1 AND idempotency_key = $2",
        )
        .bind(business_id)
        .bind(key)
        .fetch_optional(pool)
        .await?;

        if let Some((stored_hash, status, body)) = existing {
            if stored_hash != request_hash {
                return Err(AppError::unprocessable(
                    "idempotency_key_reused",
                    "this Idempotency-Key was already used with a different request body",
                ));
            }
            match (status, body) {
                (Some(status), Some(body)) => {
                    return Ok(IdempotencyCheck::UseCached {
                        status: StatusCode::from_u16(status as u16)
                            .unwrap_or(StatusCode::OK),
                        body,
                    });
                }
                _ => {
                    // Reserved by a concurrent request, not finished yet.
                    // Wait briefly and re-check rather than doing the work
                    // (and calling the PSP) a second time.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            }
        }

        // No row yet - try to reserve it. If another request wins the race,
        // the unique primary key on (business_id, idempotency_key) makes
        // this a no-op insert and we loop back to read what it wrote.
        let inserted = sqlx::query(
            "INSERT INTO idempotency_keys (business_id, idempotency_key, request_hash)
             VALUES ($1, $2, $3)
             ON CONFLICT (business_id, idempotency_key) DO NOTHING",
        )
        .bind(business_id)
        .bind(key)
        .bind(request_hash)
        .execute(pool)
        .await?;

        if inserted.rows_affected() == 1 {
            return Ok(IdempotencyCheck::Proceed);
        }
        // Someone else inserted first between our SELECT and INSERT; loop
        // and read their row.
        if attempt == 14 {
            break;
        }
    }

    Err(AppError::new(
        StatusCode::CONFLICT,
        "request_in_progress",
        "a request with this Idempotency-Key is still being processed; retry shortly",
    ))
}

pub async fn finalize<T: Serialize>(
    pool: &PgPool,
    business_id: Uuid,
    key: &str,
    status: StatusCode,
    body: &T,
    invoice_id: Option<Uuid>,
    payment_attempt_id: Option<Uuid>,
) -> Result<(), AppError> {
    let body_json = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
    sqlx::query(
        "UPDATE idempotency_keys
         SET response_status = $3, response_body = $4, invoice_id = $5, payment_attempt_id = $6
         WHERE business_id = $1 AND idempotency_key = $2",
    )
    .bind(business_id)
    .bind(key)
    .bind(status.as_u16() as i32)
    .bind(body_json)
    .bind(invoice_id)
    .bind(payment_attempt_id)
    .execute(pool)
    .await?;
    Ok(())
}
