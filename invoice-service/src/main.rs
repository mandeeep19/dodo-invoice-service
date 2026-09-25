use invoice_service::{build_router, config::Config, connect_with_retry, psp_client::PspClient, webhook_dispatcher, AppState};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env();

    let pool = connect_with_retry(&config.database_url).await;

    tracing::info!("running migrations");
    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations");

    let psp = PspClient::new(config.psp_base_url.clone(), config.psp_hard_timeout);

    let state = AppState {
        db: pool.clone(),
        psp,
        psp_sync_wait: config.psp_sync_wait,
    };

    // Webhook delivery runs completely independently of the request path -
    // it is a background loop over its own database connection and its own
    // HTTP client, started once at boot.
    let dispatcher_http = reqwest::Client::builder()
        .build()
        .expect("failed to build webhook dispatcher http client");
    tokio::spawn(webhook_dispatcher::run_dispatcher(pool.clone(), dispatcher_http));

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port))
        .await
        .expect("failed to bind listener");
    tracing::info!("invoice-service listening on :{}", config.port);
    axum::serve(listener, app).await.expect("server error");
}
