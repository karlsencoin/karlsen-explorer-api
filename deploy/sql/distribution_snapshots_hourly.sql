-- Hourly tier snapshots for exact 24 h / 7 d deltas on /analytics/addresses/distribution-summary.
--
-- Runs once per hour from karlsen-distribution-snapshot-hourly.timer as a single
-- psql call (no long-lived process). One aggregate over `balances` (~85 k rows,
-- tens of milliseconds) plus 8 inserted rows; rows older than 8 days are pruned,
-- so the table stays at roughly 8 x 24 x 8 = 1,536 rows.
--
-- Tier bounds must match TIERS in crates/explorer-api/src/routes/analytics.rs.

CREATE TABLE IF NOT EXISTS distribution_snapshots_hourly (
    taken_at      timestamptz NOT NULL,
    tier          varchar(32) NOT NULL,
    address_count integer     NOT NULL,
    tier_sompi    bigint      NOT NULL,
    PRIMARY KEY (taken_at, tier)
);

-- The API connects as the read-only role; without this the endpoint fails
-- with "permission denied" after the table is first created.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'explorer_ro') THEN
        GRANT SELECT ON distribution_snapshots_hourly TO explorer_ro;
    END IF;
END $$;

BEGIN;

INSERT INTO distribution_snapshots_hourly (taken_at, tier, address_count, tier_sompi)
SELECT date_trunc('minute', now()), tier, count(*), sum(amount)::bigint
FROM (
    SELECT CASE
             WHEN amount >= 1000000000000000                               THEN '10M+'
             WHEN amount >=  100000000000000 AND amount < 1000000000000000 THEN '1M-10M'
             WHEN amount >=   50000000000000 AND amount <  100000000000000 THEN '500K-1M'
             WHEN amount >=   10000000000000 AND amount <   50000000000000 THEN '100K-500K'
             WHEN amount >=    1000000000000 AND amount <   10000000000000 THEN '10K-100K'
             WHEN amount >=     100000000000 AND amount <    1000000000000 THEN '1K-10K'
             WHEN amount >=      10000000000 AND amount <     100000000000 THEN '100-1K'
             WHEN amount >=        100000000 AND amount <      10000000000 THEN '1-100'
           END AS tier,
           amount
    FROM balances
    WHERE amount >= 100000000
) b
WHERE tier IS NOT NULL
GROUP BY tier
ON CONFLICT (taken_at, tier) DO NOTHING;

DELETE FROM distribution_snapshots_hourly WHERE taken_at < now() - interval '8 days';

COMMIT;
