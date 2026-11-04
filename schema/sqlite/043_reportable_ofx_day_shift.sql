-- Migration 043: an OFX row one day off its Pluggy twin is still a duplicate.
--
-- OFX statements and Pluggy disagree by a day on some postings (the bank's
-- statement date vs. Pluggy's UTC timestamp), so migration 038's exact-date
-- anti-join kept those OFX rows and reports counted the purchase twice. The
-- OFX dedup now matches a Pluggy row with the same account, amount_cents and
-- normalised raw_description dated within one day. Everything else in
-- v_transactions_reportable is unchanged from migration 042.

DROP VIEW IF EXISTS v_transactions_reportable;
CREATE VIEW v_transactions_reportable AS
SELECT
  t.transaction_id,
  t.account_id,
  t.transaction_date,
  t.raw_description,
  t.description,
  t.merchant_name,
  t.purpose,
  t.amount,
  t.amount_cents,
  t.tx_type,
  t.category_id,
  t.category_source,
  t.context,
  t.classifier_trace,
  t.payment_status,
  t.source,
  t.actor_id,
  t.idempotency_key,
  t.metadata_json,
  t.created_at,
  t.updated_at,
  t.enrichment_attempted_at,
  t.display_emoji,
  t.display_label,
  t.category_display
FROM v_transactions_effective t
LEFT JOIN (
  SELECT account_id, amount_cents, transaction_date, 1 AS matched
  FROM v_transactions_effective
  WHERE source = 'pluggy'
) legacy_match
  ON t.source = 'legacy'
  AND t.transaction_id LIKE 'manual_%'
  AND legacy_match.account_id = t.account_id
  AND legacy_match.amount_cents = t.amount_cents
  AND legacy_match.transaction_date
        BETWEEN date(t.transaction_date, '-7 day') AND date(t.transaction_date, '+7 day')
LEFT JOIN (
  SELECT
    COALESCE(account_id, '') AS account_key,
    amount_cents,
    transaction_date,
    LOWER(TRIM(raw_description)) AS desc_key,
    1 AS matched
  FROM v_transactions_effective
  WHERE source = 'pluggy'
) ofx_match
  ON t.source = 'ofx'
  AND ofx_match.account_key = COALESCE(t.account_id, '')
  AND ofx_match.amount_cents = t.amount_cents
  AND ofx_match.transaction_date
        BETWEEN date(t.transaction_date, '-1 day') AND date(t.transaction_date, '+1 day')
  AND ofx_match.desc_key = LOWER(TRIM(t.raw_description))
LEFT JOIN (
  SELECT
    COALESCE(account_id, '') AS account_key,
    amount_cents,
    transaction_date,
    LOWER(TRIM(raw_description)) AS desc_key,
    json_extract(metadata_json, '$.raw.creditCardMetadata.installmentNumber') AS installment_number,
    json_extract(metadata_json, '$.raw.creditCardMetadata.totalInstallments') AS installment_total,
    1 AS matched
  FROM v_transactions_effective
  WHERE source = 'pluggy'
    AND json_extract(metadata_json, '$.raw.creditCardMetadata.billId') IS NOT NULL
    AND json_extract(metadata_json, '$.raw.creditCardMetadata.installmentNumber') IS NOT NULL
) posted_match
  ON t.source = 'pluggy'
  AND json_extract(t.metadata_json, '$.raw.creditCardMetadata.billId') IS NULL
  AND posted_match.installment_number = json_extract(t.metadata_json, '$.raw.creditCardMetadata.installmentNumber')
  AND posted_match.installment_total = json_extract(t.metadata_json, '$.raw.creditCardMetadata.totalInstallments')
  AND posted_match.account_key = COALESCE(t.account_id, '')
  AND posted_match.amount_cents = t.amount_cents
  AND posted_match.desc_key = LOWER(TRIM(t.raw_description))
  AND ABS(julianday(posted_match.transaction_date) - julianday(t.transaction_date)) <= 40
WHERE legacy_match.matched IS NULL
  AND ofx_match.matched IS NULL
  AND posted_match.matched IS NULL;
