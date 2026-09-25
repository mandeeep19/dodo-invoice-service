-- Invoice & Payment Service - initial schema.
-- See DESIGN.md for the reasoning behind each shape/index choice.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE businesses (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- API keys are scoped to a business. The full secret is never stored: only
-- an argon2 hash plus a short, non-secret prefix used to look the row up
-- quickly without a full-table hash comparison.
CREATE TABLE api_keys (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    key_prefix  TEXT NOT NULL UNIQUE,
    key_hash    TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX idx_api_keys_business ON api_keys(business_id);

CREATE TABLE customers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    email       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_customers_business ON customers(business_id);

CREATE TABLE invoices (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    customer_id UUID NOT NULL REFERENCES customers(id),
    state       TEXT NOT NULL DEFAULT 'draft'
                    CHECK (state IN ('draft', 'open', 'paid', 'void', 'uncollectible')),
    currency    TEXT NOT NULL DEFAULT 'USD' CHECK (currency = 'USD'),
    total_cents BIGINT NOT NULL DEFAULT 0 CHECK (total_cents >= 0),
    due_date    DATE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Every list endpoint is scoped to a business and optionally filtered by
-- state, so that's the composite index that matters.
CREATE INDEX idx_invoices_business_state ON invoices(business_id, state);
CREATE INDEX idx_invoices_customer ON invoices(customer_id);

CREATE TABLE invoice_line_items (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    invoice_id          UUID NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    description         TEXT NOT NULL,
    quantity            BIGINT NOT NULL CHECK (quantity > 0),
    unit_amount_cents   BIGINT NOT NULL CHECK (unit_amount_cents >= 0),
    amount_cents        BIGINT NOT NULL CHECK (amount_cents >= 0)
);
CREATE INDEX idx_line_items_invoice ON invoice_line_items(invoice_id);

CREATE TABLE payment_attempts (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    invoice_id   UUID NOT NULL REFERENCES invoices(id),
    status       TEXT NOT NULL CHECK (status IN ('pending', 'succeeded', 'failed')),
    card_token   TEXT NOT NULL,
    failure_code TEXT,
    psp_ref      TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_payment_attempts_invoice ON payment_attempts(invoice_id);
-- The core concurrency guarantee: the database itself refuses to let more
-- than one payment attempt be "in flight" for a given invoice at a time.
-- See DESIGN.md section 3(a).
CREATE UNIQUE INDEX uniq_payment_attempts_pending_per_invoice
    ON payment_attempts(invoice_id)
    WHERE status = 'pending';

-- Generic idempotency store, keyed per business so two businesses can reuse
-- the same key string without colliding. Stores a hash of the request body
-- (not the body itself) so a byte-for-byte-identical replay is detected
-- without keeping raw payloads around indefinitely.
CREATE TABLE idempotency_keys (
    business_id         UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    idempotency_key     TEXT NOT NULL,
    request_hash        TEXT NOT NULL,
    response_status     INT,
    response_body       JSONB,
    invoice_id          UUID,
    payment_attempt_id  UUID,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (business_id, idempotency_key)
);

CREATE TABLE webhook_endpoints (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    secret      TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at TIMESTAMPTZ
);
CREATE INDEX idx_webhook_endpoints_business ON webhook_endpoints(business_id);

-- The delivery outbox. A row is inserted synchronously in the same
-- transaction as the state change it reports (transactional outbox
-- pattern), and a background dispatcher polls it. This is what decouples
-- webhook delivery from the request/response path: the API handler's
-- transaction commits once this row exists, never once an HTTP POST to the
-- receiver has succeeded.
CREATE TABLE webhook_deliveries (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    endpoint_id     UUID NOT NULL REFERENCES webhook_endpoints(id) ON DELETE CASCADE,
    business_id     UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    event_type      TEXT NOT NULL,
    payload         JSONB NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'succeeded', 'failed_exhausted')),
    attempt_count   INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- The dispatcher's only query: "what's due right now".
CREATE INDEX idx_webhook_deliveries_poll ON webhook_deliveries(status, next_attempt_at);
