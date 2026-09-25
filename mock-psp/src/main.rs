use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    timeout_delay: Duration,
}

#[derive(Deserialize)]
struct ChargeRequest {
    #[allow(dead_code)]
    amount_cents: i64,
    #[allow(dead_code)]
    currency: String,
    card_token: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ChargeResponse {
    Succeeded { status: &'static str, psp_ref: Uuid },
    Failed { status: &'static str, code: &'static str },
}

async fn charge(
    State(state): State<AppState>,
    Json(req): Json<ChargeRequest>,
) -> Result<Json<ChargeResponse>, (StatusCode, Json<serde_json::Value>)> {
    match req.card_token.as_str() {
        "tok_success" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(Json(ChargeResponse::Succeeded {
                status: "succeeded",
                psp_ref: Uuid::new_v4(),
            }))
        }
        "tok_insufficient_funds" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(Json(ChargeResponse::Failed {
                status: "failed",
                code: "insufficient_funds",
            }))
        }
        "tok_card_declined" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(Json(ChargeResponse::Failed {
                status: "failed",
                code: "card_declined",
            }))
        }
        "tok_timeout" => {
            // The whole point: this must outlast any sane client timeout.
            // The invoice-service is expected to give up on this long before
            // it resolves, and treat it as an in-flight/unknown outcome.
            tokio::time::sleep(state.timeout_delay).await;
            Ok(Json(ChargeResponse::Succeeded {
                status: "succeeded",
                psp_ref: Uuid::new_v4(),
            }))
        }
        "tok_network_error" => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "simulated_network_error" })),
        )),
        _ => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "invalid_token" })),
        )),
    }
}

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // MOCK_PSP_TIMEOUT_MS lets tests shrink the tok_timeout delay; production
    // use leaves it at the spec's 30s.
    let timeout_ms: u64 = std::env::var("MOCK_PSP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);

    let state = AppState {
        timeout_delay: Duration::from_millis(timeout_ms),
    };

    let app = Router::new()
        .route("/charges", post(charge))
        .route("/health", axum::routing::get(health))
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind mock-psp listener");
    tracing::info!("mock-psp listening on :{port}");
    axum::serve(listener, app).await.expect("server error");
}
