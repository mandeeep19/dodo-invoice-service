# DESIGN.md

## 1. Data Model

```
businesses(id, name, created_at)
api_keys(id, business_id, key_prefix UNIQUE, key_hash, created_at, revoked_at)
customers(id, business_id, name, email, created_at)
invoices(id, business_id, customer_id, state, currency, total_cents, due_date, created_at, updated_at)
invoice_line_items(id, invoice_id, description, quantity, unit_amount_cents, amount_cents)
payment_attempts(id, invoice_id, status, card_token, failure_code, psp_ref, created_at, updated_at)
idempotency_keys(business_id, idempotency_key, request_hash, response_status, response_body, invoice_id, payment_attempt_id, created_at)
webhook_endpoints(id, business_id, url, secret, created_at, disabled_at)
webhook_deliveries(id, endpoint_id, business_id, event_type, payload, status, attempt_count, next_attempt_at, last_error, created_at, updated_at)
```

All primary keys are `UUID`. Every business-owned table carries
`business_id`, and every query filters on it - no endpoint can return
another business's row by guessing an id.

**Indexes:** `invoices(business_id, state)` covers the only two things list
queries filter on together. `payment_attempts(invoice_id)` plus a **partial
unique index** `(invoice_id) WHERE status = 'pending'` - the second one
isn't for speed, it's a correctness constraint: the database refuses a
second in-flight attempt per invoice (section 3a). `webhook_deliveries
(status, next_attempt_at)` backs the dispatcher's only query, "what's due
now." `api_keys(key_prefix)` UNIQUE makes auth an indexed lookup instead of
hashing against every stored key on every request.

**Primary key strategy:** UUIDv4, not auto-increment ints, because ids leak
into API responses and webhook payloads; sequential ids invite enumeration
across businesses. The cost (16 vs 8 bytes, worse insert locality) is
irrelevant at this scale.

**Idempotency keys as a durable table, not a cache:** a payment decision
must survive a restart - a client retrying an hour after a deploy must still
get the original result.

**At 100x scale:** partition `invoices`, `payment_attempts`, and
`webhook_deliveries` by `business_id` hash so a few huge businesses don't
dominate every index; move `idempotency_keys`/`webhook_deliveries` (high
write, short useful life) off the primary I/O path; replace `FOR UPDATE
SKIP LOCKED` polling with a real queue once dispatch volume causes lock
contention; add a read replica for list/GET traffic.

## 2. Invoice State Machine

```
                 finalize                  successful payment
  ┌───────┐ ─────────────────► ┌──────┐ ─────────────────────► ┌──────┐
  │ draft │                    │ open │                        │ paid │
  └───┬───┘                    └──┬───┘                        └──────┘
      │ void                      │ void            mark-uncollectible
      ▼                           ▼                           ▼
  ┌──────┐                   ┌──────┐              ┌───────────────┐
  │ void │                   │ void │              │ uncollectible │
  └──────┘                   └──────┘              └───────────────┘
  (terminal, from draft)     (terminal, from open)   (terminal, from open)
```

| From | To | Trigger |
|---|---|---|
| draft | open | `POST /invoices/{id}/finalize` |
| draft or open | void | `POST /invoices/{id}/void` |
| open | paid | a successful payment attempt (system, inside `/pay`) |
| open | uncollectible | `POST /invoices/{id}/mark-uncollectible` |

**Terminal:** `paid`, `void`, `uncollectible` - no transition leaves any of
them. A **failed** payment attempt does not transition the invoice; it stays
`open` so a different card can be retried. Only success moves state.

**Reversibility:** none of these transitions reverse. `paid -> open` would
be a refund (out of scope); `void -> draft` would resurrect a cancelled
invoice with stale line items - a business creates a new one instead.

**Rejecting invalid transitions:** every transition handler locks the
invoice row and checks current state against an explicit allow-list in
`state_machine.rs`, returning `409` with a machine-readable code
(`invalid_state_transition`, `invoice_not_payable`, `invoice_already_paid`).
A Postgres `CHECK` constraint on the column is the second line of defense.

## 3. Payment Correctness & Failure Modes

**(a) Two clients call `POST /pay` for the same invoice at the same
instant.** Exactly one succeeds; the other gets `409`. Verified by
`tests/concurrency_test.rs`, which fires 10 concurrent requests with
distinct idempotency keys and asserts exactly one `200`.

Mechanism: a short transaction does `SELECT state FROM invoices WHERE id=$1
FOR UPDATE`, checks `open`, inserts a `payment_attempts` row
(`status='pending'`), and commits immediately - the lock is held only for
the claim, not for the PSP call. A **partial unique index**,
`UNIQUE(invoice_id) WHERE status='pending'`, closes the actual race: `FOR
UPDATE` serializes the two claim transactions, so the second one's insert
runs after the first commits and hits the constraint, returning `409
payment_attempt_in_progress`. Chosen over an advisory lock (equivalent but
less legible and not enforced against code that forgets to take it),
optimistic concurrency (wrong shape - the loser should be told a payment is
already running, not retry), and serializable isolation (heavier, and still
needs this same index to express the invariant explicitly).

**(b) The mock PSP times out (`tok_timeout`, 30s).** The endpoint does not
wait 30 seconds. The PSP call and its DB finalization run on a **detached**
`tokio::spawn` task; the handler only awaits that task for a short
synchronous window (`PSP_SYNC_WAIT_MS`, default 3s) via `tokio::time::
timeout`. If the window elapses, the handler returns `202 Accepted` with the
`attempt_id` and stops waiting - the spawned task keeps running
independently (it owns its own pool/HTTP client clones) up to a hard outer
timeout (35s default). The `payment_attempts` row stays `pending`; the
invoice stays `open` (a second `/pay` still correctly gets `409` from the
constraint in 3a). The caller finds the eventual result by polling `GET
/invoices/{id}/payment-attempts/{attempt_id}`, or via the
`invoice.paid`/`invoice.payment_failed` webhook the detached task enqueues
on finish. `tests/psp_failure_test.rs` asserts the handler returns well
under the PSP's delay, the invoice stays open and un-payable meanwhile, and
both the attempt and invoice resolve correctly once the PSP answers.

**(c) The PSP returns success but the service crashes before persisting
that.** If the crash is before the claim transaction commits, nothing
happened - no attempt row, no PSP call, a retry starts clean. If it's after
the PSP succeeded but before the finalizing `UPDATE` commits, the attempt is
left `pending` indefinitely. Does the customer get charged twice? Not from
this service: the partial unique index still shows a `pending` attempt, so
any retry (fresh idempotency key or not) is rejected with `409` rather than
firing a second charge; a retry with the *original* key never even reaches
that logic (see 3d). The honest gap: resolving that orphaned `pending`
attempt needs a reconciliation job that queries the PSP's own record of the
charge - our mock PSP has no such lookup endpoint, so I didn't build one.
See section 7.

**(d) An idempotency key is reused with a different request body.**
Rejected with `422 idempotency_key_reused` before any invoice lookup or PSP
call. The stored hash covers the semantically relevant fields
(`invoice_id`, `card_token`); a mismatch is either a client bug or an unsafe
reuse of a payment slot for a different card, so it's a hard error.

**(e) An invoice in `paid` receives another `POST /pay`.** `409
invoice_already_paid` - a distinct code from the generic
`invoice_not_payable` used for draft/void/uncollectible, so a client can
tell "already done" apart from "never payable." No new attempt row, no PSP
call.

**Concurrency mechanism, named:** row lock for a short claim, enforced by a
database-level partial unique index as the real invariant, plus
status-conditional updates (`WHERE status='pending'`, `WHERE state='open'`)
at finalization so a finalize can't apply twice or race a concurrent void.

## 4. Webhook Design

**Signing:** HMAC-SHA256 over `"{unix_timestamp}.{raw_json_body}"`, keyed by
a per-endpoint secret (`whsec_...`, shown once, never stored in plaintext).
Header: `X-Webhook-Signature: t=<ts>,v1=<hex_hmac>` (Stripe's shape).
**Replay protection** is the timestamp: a receiver recomputes the HMAC and
additionally rejects signatures older than its own tolerance window (5 min
is a reasonable default to recommend) - this bounds how long a captured
request stays replayable even though the HMAC itself doesn't expire.

**Retry policy, specific numbers:** attempt 1 fires immediately on enqueue;
on failure: `30s, 5min, 30min, 2h, 12h, 24h` before attempt 7 (final). Seven
attempts total, ~39 hours end to end. After attempt 7 fails, the delivery is
marked `failed_exhausted` and stops - the row stays, nothing is deleted.

**Reconciliation:** a business can always call `GET /invoices/{id}` (or
list by state) for current truth directly; the webhook is a notification,
not the source of truth. I did not build a "list/replay failed deliveries"
endpoint (section 6) - in production it's the first thing I'd add.

**Why delivery is decoupled, and how:** handlers never make an HTTP call to
a webhook endpoint. They `INSERT` into `webhook_deliveries` inside the
*same* transaction as the state change that triggered the event
(transactional outbox) - durably queued the instant that transaction
commits, with no window where a state change happens but the event is
silently lost. Actual HTTP delivery happens in `run_dispatcher`, a
`tokio::spawn`ed loop started once at boot that polls every second (`FOR
UPDATE SKIP LOCKED`, safe for multiple replicas to run concurrently). A
slow or dead customer endpoint adds zero latency to any API response.

## 5. API Key Model

**Generation:** 24 CSPRNG bytes, hex-encoded, prefixed `sk_live_`.
**Storage:** never in plaintext. The first 12 hex chars are stored as
`key_prefix` in the clear (purely for indexed lookup); the rest is hashed
with **argon2id**. A leaked DB dump identifies which business a key belongs
to but yields no usable credential. **Transmission:** `Authorization:
Bearer sk_live_...`; never a query parameter (log/history leakage).
**Rotation:** not a dedicated endpoint - the schema supports multiple keys
per business, but there's no mint-alongside-old flow, so "rotate" today
means "revoke and re-onboard." **Revocation:** a `revoked_at` timestamp
checked on every request; instant, no cache to wait out. **Blast radius if
leaked:** full read/write access to that one business's customers,
invoices, and payment attempts, and the ability to register a webhook
endpoint (redirecting that business's event stream). No access to any other
business's data, no ability to mint new keys or rename the business.

## 6. What I Cut, and Why

1. **Refunds/partial payments** - out of scope per spec; needs a `refunds`
   table and PSP refund semantics the mock doesn't model.
2. **API key rotation endpoint** - schema supports it, flow doesn't exist;
   mechanical but not a must-have, and I'd rather the concurrency/idempotency
   work be solid than pad surface area.
3. **List/replay failed webhook deliveries** - the data exists
   (`status='failed_exhausted'`), but exposing and safely re-triggering it
   (replay-then-succeeds-twice?) felt like real scope, not a quick add.
4. **Editing a draft invoice** - creation is a single call; real products
   let you add/remove line items pre-finalize, which would add a second
   total-recomputation path for one demo's worth of value.
5. **Rate limiting** - explicitly out of scope; discussed in section 7.
6. **Reconciliation for orphaned `pending` attempts** (tail of 3c) - I built
   what prevents double charges, not the background sweep that resolves a
   `pending` attempt whose PSP call never got an answer at all. First thing
   I'd fix in production - see section 7.

## 7. Production Readiness Gap

1. **Reconciliation for stuck `pending` attempts.** If the process hosting
   the detached PSP-call task dies mid-flight, the attempt stays `pending`
   forever with nothing to wake it, since the mock PSP has no
   look-up-a-past-charge endpoint. Needs a periodic job that finds attempts
   `pending` past ~2 minutes and reconciles against the real PSP or flags
   for manual review.
2. **Observability.** Structured `tracing` logs exist and that's it - no
   metrics (payment success rate, webhook latency, PSP latency), no
   alerting on `failed_exhausted` webhooks or stuck attempts, no correlated
   tracing across the invoice-service -> PSP hop. A spike in
   `psp_unavailable` should page someone; today it's a log line.
3. **Audit log.** State is reconstructable from `updated_at` and current
   values, but there's no append-only record of *who* (which key, which IP)
   did *what*, *when*. For billing specifically, that's the first thing a
   support escalation or compliance review asks for.

Also worth naming, lower priority: no API key rotation UX (section 6), no
multi-currency (out of scope per spec), and the webhook backoff numbers are
reasonable defaults, not tuned against real receiver behavior.
