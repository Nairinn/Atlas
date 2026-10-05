-- Stripe Connect: the tenancy answer for payments.
--
-- # The decision this migration records
--
-- Atlas never holds money. Before this migration there was no answer to
-- "who holds the money" at all: wallets stored balances inside Postgres,
-- which is close to the textbook definition of money transmission — a
-- licensed activity in most jurisdictions, and true regardless of which
-- processor is wired in.
--
-- The answer chosen is Stripe Connect: each Atlas PROJECT brings its own
-- Stripe account, and each payee (driver) in that project gets a Stripe
-- Express connected account under it. Money is charged, held, and paid out
-- by Stripe on the tenant's account; internal wallets become a mirror of
-- what happened at the processor, not a store of value Atlas owes anyone.
--
-- That has two consequences for the schema:
--
--   1. Atlas needs per-project Stripe credentials, because a charge must
--      be created on the TENANT's Stripe account (destination charges:
--      `transfer_data.destination` = the driver's connected account,
--      `application_fee_amount` = the tenant's cut).
--   2. Atlas needs to know, per user, which Stripe connected account pays
--      them, because settlement sends money there.
--
-- WITHDRAWAL stays unimplemented — now permanently rather than
-- provisionally: payouts are Stripe's job, and implementing them would be
-- Atlas taking custody, which is exactly what this structure exists to
-- avoid.

-- ---------------------------------------------------------------------------
-- Per-project Stripe configuration
-- ---------------------------------------------------------------------------
-- One row per project that has payments connected. The secret key is
-- stored ENCRYPTED (AES-256-GCM, key from the PAYMENT_CONFIG_ENC_KEY env
-- var, see services/payments-service .../crypto/ConfigCipher.kt), never in
-- plaintext: this column is a tenant's live Stripe credential, and a
-- database dump is exactly the artifact that leaks.
--
-- The webhook secret is per-endpoint (each project registers its own
-- webhook endpoint in its Stripe dashboard so events verify with its own
-- secret). It is stored encrypted for the same reason the key is, though
-- it grants no write access — it lets its holder forge webhook events,
-- which this service acts on, which is nearly as good.
CREATE TABLE control.project_payment_config (
    project_id       UUID PRIMARY KEY REFERENCES control.projects(id) ON DELETE CASCADE,
    secret_key_enc   BYTEA NOT NULL,
    webhook_secret_enc BYTEA NOT NULL,
    -- Three-letter ISO currency the project charges in (usd, eur, ...).
    -- Not the wallet currency: wallets mirror, they no longer choose.
    currency         TEXT NOT NULL DEFAULT 'usd',
    -- The processor this project is wired to. 'stripe' today; 'fake' for
    -- dev/test projects. Deliberately NOT read from a global env var:
    -- PAYMENT_PROVIDER chooses the DEFAULT provider, this overrides it
    -- per tenant, so one deployment can serve a test tenant on the fake
    -- provider while another project is on Stripe.
    provider         TEXT NOT NULL DEFAULT 'fake'
                  CHECK (provider IN ('fake', 'stripe')),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ---------------------------------------------------------------------------
-- Per-user Stripe connected accounts (drivers and other payees)
-- ---------------------------------------------------------------------------
CREATE TABLE payments.stripe_accounts (
    id                  UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    project_id          UUID NOT NULL REFERENCES control.projects(id) ON DELETE CASCADE,
    -- The composite (project_id, user_id) key added by migration 0080:
    -- a wallet for a user who belongs to another tenant must not exist,
    -- and the same rule applies to their payout account.
    user_id             UUID NOT NULL,
    stripe_account_id   TEXT NOT NULL,
    -- Flipped by the `account.updated` webhook when Stripe finishes
    -- onboarding. A transaction whose payee is not yet payable fails at
    -- authorize time with DriverNotOnboarded rather than at capture time,
    -- which is the difference between a clean error and money in limbo.
    payouts_enabled     BOOLEAN NOT NULL DEFAULT FALSE,
    onboarding_url      TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (project_id, user_id),
    FOREIGN KEY (project_id, user_id)
        REFERENCES auth.users(project_id, id) ON DELETE CASCADE
);

CREATE INDEX idx_stripe_accounts_project ON payments.stripe_accounts(project_id);

-- ---------------------------------------------------------------------------
-- Wire provider_ref lookups for the webhook path
-- ---------------------------------------------------------------------------
-- The webhook handler receives a Stripe PaymentIntent id and must find the
-- transaction it belongs to. provider_ref is unique per project in
-- practice, but the enforcing index is scoped by tenant — the same rule
-- as the idempotency key: an unscoped lookup could hand another tenant's
-- transaction to a webhook as if it were theirs.
CREATE UNIQUE INDEX idx_transactions_project_provider_ref
    ON payments.transactions(project_id, provider_ref)
    WHERE provider_ref IS NOT NULL;
