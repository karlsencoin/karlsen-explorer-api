-- Exchange hot-wallet tags that follow the change output.
SET client_min_messages = warning;
--
-- Exchanges send the change of every withdrawal to a freshly derived address,
-- so a hand-written tag stops matching after one withdrawal. This job extends
-- each exchange tag along its change chain and stores the derived tags in
-- address_tags_auto; the API serves them at /analytics/address-tags/auto and
-- the explorer shows them next to the hand-written tags.
--
-- A transaction moves an exchange tag to its change output only when ALL of:
--   1. every input is resolved, at least one input carries a tag of the
--      exchange and no input carries a tag of another exchange,
--   2. the largest output carries at least 75 % of the input value (2026-10-22:
--      NonKYC's change was 85 % after a 15.28M KLS withdrawal; the earlier
--      90 % threshold missed it),
--   3. that output's address has never appeared before this transaction and
--      carries no tag yet, and
--   4. the transaction also pays at least one other address and nothing goes
--      back to an input address or to the exchange's own tagged addresses (a
--      whole balance moved to one new address, or a payment with change kept,
--      is not a change hop).
--
-- Untagged inputs of such a transaction are tagged too (kind 'co-input'):
-- addresses spent together are controlled by the same keys. Exchanges spend
-- their deposit-collection address together with the hot wallet, and an
-- untagged co-input used to stop the chain (2026-10-22: qptmu6 + qrunj7 ->
-- qqzqv2q4). Co-inputs are only taken from transactions that pass all four
-- rules, so a single wrong tag cannot spread through unrelated spends.
--
-- Confirmed and unconfirmed tags
-- ------------------------------
-- Rules 1-4 only guess which output is the change. An exchange that pays out
-- its whole input with no change at all looks exactly like a change hop when
-- one payee receives more than 75 % (2026-10-27, tx 1164ae4053: two deposits
-- of qqlahash, 5,741,702 + 5,095,763 KLS, paid out in full as 9,658,252.31 to
-- qzsggnse and 1,179,212.69 to qqwa7nwa; qzsggnse was tagged NonKYC although
-- nothing proves it belongs to NonKYC). The amounts cannot tell such a payout
-- from real change, so every derived tag starts as UNCONFIRMED.
--
-- A derived tag becomes CONFIRMED only through the common-input rule, which
-- is proof of the same keys rather than a guess: the address is an input of a
-- transaction in which another input is a hand-written tag of the exchange or
-- an already confirmed derived tag, and no input is tagged for another
-- exchange. Confirmation repeats until nothing changes (an address spent with
-- a confirmed one confirms, then the addresses spent with it). It looks at the
-- whole history of the unconfirmed addresses, not only the scan window, so a
-- change address confirms on the first run after the exchange spends it.
-- A change link never confirms anything by itself: change from a confirmed
-- address stays unconfirmed until it is spent together with the wallet.
--
-- Derived tags carry the plain exchange name; the API adds " (unconfirmed)"
-- to tags that are not confirmed. Provenance stays in parent_address,
-- transaction_id and kind; confirmed_by is the transaction that proved it.
--
-- Run hourly by karlsen-distribution-snapshot-hourly.service:
--   psql -v tags_file=/opt/karlsen/karlsen-explorer-v2/address-tags.json -f address_tags_auto.sql
-- Backfill: add -v hours=720 to scan the last 30 days once.
-- Cost: only transactions that touch tagged exchange addresses are read
-- (index on addresses_transactions(address, block_time)), a few per hour.

\if :{?hours}
\else
\set hours 2
\endif
\set tags_json `cat :'tags_file'`

CREATE TABLE IF NOT EXISTS address_tags_auto (
    address        text        PRIMARY KEY,   -- stored without the 'karlsen:' prefix
    label          text        NOT NULL,      -- exchange name, e.g. 'NonKYC'
    parent_address text        NOT NULL,      -- address the tag was inherited from
    transaction_id bytea       NOT NULL,      -- transaction that moved the tag
    tagged_at      timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE address_tags_auto ADD COLUMN IF NOT EXISTS kind text NOT NULL DEFAULT 'change';
-- Proven by the common-input rule (see above); false = a guess from rules 1-4.
ALTER TABLE address_tags_auto ADD COLUMN IF NOT EXISTS confirmed    boolean NOT NULL DEFAULT false;
ALTER TABLE address_tags_auto ADD COLUMN IF NOT EXISTS confirmed_by bytea;
ALTER TABLE address_tags_auto ADD COLUMN IF NOT EXISTS confirmed_at timestamptz;
-- Older runs labelled derived tags 'NonKYC (auto)'; use the plain name.
UPDATE address_tags_auto SET label = regexp_replace(label, '\s*\(auto\)$', '')
WHERE  label ~ '\(auto\)$';

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'explorer_ro') THEN
        GRANT SELECT ON address_tags_auto TO explorer_ro;
    END IF;
END $$;

-- Exchanges whose tags follow change outputs. Mining pools pay out from fixed
-- addresses and are deliberately left out.
CREATE TEMP TABLE follow_exchange (name text PRIMARY KEY);
INSERT INTO follow_exchange VALUES ('NonKYC'), ('AnonKYC'), ('Xeggex'), ('Mexc');

-- Every hand-written tag (any kind), so no tagged address is ever overwritten.
CREATE TEMP TABLE manual_tags AS
SELECT regexp_replace(key, '^karlsen:', '') AS address, value AS label
FROM   jsonb_each_text(:'tags_json'::jsonb);

-- Seeds: hand-written exchange tags (always confirmed) plus all derived tags,
-- keyed by exchange.
CREATE TEMP TABLE seed (
    address   text    PRIMARY KEY,
    exchange  text    NOT NULL,
    confirmed boolean NOT NULL
);
INSERT INTO seed
SELECT m.address, f.name, true
FROM   manual_tags m
JOIN   follow_exchange f ON regexp_replace(m.label, '\s*\(.*\)$', '') = f.name
ON CONFLICT DO NOTHING;
INSERT INTO seed
SELECT a.address, regexp_replace(a.label, '\s*\(.*\)$', ''), a.confirmed
FROM   address_tags_auto a
ON CONFLICT DO NOTHING;

CREATE TEMP TABLE params AS
SELECT (extract(epoch FROM now() - make_interval(hours => :hours)) * 1000)::bigint AS since_ms;

-- ---- 1. follow the change chains (new tags start unconfirmed) ----

DO $$
DECLARE
    added int;
    co_added int;
    rounds int := 0;
BEGIN
    -- A chain can move several times inside one window; repeat until stable.
    LOOP
        rounds := rounds + 1;

        DROP TABLE IF EXISTS hop;
        CREATE TEMP TABLE hop AS
        WITH cand AS (
            SELECT DISTINCT at.transaction_id
            FROM   seed s
            JOIN   addresses_transactions at ON at.address = s.address
            WHERE  at.block_time >= (SELECT since_ms FROM params)
        ),
        ins AS (
            SELECT t.transaction_id,
                   t.block_time,
                   cardinality(t.inputs)              AS n_inputs,
                   (prev_o).script_public_key_address AS address,
                   (prev_o).amount                    AS amount
            FROM   cand c
            JOIN   transactions t ON t.transaction_id = c.transaction_id
            CROSS  JOIN LATERAL unnest(t.inputs) AS inp
            JOIN   transactions prev_t ON prev_t.transaction_id = (inp).previous_outpoint_hash
            CROSS  JOIN LATERAL unnest(prev_t.outputs) AS prev_o
            WHERE  (prev_o).index = (inp).previous_outpoint_index
        ),
        spends AS (
            -- Rule 1: all inputs resolved, tagged ones from a single exchange.
            SELECT i.transaction_id,
                   min(i.block_time)   AS block_time,
                   min(s.exchange)     AS exchange,
                   -- parent: the largest tagged input
                   (array_agg(i.address ORDER BY (s.address IS NULL), i.amount DESC))[1]
                                       AS parent_address,
                   array_agg(DISTINCT i.address) FILTER (WHERE s.address IS NULL)
                                       AS untagged_inputs,
                   array_agg(DISTINCT i.address) AS input_addresses,
                   sum(i.amount)       AS input_sompi
            FROM   ins i
            LEFT   JOIN seed s ON s.address = i.address
            GROUP  BY i.transaction_id
            HAVING count(*) = min(i.n_inputs)
               AND count(s.address) >= 1
               AND count(DISTINCT s.exchange) = 1
        ),
        change AS (
            SELECT DISTINCT ON (sp.transaction_id)
                   sp.transaction_id, sp.block_time, sp.exchange, sp.parent_address,
                   sp.untagged_inputs, sp.input_sompi,
                   (SELECT count(DISTINCT (o2).script_public_key_address)
                    FROM unnest(t.outputs) AS o2)  AS output_addresses,
                   (SELECT bool_or(s2.address IS NOT NULL
                                   OR (o3).script_public_key_address = ANY(sp.input_addresses))
                    FROM unnest(t.outputs) AS o3
                    LEFT JOIN seed s2 ON s2.address = (o3).script_public_key_address
                                     AND s2.exchange = sp.exchange) AS change_to_self,
                   (o).script_public_key_address AS address,
                   (o).amount                    AS amount
            FROM   spends sp
            JOIN   transactions t ON t.transaction_id = sp.transaction_id
            CROSS  JOIN LATERAL unnest(t.outputs) AS o
            ORDER  BY sp.transaction_id, (o).amount DESC, (o).script_public_key_address
        )
        SELECT ch.*
        FROM   change ch
        WHERE  ch.amount * 4 >= ch.input_sompi * 3                               -- rule 2
          AND  ch.output_addresses >= 2                                           -- rule 4
          AND  NOT ch.change_to_self
          AND  NOT EXISTS (SELECT 1 FROM seed s        WHERE s.address = ch.address)
          AND  NOT EXISTS (SELECT 1 FROM manual_tags m WHERE m.address = ch.address)
          AND  NOT EXISTS (                                                       -- rule 3
                   SELECT 1 FROM addresses_transactions at
                   WHERE  at.address = ch.address AND at.block_time < ch.block_time);

        -- New tags are unconfirmed; only step 2 below confirms.
        INSERT INTO address_tags_auto (address, label, parent_address, transaction_id, kind, confirmed)
        SELECT h.address, h.exchange, h.parent_address, h.transaction_id, 'change', false
        FROM   hop h
        ON CONFLICT (address) DO NOTHING;
        GET DIAGNOSTICS added = ROW_COUNT;

        -- Untagged co-inputs of the same transactions (common-input rule).
        INSERT INTO address_tags_auto (address, label, parent_address, transaction_id, kind, confirmed)
        SELECT DISTINCT ON (u.address) u.address, h.exchange, h.parent_address, h.transaction_id,
               'co-input', false
        FROM   hop h CROSS JOIN LATERAL unnest(h.untagged_inputs) AS u(address)
        WHERE  NOT EXISTS (SELECT 1 FROM manual_tags m WHERE m.address = u.address)
        ORDER  BY u.address, h.block_time
        ON CONFLICT (address) DO NOTHING;
        GET DIAGNOSTICS co_added = ROW_COUNT;
        added := added + co_added;

        EXIT WHEN added = 0 OR rounds >= 20;

        INSERT INTO seed
        SELECT a.address, regexp_replace(a.label, '\s*\(.*\)$', ''), a.confirmed
        FROM   address_tags_auto a
        ON CONFLICT DO NOTHING;
    END LOOP;
END $$;

-- Step 1 may have added rows that are not seeds yet.
INSERT INTO seed
SELECT a.address, regexp_replace(a.label, '\s*\(.*\)$', ''), a.confirmed
FROM   address_tags_auto a
ON CONFLICT DO NOTHING;

-- ---- 2. confirm derived tags by the common-input rule ----

DO $$
DECLARE
    confirmed_now int;
    rounds int := 0;
BEGIN
    LOOP
        rounds := rounds + 1;

        WITH pending AS (
            SELECT a.address, regexp_replace(a.label, '\s*\(.*\)$', '') AS exchange
            FROM   address_tags_auto a
            WHERE  NOT a.confirmed
        ),
        -- Every resolved input of every transaction of a pending address.
        spend AS (
            SELECT p.address                          AS target,
                   p.exchange,
                   t.transaction_id,
                   t.block_time,
                   (prev_o).script_public_key_address AS input_address
            FROM   pending p
            JOIN   addresses_transactions at ON at.address = p.address
            JOIN   transactions t ON t.transaction_id = at.transaction_id
            CROSS  JOIN LATERAL unnest(t.inputs) AS inp
            JOIN   transactions prev_t ON prev_t.transaction_id = (inp).previous_outpoint_hash
            CROSS  JOIN LATERAL unnest(prev_t.outputs) AS prev_o
            WHERE  (prev_o).index = (inp).previous_outpoint_index
        ),
        proof AS (
            -- The pending address is an input, another input is confirmed for
            -- the same exchange, and no input belongs to another exchange.
            -- The earliest such transaction is kept as the proof.
            SELECT DISTINCT ON (g.target) g.target, g.transaction_id
            FROM (
                SELECT sp.target, sp.transaction_id, min(sp.block_time) AS block_time
                FROM   spend sp
                LEFT   JOIN seed s ON s.address = sp.input_address
                GROUP  BY sp.target, sp.exchange, sp.transaction_id
                HAVING bool_or(sp.input_address = sp.target)
                   AND COALESCE(bool_or(s.confirmed
                                        AND s.exchange = sp.exchange
                                        AND sp.input_address <> sp.target), false)
                   AND NOT COALESCE(bool_or(s.exchange <> sp.exchange), false)
            ) g
            ORDER  BY g.target, g.block_time NULLS LAST, g.transaction_id
        )
        UPDATE address_tags_auto a
        SET    confirmed    = true,
               confirmed_by = pr.transaction_id,
               confirmed_at = now()
        FROM   proof pr
        WHERE  a.address = pr.target
          AND  NOT a.confirmed;
        GET DIAGNOSTICS confirmed_now = ROW_COUNT;

        EXIT WHEN confirmed_now = 0 OR rounds >= 50;

        UPDATE seed s
        SET    confirmed = true
        FROM   address_tags_auto a
        WHERE  a.address = s.address AND a.confirmed AND NOT s.confirmed;
    END LOOP;
END $$;

SELECT label, kind, confirmed, count(*) AS addresses
FROM   address_tags_auto
GROUP  BY label, kind, confirmed
ORDER  BY label, kind, confirmed DESC;
