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
--   1. every input belongs to tagged addresses of one and the same exchange,
--   2. the largest output carries at least 90 % of the input value,
--   3. that output's address has never appeared before this transaction and
--      carries no tag yet, and
--   4. the transaction also pays at least one other address and nothing goes
--      back to the exchange's own tagged addresses (a whole balance moved to
--      one new address, or a payment with change kept, is not a change hop).
-- Customer withdrawals sent to a fresh address fail rule 2 unless they take
-- nearly the whole hot wallet, which is what keeps false tags rare.
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
    label          text        NOT NULL,      -- e.g. 'NonKYC (auto)'
    parent_address text        NOT NULL,      -- address the tag was inherited from
    transaction_id bytea       NOT NULL,      -- transaction that moved the tag
    tagged_at      timestamptz NOT NULL DEFAULT now()
);

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

-- Seeds: hand-written exchange tags plus all derived tags, keyed by exchange.
CREATE TEMP TABLE seed (address text PRIMARY KEY, exchange text NOT NULL);
INSERT INTO seed
SELECT m.address, f.name
FROM   manual_tags m
JOIN   follow_exchange f ON regexp_replace(m.label, '\s*\(.*\)$', '') = f.name
ON CONFLICT DO NOTHING;
INSERT INTO seed
SELECT a.address, regexp_replace(a.label, '\s*\(.*\)$', '')
FROM   address_tags_auto a
ON CONFLICT DO NOTHING;

CREATE TEMP TABLE params AS
SELECT (extract(epoch FROM now() - make_interval(hours => :hours)) * 1000)::bigint AS since_ms;

DO $$
DECLARE
    added int;
    rounds int := 0;
BEGIN
    -- A chain can move several times inside one window; repeat until stable.
    LOOP
        rounds := rounds + 1;

        WITH cand AS (
            SELECT DISTINCT at.transaction_id
            FROM   seed s
            JOIN   addresses_transactions at ON at.address = s.address
            WHERE  at.block_time >= (SELECT since_ms FROM params)
        ),
        ins AS (
            SELECT t.transaction_id,
                   t.block_time,
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
            -- Rule 1: all inputs resolved and owned by a single exchange.
            SELECT i.transaction_id,
                   min(i.block_time)   AS block_time,
                   min(s.exchange)     AS exchange,
                   min(i.address)      AS parent_address,
                   sum(i.amount)       AS input_sompi
            FROM   ins i
            LEFT   JOIN seed s ON s.address = i.address
            GROUP  BY i.transaction_id
            HAVING bool_and(s.address IS NOT NULL)
               AND count(DISTINCT s.exchange) = 1
        ),
        change AS (
            SELECT DISTINCT ON (sp.transaction_id)
                   sp.transaction_id, sp.block_time, sp.exchange, sp.parent_address,
                   sp.input_sompi,
                   (SELECT count(DISTINCT (o2).script_public_key_address)
                    FROM unnest(t.outputs) AS o2)  AS output_addresses,
                   (SELECT bool_or(s2.address IS NOT NULL)
                    FROM unnest(t.outputs) AS o3
                    LEFT JOIN seed s2 ON s2.address = (o3).script_public_key_address
                                     AND s2.exchange = sp.exchange) AS change_to_self,
                   (o).script_public_key_address AS address,
                   (o).amount                    AS amount
            FROM   spends sp
            JOIN   transactions t ON t.transaction_id = sp.transaction_id
            CROSS  JOIN LATERAL unnest(t.outputs) AS o
            ORDER  BY sp.transaction_id, (o).amount DESC
        )
        INSERT INTO address_tags_auto (address, label, parent_address, transaction_id)
        SELECT ch.address, ch.exchange || ' (auto)', ch.parent_address, ch.transaction_id
        FROM   change ch
        WHERE  ch.amount * 10 >= ch.input_sompi * 9                              -- rule 2
          AND  ch.output_addresses >= 2                                           -- rule 4
          AND  NOT ch.change_to_self
          AND  NOT EXISTS (SELECT 1 FROM seed s        WHERE s.address = ch.address)
          AND  NOT EXISTS (SELECT 1 FROM manual_tags m WHERE m.address = ch.address)
          AND  NOT EXISTS (                                                       -- rule 3
                   SELECT 1 FROM addresses_transactions at
                   WHERE  at.address = ch.address AND at.block_time < ch.block_time)
        ON CONFLICT (address) DO NOTHING;

        GET DIAGNOSTICS added = ROW_COUNT;
        EXIT WHEN added = 0 OR rounds >= 20;

        INSERT INTO seed
        SELECT a.address, regexp_replace(a.label, '\s*\(.*\)$', '')
        FROM   address_tags_auto a
        ON CONFLICT DO NOTHING;
    END LOOP;
END $$;

SELECT label, count(*) AS addresses FROM address_tags_auto GROUP BY label ORDER BY label;
