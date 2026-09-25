# Dodo Payments - Invoice & Payment Service

A minimal invoice and payment service: businesses authenticate with an API
key, create customers and invoices, and accept payments through a mock PSP.
See [DESIGN.md](./DESIGN.md) for the state machine, concurrency model,
webhook design, and the failure-mode reasoning - that document is the
primary deliverable for this assignment. See [AI_USAGE.md](./AI_USAGE.md)
for how AI was used to build this.

## Stack

Rust, [Axum](https://github.com/tokio-rs/axum), [sqlx](https://github.com/launchbadge/sqlx) (runtime-checked queries, not compile-time macros - keeps the Docker build independent of a live database), PostgreSQL, Docker Compose.

## Running it

```bash
docker compose up --build
```

That's the whole setup: it builds and starts three containers -
`postgres`, `mock-psp` (the fake card processor, port 9000), and
`invoice-service` (port 8080). The service runs its own database migrations
on boot; no manual migration step is needed. Give it ~10-20 seconds on first
run while Postgres initializes and both Rust binaries finish compiling
inside the build stage.

Health check once it's up:

```bash
curl http://localhost:8080/health   # -> ok
curl http://localhost:9000/health   # -> ok
```

### Running tests locally (outside Docker)

The test suite needs a real Postgres instance (it exercises real
transactions and the real partial-unique-index concurrency guard - nothing
about the database is mocked).

If `docker compose up` is already running, you can create the test DB in the container and point to port `5433`:

```bash
docker compose exec postgres psql -U dodo -d dodo_invoice -c "CREATE DATABASE dodo_invoice_test;"
export TEST_DATABASE_URL=postgres://dodo:dodo@localhost:5433/dodo_invoice_test
cargo test -p invoice-service
```

Or using a standard local Postgres instance:

```bash
createdb dodo_invoice_test
export TEST_DATABASE_URL=postgres://localhost:5432/dodo_invoice_test
cargo test -p invoice-service
```

Three tests are required by the assignment and all three exist:
`tests/concurrency_test.rs` (N concurrent payments, exactly one succeeds),
`tests/idempotency_test.rs` (replay returns the identical cached response;
a reused key with a different body is rejected), and
`tests/psp_failure_test.rs` (covers both `tok_timeout` and
`tok_network_error` - the invoice is never left stuck). Nothing was skipped.

## curl walkthrough

All examples assume the service is running on `localhost:8080` via
`docker compose up`.

**1. Create a business (bootstraps an API key - see DESIGN.md section 5 for why this endpoint exists and isn't a full auth system):**

```bash
curl -s -X POST http://localhost:8080/businesses \
  -H 'Content-Type: application/json' \
  -d '{"name": "Acme Inc"}'
# => {"business_id":"...","name":"Acme Inc","api_key":"sk_live_...."}
# Save the api_key - it is shown exactly once.
export API_KEY=sk_live_...
```

**2. Create a customer:**

```bash
curl -s -X POST http://localhost:8080/customers \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name": "Jane Doe", "email": "jane@example.com"}'
# => {"id":"<customer_id>","business_id":"...","name":"Jane Doe","email":"jane@example.com","created_at":"..."}
export CUSTOMER_ID=<paste the id above>
```

**3. Create an invoice (the server computes total_cents from line items - never send a total yourself):**

```bash
curl -s -X POST http://localhost:8080/invoices \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"customer_id\": \"$CUSTOMER_ID\", \"line_items\": [
        {\"description\": \"Widget\", \"quantity\": 3, \"unit_amount_cents\": 500},
        {\"description\": \"Gadget\", \"quantity\": 1, \"unit_amount_cents\": 1999}
      ]}"
# => total_cents: 3499  (3*500 + 1999, computed server-side, integer cents throughout)
export INVOICE_ID=<paste the id above>

# Invoices start in 'draft'. Finalize to move to 'open' (payable):
curl -s -X POST http://localhost:8080/invoices/$INVOICE_ID/finalize \
  -H "Authorization: Bearer $API_KEY"
```

**4a. Attempt payment - success:**

```bash
curl -s -X POST http://localhost:8080/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: demo-pay-1' \
  -d '{"card_token": "tok_success"}'
# => {"attempt_id":"...","invoice_id":"...","status":"succeeded","failure_code":null,"psp_ref":"..."}
```

**4b. Attempt payment - failure cases:**

*Case 1: Card declined on an open invoice (stays 'open', retryable)*

```bash
# Create and finalize a second invoice to demonstrate payment decline:
export INVOICE_ID_2=$(curl -s -X POST http://localhost:8080/invoices \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"customer_id\": \"$CUSTOMER_ID\", \"line_items\": [{\"description\": \"Book\", \"quantity\": 1, \"unit_amount_cents\": 1200}]}" | jq -r .id)

curl -s -X POST http://localhost:8080/invoices/$INVOICE_ID_2/finalize \
  -H "Authorization: Bearer $API_KEY"

# Pay with tok_card_declined:
curl -s -X POST http://localhost:8080/invoices/$INVOICE_ID_2/pay \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: demo-pay-decline' \
  -d '{"card_token": "tok_card_declined"}'
# => {"attempt_id":"...","invoice_id":"...","status":"failed","failure_code":"card_declined","psp_ref":null}
# (The invoice remains 'open' and can be retried with a different card)
```

*Case 2: Paying an invoice that is already paid (terminal state rejection)*

```bash
curl -s -X POST http://localhost:8080/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: demo-pay-already-paid' \
  -d '{"card_token": "tok_success"}'
# => {"error":{"code":"invoice_already_paid","message":"invoice is 'paid' and cannot accept a payment"}}
```

**5. Register a webhook endpoint and watch signed deliveries arrive** (run a
local receiver first, e.g. `python3 -m http.server` won't show headers -
any endpoint that logs request headers/body works, or use
[webhook.site](https://webhook.site) for a quick manual check):

```bash
curl -s -X POST http://localhost:8080/webhook-endpoints \
  -H "Authorization: Bearer $API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://webhook.site/<your-id>"}'
# => {"id":"...","url":"...","secret":"whsec_...","created_at":"..."}
# Save the secret to verify X-Webhook-Signature (HMAC-SHA256 of "{t}.{body}") on received events.
```

## Notes on scope decisions

- **Idempotency-Key is required** on `POST /pay` (returns `400` if missing)
  - the assignment requires idempotent payments, and an optional key that's
    silently ignored isn't idempotent.
- **`PSP_SYNC_WAIT_MS`** (default 3000ms in Docker) controls how long the
  API waits for the mock PSP before returning `202` and finishing the
  payment in the background. This is well under `tok_timeout`'s 30s by
  design - see DESIGN.md section 3(b).
- Language: Rust + Axum, matching the assignment's stated preference
  directly, so no justification note is needed here.

## Demo Video

Demo link - https://www.loom.com/share/6448da07500644c2bae9fb6939f0c296
