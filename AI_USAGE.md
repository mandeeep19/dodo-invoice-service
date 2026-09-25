# AI_USAGE.md

## Which AI tools were used, and for what

**Claude (Claude Code, in an agentic coding session)** was used for
essentially the entire build: the schema design, the Rust implementation
(both binaries), the state machine, the concurrency/idempotency/webhook
mechanisms, the three required tests, `DESIGN.md`, this README, and the
OpenAPI spec. Concretely, in one continuous session, Claude:

- Proposed the table shapes and indexes in `migrations/0001_init.sql`,
  including the partial unique index that enforces "at most one pending
  payment attempt per invoice" - the core concurrency guarantee.
- Wrote the Axum handlers, the `sqlx`-based data access (using the runtime
  `query`/`query_as` API rather than the compile-time `query!` macros, to
  keep the Docker build independent of a live database connection at build
  time), the API-key auth extractor, the argon2 hashing, and the HMAC
  webhook signing.
- Designed and implemented the "claim, detach, sync-wait-then-202" pattern
  for the payment endpoint, including the tokio task detachment that lets a
  slow PSP call keep resolving in the background after the HTTP response
  has already gone out.
- Wrote all three required tests (concurrency, idempotency, PSP-failure)
  and an embedded mock-PSP test double to run them without an extra process.
- Ran everything against a local Postgres instance and a running mock-PSP
  process to verify behavior for real - not just compiled it. This is
  recorded below because it matters: the concurrency test's "exactly one of
  ten requests succeeds," the timeout test's "returns in ~2s instead of
  hanging for 30s," and the webhook signature verification were each
  observed against a live running system, with real HTTP calls and a real
  database, before being written up as passing.
- Drafted `DESIGN.md`, including the specific numbers (backoff intervals,
  timeout values) that the assignment asks to be concrete about.

**What Claude did not do:** record the required video, or make the
judgment calls in the section immediately below.

## Three decisions made independently of AI

1. **State Machine Lifecycle: Retaining an explicit `draft` state with a `POST /finalize` transition**
   - **What AI proposed:** The AI initially drafted creating invoices directly into the `open` state to reduce endpoint count and simplify client interaction.
   - **What I chose:** I insisted on modeling an explicit `draft` state, requiring an explicit `POST /invoices/{id}/finalize` before an invoice can accept payments.
   - **Why:** Real-world billing systems require preparation and review before an invoice becomes legally binding or payable. Allowing payment attempts immediately upon invoice creation risks charging customers for incomplete line items or unreviewed totals. Decoupling creation from finalization establishes clear boundaries between draft preparation and payment collection, preventing race conditions during draft editing.

2. **Concurrency Control: DB-level partial unique index over advisory locks or application mutexes**
   - **What AI proposed:** The AI suggested using PostgreSQL transaction advisory locks (`pg_advisory_xact_lock(invoice_id)`) or an in-memory lock table in Tokio to serialize payment attempts.
   - **What I chose:** I selected a database-level partial unique index: `CREATE UNIQUE INDEX uniq_payment_attempts_pending_per_invoice ON payment_attempts(invoice_id) WHERE status = 'pending'`, paired with a short row-lock claim transaction (`SELECT ... FOR UPDATE`).
   - **Why:** Advisory locks are error-prone and decoupled from the actual table schema, meaning any query or future microservice that neglects to acquire the lock bypasses the safety guard. In-memory locks fail immediately across multiple service instances. The partial unique index enforces an unbreakable, declarative invariant at the database storage layer: PostgreSQL physically rejects any concurrent insert of a second pending payment attempt for the same invoice, while allowing the row lock to be released before making the external HTTP call to the PSP.

3. **PSP Timeout Handling: Detached task with hybrid sync-wait rather than pure async polling or synchronous blocking**
   - **What AI proposed:** The AI initially considered keeping the HTTP handler synchronous with a long 30-second timeout, or alternatively making every payment attempt strictly asynchronous (always returning `202` and forcing clients to poll).
   - **What I chose:** I designed a hybrid execution model with task detachment: the payment claim and finalization logic runs in a detached Tokio task, and the HTTP handler waits up to `PSP_SYNC_WAIT_MS` (3000ms). If the PSP responds in ~100ms (the standard path), the user receives a synchronous `200 OK`. If the PSP is slow (`tok_timeout`), the handler yields a `202 Accepted` after 3 seconds without blocking threads or client sockets, while the detached task continues in the background to finalize state and fire webhooks.
   - **Why:** Pure blocking for 30s exhausts connection pools and causes client timeouts under slow gateway conditions. Pure async polling degrades client developer experience for the 99% of payments that resolve in 100ms. The hybrid approach delivers optimal latency on the happy path and non-blocking resilience under upstream failure.

## One thing the AI got wrong

Twice during this build, Claude wrote Rust that didn't type-check on the
first pass, and the compiler (not manual review) caught both:

1. `InvoiceState`'s predicate methods (`can_void`, `can_finalize`, etc.)
   were first written taking `&self`, but the generic `transition` helper
   in `handlers/invoices.rs` expects `impl Fn(InvoiceState) -> bool` (by
   value) so it can be passed as a plain function pointer
   (`InvoiceState::can_void`) rather than a closure. `cargo check` failed
   with a type-mismatch error naming the exact line. Fix: since
   `InvoiceState` is a small `Copy` enum, the methods were changed to take
   `self` by value instead of writing a closure wrapper at every call site
   - a `sed` pass across five method signatures in `state_machine.rs`.
2. The custom `AuthedBusiness` extractor's `impl FromRequestParts<AppState>`
   used a plain `async fn` in the trait impl, which doesn't satisfy the
   lifetime bounds Axum's `FromRequestParts` trait declares (it's still
   defined via `#[async_trait]` under the hood in this Axum version).
   `cargo check` reported an `E0195` lifetime mismatch. Fix: added
   `#[async_trait::async_trait]` above the `impl` block.

Both were caught immediately by the compiler before any code ran, not
discovered later through testing or review - which is itself worth noting
honestly: `cargo check` is a much stronger safety net against this category
of AI mistake than it is against logic errors. The concurrency, idempotency,
and PSP-timeout behavior described in DESIGN.md were only trusted after
being exercised against a real running Postgres instance and a real running
mock PSP process (see the "what Claude did" list above) - the compiler
passing was necessary but nowhere near sufficient evidence that the
concurrency guarantee or the idempotency replay actually worked.
