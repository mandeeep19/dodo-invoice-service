//! Webhook signing, enqueueing (transactional outbox), and the background
//! delivery loop. See DESIGN.md section 4 for the full design rationale.
//!
//! Retry schedule (attempt N fails -> wait -> attempt N+1):
//!   1 -> 30s -> 2 -> 5min -> 3 -> 30min -> 4 -> 2h -> 5 -> 12h -> 6 -> 24h -> 7 (final)
//! Seven attempts total, ~39 hours end to end. After the 7th failure the
//! delivery is marked `failed_exhausted` and stops retrying.

use crate::error::AppError;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::time::Duration;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const MAX_ATTEMPTS: i32 = 7;
const BACKOFF_SECONDS: [i64; 6] = [30, 300, 1800, 7_200, 43_200, 86_400];

/// Signs a payload the same way Stripe-style webhooks do: HMAC-SHA256 over
/// `"{timestamp}.{raw_json_body}"`, so a receiver can (a) verify the body
/// wasn't tampered with and (b) reject old signatures as replays by
/// checking the timestamp is within their own tolerance window.
pub fn sign(secret: &str, timestamp: i64, payload: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(format!("{timestamp}.{payload}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Enqueues `event_type` for every enabled webhook endpoint on the business,
/// inside the caller's transaction. Because this write lands in the same
/// transaction as the invoice/customer state change that triggered it, the
/// event is durable the instant that transaction commits - a crash between
/// "invoice marked paid" and "webhook enqueued" cannot happen; they either
/// both happened or neither did. Actual HTTP delivery happens later, out of
/// band, by `run_dispatcher`.
pub async fn enqueue_event(
    tx: &mut Transaction<'_, Postgres>,
    business_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<(), AppError> {
    let endpoint_ids: Vec<Uuid> = sqlx::query(
        "SELECT id FROM webhook_endpoints WHERE business_id = $1 AND disabled_at IS NULL",
    )
    .bind(business_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| row.get::<Uuid, _>("id"))
    .collect();

    for endpoint_id in endpoint_ids {
        sqlx::query(
            "INSERT INTO webhook_deliveries (endpoint_id, business_id, event_type, payload)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(endpoint_id)
        .bind(business_id)
        .bind(event_type)
        .bind(&payload)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

struct DueDelivery {
    id: Uuid,
    endpoint_url: String,
    endpoint_secret: String,
    event_type: String,
    payload: serde_json::Value,
    attempt_count: i32,
}

/// Runs forever, polling for due deliveries and sending them. Meant to be
/// spawned once as a background tokio task; deliberately isolated from any
/// HTTP request so a slow or unreachable receiver can never add latency to
/// the API response path.
pub async fn run_dispatcher(pool: PgPool, http: reqwest::Client) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    loop {
        ticker.tick().await;
        if let Err(e) = dispatch_due_batch(&pool, &http).await {
            tracing::error!(error = ?e, "webhook dispatch batch failed");
        }
    }
}

async fn dispatch_due_batch(pool: &PgPool, http: &reqwest::Client) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;

    // SKIP LOCKED means multiple dispatcher instances (e.g. several replicas
    // of this service) can run this same loop concurrently without ever
    // double-sending the same delivery.
    let rows = sqlx::query(
        "SELECT wd.id, we.url, we.secret, wd.event_type, wd.payload, wd.attempt_count
         FROM webhook_deliveries wd
         JOIN webhook_endpoints we ON we.id = wd.endpoint_id
         WHERE wd.status = 'pending' AND wd.next_attempt_at <= now()
         ORDER BY wd.next_attempt_at
         LIMIT 20
         FOR UPDATE OF wd SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await?;

    let due: Vec<DueDelivery> = rows
        .into_iter()
        .map(|row| DueDelivery {
            id: row.get("id"),
            endpoint_url: row.get("url"),
            endpoint_secret: row.get("secret"),
            event_type: row.get("event_type"),
            payload: row.get("payload"),
            attempt_count: row.get("attempt_count"),
        })
        .collect();

    // Mark them all as "claimed" for this tick by bumping nothing yet - we
    // hold the row locks until we decide the outcome below, then commit.
    for d in &due {
        send_and_record(&mut tx, http, d).await?;
    }

    tx.commit().await?;
    Ok(())
}

async fn send_and_record(
    tx: &mut Transaction<'_, Postgres>,
    http: &reqwest::Client,
    d: &DueDelivery,
) -> Result<(), AppError> {
    let payload_str = serde_json::to_string(&d.payload).unwrap_or_default();
    let ts = Utc::now().timestamp();
    let signature = sign(&d.endpoint_secret, ts, &payload_str);

    let send_result = http
        .post(&d.endpoint_url)
        .header("Content-Type", "application/json")
        .header("X-Webhook-Id", d.id.to_string())
        .header("X-Webhook-Event", &d.event_type)
        .header("X-Webhook-Signature", format!("t={ts},v1={signature}"))
        .timeout(Duration::from_secs(5))
        .body(payload_str)
        .send()
        .await;

    let outcome = match send_result {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!("receiver returned HTTP {}", resp.status())),
        Err(e) => Err(format!("delivery error: {e}")),
    };

    match outcome {
        Ok(()) => {
            sqlx::query(
                "UPDATE webhook_deliveries SET status = 'succeeded', updated_at = now() WHERE id = $1",
            )
            .bind(d.id)
            .execute(&mut **tx)
            .await?;
        }
        Err(err) => {
            let new_attempt_count = d.attempt_count + 1;
            if new_attempt_count >= MAX_ATTEMPTS {
                sqlx::query(
                    "UPDATE webhook_deliveries
                     SET status = 'failed_exhausted', attempt_count = $2, last_error = $3, updated_at = now()
                     WHERE id = $1",
                )
                .bind(d.id)
                .bind(new_attempt_count)
                .bind(&err)
                .execute(&mut **tx)
                .await?;
                tracing::warn!(delivery_id = %d.id, "webhook delivery exhausted retries");
            } else {
                let delay = BACKOFF_SECONDS[(new_attempt_count - 1) as usize];
                sqlx::query(
                    "UPDATE webhook_deliveries
                     SET attempt_count = $2, last_error = $3,
                         next_attempt_at = now() + make_interval(secs => $4),
                         updated_at = now()
                     WHERE id = $1",
                )
                .bind(d.id)
                .bind(new_attempt_count)
                .bind(&err)
                .bind(delay as f64)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(())
}
