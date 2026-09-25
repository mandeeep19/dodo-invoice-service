pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod idempotency;
pub mod models;
pub mod psp_client;
pub mod state_machine;
pub mod webhook_dispatcher;

use axum::{
    routing::{get, post},
    Router,
};
use psp_client::PspClient;
use std::time::Duration;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub psp: PspClient,
    pub psp_sync_wait: Duration,
}

/// Builds the full router. Shared by `main.rs` (real server) and the
/// integration tests (in-process server on a random port), so the tests
/// exercise the exact same routing and middleware as production.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/businesses", post(handlers::business::create_business))
        .route(
            "/customers",
            post(handlers::customers::create_customer).get(handlers::customers::list_customers),
        )
        .route("/customers/:id", get(handlers::customers::get_customer))
        .route(
            "/invoices",
            post(handlers::invoices::create_invoice).get(handlers::invoices::list_invoices),
        )
        .route("/invoices/:id", get(handlers::invoices::get_invoice))
        .route("/invoices/:id/finalize", post(handlers::invoices::finalize_invoice))
        .route("/invoices/:id/void", post(handlers::invoices::void_invoice))
        .route(
            "/invoices/:id/mark-uncollectible",
            post(handlers::invoices::mark_uncollectible),
        )
        .route("/invoices/:id/pay", post(handlers::payments::pay_invoice))
        .route(
            "/invoices/:id/payment-attempts/:attempt_id",
            get(handlers::payments::get_payment_attempt),
        )
        .route("/webhook-endpoints", post(handlers::webhooks::register_webhook))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// The database container may still be starting when this service boots
/// (docker-compose does not guarantee readiness, only start order), so we
/// retry the initial connection a few times instead of crash-looping.
pub async fn connect_with_retry(database_url: &str) -> sqlx::PgPool {
    use sqlx::postgres::PgPoolOptions;
    let mut attempts = 0;
    loop {
        match PgPoolOptions::new()
            .max_connections(20)
            .connect(database_url)
            .await
        {
            Ok(pool) => return pool,
            Err(e) if attempts < 10 => {
                attempts += 1;
                tracing::warn!(error = %e, attempt = attempts, "database not ready yet, retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => panic!("failed to connect to database after {attempts} attempts: {e}"),
        }
    }
}
