-- CANCELLED: an authorization voided before capture, plus the card_funded
-- flag that separates the two money flows D2 describes.
--
-- # Why CANCELLED and not just FAILED
--
-- A ride can be cancelled while its transaction is still PENDING: the
-- customer confirmed nothing (or the authorization is still just a hold),
-- so nothing was captured and nothing needs returning. Marking such a row
-- FAILED conflates "the provider refused" with "the ride never happened" —
-- reporting cannot tell a lost fare from a cancelled one, and the
-- reconciliation sweep cannot tell a dead hold it may cancel from a
-- charge attempt that genuinely failed.
--
-- The void path: fare-consumer reacts to RIDE_CANCELLED by calling
-- RefundTransaction; payments sees PENDING, cancels the PaymentIntent at
-- the provider (releasing the hold), and marks the row CANCELLED. No
-- balances move, because none moved.
--
-- # card_funded
--
-- Under Stripe Connect a fare is paid from the rider's CARD via a
-- destination charge, not from their wallet; wallets mirror processor
-- activity rather than storing value. A transfer initiated while a
-- payout account existed for the payee is card-funded: settle must not
-- debit the payer's wallet (the money never left it). Everything else
-- (the fake provider, plain wallet transfers) debits the payer's balance
-- as before. The flag is fixed at initiation — whether the charge had a
-- destination account — because settle and the webhook both need it and
-- cannot re-derive it later.

ALTER TABLE payments.transactions DROP CONSTRAINT transactions_status_check;

ALTER TABLE payments.transactions
    ADD CONSTRAINT transactions_status_check
    CHECK (status IN ('pending', 'settled', 'failed', 'refunded', 'cancelled'));

ALTER TABLE payments.transactions
    ADD COLUMN card_funded BOOLEAN NOT NULL DEFAULT FALSE;
