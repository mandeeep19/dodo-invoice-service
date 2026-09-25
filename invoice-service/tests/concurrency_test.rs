//! Required test #1: N concurrent POST /pay for the same invoice must yield
//! at most one success, no double-charge, and a consistent final state.
//! See DESIGN.md section 3(a) for the mechanism this exercises: a row lock
//! plus a DB-level partial unique index on "one pending attempt per invoice".

mod common;

use common::spawn_app;
use futures::future::join_all;

#[tokio::test]
async fn only_one_of_n_concurrent_payments_succeeds() {
    let app = spawn_app().await;
    let customer_id = app.create_customer().await;
    let invoice_id = app.create_invoice(customer_id, 1_000).await;
    app.finalize(invoice_id).await;

    const N: usize = 10;
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let client = app.client.clone();
        let url = format!("{}/invoices/{}/pay", app.base_url, invoice_id);
        let api_key = app.api_key.clone();
        // Each request uses a DISTINCT idempotency key - this is a genuine
        // race between N different payment attempts, not N replays of one
        // idempotent request (that's a separate, easier case tested in
        // idempotency_test.rs).
        handles.push(tokio::spawn(async move {
            client
                .post(url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Idempotency-Key", format!("concurrency-test-{i}"))
                .json(&serde_json::json!({ "card_token": "tok_success" }))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }

    let statuses: Vec<u16> = join_all(handles).await.into_iter().map(|r| r.unwrap()).collect();
    let success_count = statuses.iter().filter(|&&s| s == 200).count();
    let conflict_count = statuses.iter().filter(|&&s| s == 409).count();

    assert_eq!(
        success_count, 1,
        "exactly one concurrent payment must succeed, got statuses: {statuses:?}"
    );
    assert_eq!(
        conflict_count,
        N - 1,
        "every other concurrent payment must be rejected with 409, got statuses: {statuses:?}"
    );

    let invoice = app.get_invoice(invoice_id).await;
    assert_eq!(invoice["state"], "paid", "final invoice state must be consistent: paid, exactly once");
}
