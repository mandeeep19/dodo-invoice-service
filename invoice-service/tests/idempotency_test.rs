//! Required test #2: retrying the same request with the same Idempotency-Key
//! must return the same response without a second PSP call. See DESIGN.md
//! section 3(d) for the "different body, same key" case.

mod common;

use common::spawn_app;

#[tokio::test]
async fn retry_with_same_key_and_body_returns_identical_response() {
    let app = spawn_app().await;
    let customer_id = app.create_customer().await;
    let invoice_id = app.create_invoice(customer_id, 500).await;
    app.finalize(invoice_id).await;

    let first = app.pay(invoice_id, "idem-replay-test", "tok_success").await;
    assert_eq!(first.status(), 200);
    let first_body: serde_json::Value = first.json().await.unwrap();

    let second = app.pay(invoice_id, "idem-replay-test", "tok_success").await;
    assert_eq!(second.status(), 200);
    let second_body: serde_json::Value = second.json().await.unwrap();

    // Same attempt_id and same psp_ref proves the second call replayed the
    // cached response rather than re-entering the claim logic and calling
    // the PSP a second time (a second real charge would mint a new
    // attempt_id and a new psp_ref every time, since the mock PSP generates
    // a fresh UUID per call).
    assert_eq!(first_body, second_body, "a replayed request must return the exact cached response");

    let invoice = app.get_invoice(invoice_id).await;
    assert_eq!(invoice["state"], "paid");
}

#[tokio::test]
async fn same_key_with_different_body_is_rejected() {
    let app = spawn_app().await;
    let customer_id = app.create_customer().await;
    let invoice_id = app.create_invoice(customer_id, 500).await;
    app.finalize(invoice_id).await;

    let first = app.pay(invoice_id, "idem-conflict-test", "tok_success").await;
    assert_eq!(first.status(), 200);

    // Same key, different card_token -> must not be silently accepted or
    // silently replayed; it must be rejected outright.
    let second = app.pay(invoice_id, "idem-conflict-test", "tok_card_declined").await;
    assert_eq!(second.status(), 422);
    let body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(body["error"]["code"], "idempotency_key_reused");
}
