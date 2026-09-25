//! API key authentication.
//!
//! Keys look like `sk_live_<48 hex chars>`. The first 12 hex chars after the
//! `sk_live_` marker are stored in the clear as `key_prefix` purely so we can
//! do an indexed lookup; they carry no authenticating power on their own.
//! The rest of the secret is hashed with argon2id before it ever touches the
//! database, so a leaked database dump does not hand out usable keys.
//!
//! See DESIGN.md section 5 for the full threat-model discussion.

use crate::{error::AppError, AppState};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use rand::RngCore;
use uuid::Uuid;

const KEY_PREFIX_MARKER: &str = "sk_live_";
const PREFIX_LEN: usize = 12;

/// Generates a new API key. Returns (full_key_to_show_once, prefix, hash_to_store).
pub fn generate_api_key() -> (String, String, String) {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let secret_hex = hex::encode(bytes);
    let full_key = format!("{KEY_PREFIX_MARKER}{secret_hex}");
    let prefix = secret_hex[..PREFIX_LEN].to_string();
    let hash = hash_secret(&secret_hex);
    (full_key, prefix, hash)
}

pub fn hash_secret(secret: &str) -> String {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .expect("argon2 hashing failed")
        .to_string()
}

pub fn verify_secret(secret: &str, hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(secret.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Extracted from `Authorization: Bearer sk_live_...`. Presence of this
/// extractor on a handler is what scopes every query to the caller's
/// business_id - there is no other way to reach a handler that needs it.
pub struct AuthedBusiness(pub Uuid);

#[async_trait::async_trait]
impl FromRequestParts<AppState> for AuthedBusiness {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                AppError::new(StatusCode::UNAUTHORIZED, "missing_api_key", "missing Authorization header")
            })?;

        let token = header.strip_prefix("Bearer ").ok_or_else(|| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                "malformed_authorization_header",
                "expected 'Authorization: Bearer <api_key>'",
            )
        })?;

        let secret = token.strip_prefix(KEY_PREFIX_MARKER).ok_or_else(|| {
            AppError::unauthorized("invalid_api_key", "unrecognized API key format")
        })?;

        if secret.len() < PREFIX_LEN {
            return Err(AppError::unauthorized("invalid_api_key", "malformed API key"));
        }
        let prefix = &secret[..PREFIX_LEN];

        let row = sqlx::query_as::<_, (Uuid, String, Option<chrono::DateTime<chrono::Utc>>)>(
            "SELECT business_id, key_hash, revoked_at FROM api_keys WHERE key_prefix = $1",
        )
        .bind(prefix)
        .fetch_optional(&state.db)
        .await?;

        let Some((business_id, key_hash, revoked_at)) = row else {
            return Err(AppError::unauthorized("invalid_api_key", "API key not recognized"));
        };

        if revoked_at.is_some() {
            return Err(AppError::unauthorized("revoked_api_key", "API key has been revoked"));
        }

        if !verify_secret(secret, &key_hash) {
            return Err(AppError::unauthorized("invalid_api_key", "API key not recognized"));
        }

        Ok(AuthedBusiness(business_id))
    }
}
