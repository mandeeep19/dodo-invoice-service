use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub port: u16,
    pub psp_base_url: String,
    /// How long the request handler waits synchronously for the PSP to
    /// answer before detaching and returning 202 "pending" to the caller.
    /// Deliberately shorter than the mock PSP's 30s tok_timeout case.
    pub psp_sync_wait: Duration,
    /// Hard cap on how long we'll wait for the PSP at all, sync or async.
    pub psp_hard_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            database_url: std::env::var("DATABASE_URL")
                .expect("DATABASE_URL must be set"),
            port: std::env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8080),
            psp_base_url: std::env::var("PSP_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:9000".to_string()),
            psp_sync_wait: Duration::from_millis(
                std::env::var("PSP_SYNC_WAIT_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(3_000),
            ),
            psp_hard_timeout: Duration::from_millis(
                std::env::var("PSP_HARD_TIMEOUT_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(35_000),
            ),
        }
    }
}
