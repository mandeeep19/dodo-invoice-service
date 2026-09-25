//! Required test #3: a PSP failure (slow or erroring) must not leave the
//! invoice stuck in a bad state. Covers both tok_timeout and
//! tok_network_error, since the spec calls both out and they exercise two
//! different code paths (detach-and-poll vs. immediate failed attempt).
//! See DESIGN.md section 3(b) and 3(c).

mod common;

use common::spawn_app_with_psp_timeout;
use std::time::{Duration, Instant};

#[tokio::test]
async fn psp_timeout_does_not_hang_the_endpoint_and_resolves_cleanly_in_background() {
    // Mock PSP takes 3s to answer tok_timeout in this test (the real mock
    // PSP spec says 30s; the mechanism being tested - detach, respond fast,
    // resolve later - doesn't depend on the exact duration).
    let app = spawn_app_with_psp_timeout(Duration::from_secs(3)).await;
    let customer_id = app.create_customer().await;
    let invoice_id = app.create_invoice(customer_id, 500).await;
    app.finalize(invoice_id).await;

    let start = Instant::now();
    let resp = app.pay(invoice_id, "psp-timeout-test", "tok_timeout").await;
    let elapsed = start.elapsed();

    assert_eq!(resp.status(), 202, "a slow PSP must produce a 202/pending response, not a hang or an error");
    assert!(
        elapsed < Duration::from_secs(2),
        "the handler must return well before the PSP resolves (took {elapsed:?})"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    let attempt_id = body["attempt_id"].as_str().unwrap().to_string();

    // While the PSP call is still in flight, the invoice must not be
    // corrupted - it stays 'open', not stuck in some half-paid limbo.
    let invoice = app.get_invoice(invoice_id).await;
    assert_eq!(invoice["state"], "open");

    // A second payment attempt must be rejected while one is in flight -
    // this is the same guarantee the concurrency test checks, but here
    // specifically covering the "slow PSP" window.
    let second = app.pay(invoice_id, "psp-timeout-test-2", "tok_success").await;
    assert_eq!(second.status(), 409);

    // Once the background task's PSP call actually resolves, the attempt
    // and the invoice must both reflect the real outcome - this is how the
    // caller eventually finds out the result (poll GET, or the
    // invoice.paid webhook).
    tokio::time::sleep(Duration::from_secs(4)).await;
    let attempt = app.get_payment_attempt(invoice_id, &attempt_id).await;
    assert_eq!(attempt["status"], "succeeded");

    let invoice_after = app.get_invoice(invoice_id).await;
    assert_eq!(invoice_after["state"], "paid");
}

#[tokio::test]
async fn psp_network_error_leaves_invoice_open_and_retryable() {
    let app = spawn_app_with_psp_timeout(Duration::from_secs(3)).await;
    let customer_id = app.create_customer().await;
    let invoice_id = app.create_invoice(customer_id, 500).await;
    app.finalize(invoice_id).await;

    let resp = app.pay(invoice_id, "psp-network-error-test", "tok_network_error").await;
    assert_eq!(resp.status(), 200, "a PSP-side error is a resolved (failed) attempt, not a client error");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "failed");
    assert_eq!(body["failure_code"], "psp_unavailable");

    let invoice = app.get_invoice(invoice_id).await;
    assert_eq!(invoice["state"], "open", "a PSP outage must not corrupt or freeze the invoice's state");

    // Prove it is genuinely retryable, not just nominally 'open'.
    let retry = app.pay(invoice_id, "psp-network-error-retry", "tok_success").await;
    assert_eq!(retry.status(), 200);
    let invoice_after = app.get_invoice(invoice_id).await;
    assert_eq!(invoice_after["state"], "paid");
}
