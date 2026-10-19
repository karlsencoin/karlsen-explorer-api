-- Rich list / distribution test data. Balances span every tier, include a tie and
-- sums above 2^53 sompi (where float shortcuts lose precision).
INSERT INTO balances (id, address, amount) VALUES
 (1,  'karlsen:qwhale1', 1234567890123456789 / 100),   -- 123M KLS (10M+)
 (2,  'karlsen:qorca01', 345678901234567),             -- 3.45M KLS
 (3,  'karlsen:qorca02', 345678901234567),             -- same balance (tie)
 (4,  'karlsen:qshark1', 76543210987654),              -- 765K KLS
 (5,  'karlsen:qtuna01', 12345678901234),              -- 123K KLS
 (6,  'karlsen:qfish01', 2345678901234),               -- 23K KLS
 (7,  'karlsen:qfish02', 999999999999),                -- just under 10K KLS
 (8,  'karlsen:qcrab01', 12345678901),                 -- 123 KLS
 (9,  'karlsen:qplnk01', 100000000),                   -- exactly 1 KLS
 (10, 'karlsen:qdust01', 99999999),                    -- below 1 KLS: no tier
 (11, 'karlsen:qzero01', 0),                           -- empty: excluded
 (12, 'karlsen:qnewbie', 5000000000000);               -- 50K KLS, new today

-- Yesterday's rich-list snapshot (and an older one that must be ignored).
INSERT INTO top_balance_snapshots (snapshot_date, address, amount) VALUES
 (CURRENT_DATE - 1, 'karlsen:qwhale1', 1234567890123456789 / 100 - 4200000000000),
 (CURRENT_DATE - 1, 'karlsen:qorca01', 345678901234567),
 (CURRENT_DATE - 1, 'karlsen:qorca02', 345678901234567 + 123456789),
 (CURRENT_DATE - 1, 'karlsen:qshark1', 70000000000000),
 (CURRENT_DATE - 1, 'karlsen:qtuna01', 12345678901234),
 (CURRENT_DATE - 1, 'karlsen:qfish01', 2000000000000),
 (CURRENT_DATE - 1, 'karlsen:qfish02', 999999999999),
 (CURRENT_DATE - 1, 'karlsen:qcrab01', 12345678901),
 (CURRENT_DATE - 1, 'karlsen:qplnk01', 100000000),
 (CURRENT_DATE - 3, 'karlsen:qnewbie', 1);

-- Distribution snapshots: full 1-day baseline; 7-day baseline from before the
-- tier_sompi column existed for one tier (balance delta must become null).
INSERT INTO distribution_snapshots (snapshot_date, tier, address_count, total_addresses, tier_sompi) VALUES
 (CURRENT_DATE - 1, '10M+',      1, 9, 1234567890123456789 / 100 - 4200000000000),
 (CURRENT_DATE - 1, '1M-10M',    2, 9, 691357802469134),
 (CURRENT_DATE - 1, '500K-1M',   0, 9, 0),
 (CURRENT_DATE - 1, '100K-500K', 1, 9, 12345678901234),
 (CURRENT_DATE - 1, '10K-100K',  1, 9, 2000000000000),
 (CURRENT_DATE - 1, '1K-10K',    1, 9, 999999999999),
 (CURRENT_DATE - 1, '100-1K',    1, 9, 12345678901),
 (CURRENT_DATE - 1, '1-100',     2, 9, 200000000),
 (CURRENT_DATE - 7, '10M+',      1, 7, 1000000000000000000 / 100),
 (CURRENT_DATE - 7, '1M-10M',    1, 7, NULL),
 (CURRENT_DATE - 7, '1-100',     5, 7, 500000000);
