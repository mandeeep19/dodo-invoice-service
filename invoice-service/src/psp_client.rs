//! Thin HTTP client for the mock PSP. Treated as a real, untrusted external
//! dependency: bounded timeout, explicit handling for every failure shape
//! the spec calls out (slow, 500, dropped connection).

use serde::Deserialize;
use std::time::Duration;

#[derive(Debug)]
pub enum PspOutcome {
    Succeeded { psp_ref: String },
    Declined { code: String },
    /// The PSP itself errored (500 / dropped connection) rather than
    /// rendering a payment decision. Distinct from `Declined`: this is our
    /// infrastructure's fault or theirs, not a statement about the card.
    Unavailable { detail: String },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ChargeResponse {
    Succeeded {
        #[allow(dead_code)]
        status: String,
        psp_ref: String,
    },
    Failed {
        #[allow(dead_code)]
        status: String,
        code: String,
    },
}

#[derive(Clone)]
pub struct PspClient {
    client: reqwest::Client,
    base_url: String,
}

impl PspClient {
    pub fn new(base_url: String, hard_timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(hard_timeout)
            .build()
            .expect("failed to build PSP http client");
        Self { client, base_url }
    }

    /// Makes exactly one charge call. Callers are responsible for deciding
    /// how long to wait for this future before treating the attempt as
    /// still-pending (see `payments.rs`); this method itself only enforces
    /// the hard outer timeout so a truly hung connection cannot leak forever.
    pub async fn charge(&self, amount_cents: i64, card_token: &str) -> PspOutcome {
        let url = format!("{}/charges", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "amount_cents": amount_cents,
                "currency": "USD",
                "card_token": card_token,
            }))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                return PspOutcome::Unavailable {
                    detail: format!("psp request failed: {e}"),
                }
            }
        };

        if resp.status().is_server_error() {
            // A 5xx (or, per tok_network_error, exactly this) means the PSP
            // itself is down - an infrastructure problem, not a statement
            // about the card. Distinct from the 4xx case below.
            let status = resp.status();
            return PspOutcome::Unavailable {
                detail: format!("psp returned HTTP {status}"),
            };
        }

        if resp.status().is_client_error() {
            // The PSP rejected the request itself (e.g. a token it doesn't
            // recognize). This is a decision about the payment/request, not
            // an availability problem, so it's a Declined outcome with
            // whatever code the PSP gave us - never lumped in with
            // "psp_unavailable", which should be reserved for genuine
            // outages an operator would want to alert on.
            let code = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|body| {
                    body.get("code")
                        .or_else(|| body.get("error"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "psp_rejected_request".to_string());
            return PspOutcome::Declined { code };
        }

        match resp.json::<ChargeResponse>().await {
            Ok(ChargeResponse::Succeeded { psp_ref, .. }) => PspOutcome::Succeeded { psp_ref },
            Ok(ChargeResponse::Failed { code, .. }) => PspOutcome::Declined { code },
            Err(e) => PspOutcome::Unavailable {
                detail: format!("psp returned an unparseable body: {e}"),
            },
        }
    }
}
