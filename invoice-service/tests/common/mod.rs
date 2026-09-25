#![allow(dead_code)] // each test binary only uses a subset of these helpers

//! Shared integration-test scaffolding.
//!
//! Spins up the *real* router (`invoice_service::build_router`) on a random
//! local port, backed by a real Postgres database, alongside a tiny
//! in-process stand-in for the mock PSP (same token contract as the
//! standalone `mock-psp` binary, just embedded here so tests don't need to
//! shell out to a second process). This means the tests below exercise the
//! actual handlers, actual SQL, and actual concurrency control - nothing
//! about payments or idempotency is mocked, only the PSP's network call is.

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use invoice_service::{build_router, psp_client::PspClient, webhook_dispatcher, AppState};
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
struct MockPspState {
    timeout_delay: Duration,
}

#[derive(Deserialize)]
struct ChargeReq {
    card_token: String,
    #[allow(dead_code)]
    amount_cents: i64,
    #[allow(dead_code)]
    currency: String,
}

async fn mock_charge(State(state): State<MockPspState>, Json(req): Json<ChargeReq>) -> impl IntoResponse {
    match req.card_token.as_str() {
        "tok_success" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Json(serde_json::json!({"status": "succeeded", "psp_ref": Uuid::new_v4()})).into_response()
        }
        "tok_insufficient_funds" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Json(serde_json::json!({"status": "failed", "code": "insufficient_funds"})).into_response()
        }
        "tok_card_declined" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Json(serde_json::json!({"status": "failed", "code": "card_declined"})).into_response()
        }
        "tok_timeout" => {
            tokio::time::sleep(state.timeout_delay).await;
            Json(serde_json::json!({"status": "succeeded", "psp_ref": Uuid::new_v4()})).into_response()
        }
        "tok_network_error" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "simulated_network_error"})),
        )
            .into_response(),
        _ => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "invalid_token"})),
        )
            .into_response(),
    }
}

async fn start_mock_psp(timeout_delay: Duration) -> String {
    let state = MockPspState { timeout_delay };
    let app = Router::new().route("/charges", post(mock_charge)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

pub struct TestApp {
    pub base_url: String,
    pub api_key: String,
    pub client: reqwest::Client,
}

/// Default setup: mock PSP's tok_timeout case sleeps 3s (long enough to
/// prove the handler doesn't just get lucky, short enough to keep the test
/// suite fast). Use `spawn_app_with_psp_timeout` to control this directly.
pub async fn spawn_app() -> TestApp {
    spawn_app_with_psp_timeout(Duration::from_secs(3)).await
}

pub async fn spawn_app_with_psp_timeout(psp_timeout_delay: Duration) -> TestApp {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost:5432/dodo_invoice_test".to_string());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("connect to TEST_DATABASE_URL (create it first, e.g. `createdb dodo_invoice_test`)");
    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations against test database");

    let psp_base_url = start_mock_psp(psp_timeout_delay).await;
    // Sync wait is short in tests so the "PSP is slow" test doesn't need to
    // wait long to observe the 202/pending behavior; hard timeout just needs
    // to comfortably outlast psp_timeout_delay.
    let psp = PspClient::new(psp_base_url, psp_timeout_delay + Duration::from_secs(5));

    let state = AppState {
        db: pool.clone(),
        psp,
        psp_sync_wait: Duration::from_millis(800),
    };

    tokio::spawn(webhook_dispatcher::run_dispatcher(pool.clone(), reqwest::Client::new()));

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let base_url = format!("http://{addr}");
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .post(format!("{base_url}/businesses"))
        .json(&serde_json::json!({ "name": format!("Test Biz {}", Uuid::new_v4()) }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    TestApp {
        base_url,
        api_key: resp["api_key"].as_str().unwrap().to_string(),
        client,
    }
}

impl TestApp {
    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.header("Authorization", format!("Bearer {}", self.api_key))
    }

    pub async fn create_customer(&self) -> Uuid {
        let resp: serde_json::Value = self
            .auth(self.client.post(format!("{}/customers", self.base_url)))
            .json(&serde_json::json!({ "name": "Test Customer", "email": "test@example.com" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Uuid::parse_str(resp["id"].as_str().unwrap()).unwrap()
    }

    pub async fn create_invoice(&self, customer_id: Uuid, unit_amount_cents: i64) -> Uuid {
        let resp: serde_json::Value = self
            .auth(self.client.post(format!("{}/invoices", self.base_url)))
            .json(&serde_json::json!({
                "customer_id": customer_id,
                "line_items": [{"description": "Test item", "quantity": 1, "unit_amount_cents": unit_amount_cents}]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Uuid::parse_str(resp["id"].as_str().unwrap()).unwrap()
    }

    pub async fn finalize(&self, invoice_id: Uuid) {
        let resp = self
            .auth(self.client.post(format!("{}/invoices/{}/finalize", self.base_url, invoice_id)))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "finalize should succeed on a fresh draft invoice");
    }

    pub async fn pay(&self, invoice_id: Uuid, idempotency_key: &str, card_token: &str) -> reqwest::Response {
        self.auth(self.client.post(format!("{}/invoices/{}/pay", self.base_url, invoice_id)))
            .header("Idempotency-Key", idempotency_key)
            .json(&serde_json::json!({ "card_token": card_token }))
            .send()
            .await
            .unwrap()
    }

    pub async fn get_invoice(&self, invoice_id: Uuid) -> serde_json::Value {
        self.auth(self.client.get(format!("{}/invoices/{}", self.base_url, invoice_id)))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    pub async fn get_payment_attempt(&self, invoice_id: Uuid, attempt_id: &str) -> serde_json::Value {
        self.auth(self.client.get(format!(
            "{}/invoices/{}/payment-attempts/{}",
            self.base_url, invoice_id, attempt_id
        )))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
    }
}
