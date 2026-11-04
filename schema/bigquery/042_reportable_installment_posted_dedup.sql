-- Migration 042: drop Pluggy's projected installment when its posted bill line exists.
--
-- Pluggy emits each card installment twice: a projected row created at
-- purchase time (dated in the expected month, no `billId`) and, once the bill
-- closes, the posted bill line (`billId` set, dated at the bill's post date).
-- The posted line usually arrives with a back-dated `transaction_date`, so
-- incremental syncs skipped it; a full-history sync brought both in and every
-- report counted the installment twice, sometimes in different bills.
--
-- v_transactions_reportable now also drops a projected Pluggy installment when
-- a posted Pluggy row exists for the same account, installment number/total,
-- amount_cents and normalised raw_description within 40 days. The posted line
-- wins because its date is the bill it actually landed in. Same LEFT JOIN
-- anti-join shape as migration 038 (BigQuery cannot de-correlate EXISTS
-- through the nested views); nothing is deleted from `transactions`.

CREATE OR REPLACE VIEW `{{project_id}}.{{dataset_id}}.v_transactions_reportable` AS
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
FROM `{{project_id}}.{{dataset_id}}.v_transactions_effective` t
LEFT JOIN (
  SELECT account_id, amount_cents, transaction_date, 1 AS matched
  FROM `{{project_id}}.{{dataset_id}}.v_transactions_effective`
  WHERE source = 'pluggy'
) legacy_match
  ON t.source = 'legacy'
  AND STARTS_WITH(t.transaction_id, 'manual_')
  AND legacy_match.account_id = t.account_id
  AND legacy_match.amount_cents = t.amount_cents
  AND legacy_match.transaction_date
        BETWEEN DATE_SUB(t.transaction_date, INTERVAL 7 DAY)
        AND DATE_ADD(t.transaction_date, INTERVAL 7 DAY)
LEFT JOIN (
  SELECT
    COALESCE(account_id, '') AS account_key,
    amount_cents,
    transaction_date,
    LOWER(TRIM(raw_description)) AS desc_key,
    1 AS matched
  FROM `{{project_id}}.{{dataset_id}}.v_transactions_effective`
  WHERE source = 'pluggy'
) ofx_match
  ON t.source = 'ofx'
  AND ofx_match.account_key = COALESCE(t.account_id, '')
  AND ofx_match.amount_cents = t.amount_cents
  AND ofx_match.transaction_date = t.transaction_date
  AND ofx_match.desc_key = LOWER(TRIM(t.raw_description))
LEFT JOIN (
  SELECT
    COALESCE(account_id, '') AS account_key,
    amount_cents,
    transaction_date,
    LOWER(TRIM(raw_description)) AS desc_key,
    JSON_VALUE(metadata_json, '$.raw.creditCardMetadata.installmentNumber') AS installment_number,
    JSON_VALUE(metadata_json, '$.raw.creditCardMetadata.totalInstallments') AS installment_total,
    1 AS matched
  FROM `{{project_id}}.{{dataset_id}}.v_transactions_effective`
  WHERE source = 'pluggy'
    AND JSON_VALUE(metadata_json, '$.raw.creditCardMetadata.billId') IS NOT NULL
    AND JSON_VALUE(metadata_json, '$.raw.creditCardMetadata.installmentNumber') IS NOT NULL
) posted_match
  ON t.source = 'pluggy'
  AND JSON_VALUE(t.metadata_json, '$.raw.creditCardMetadata.billId') IS NULL
  AND posted_match.installment_number = JSON_VALUE(t.metadata_json, '$.raw.creditCardMetadata.installmentNumber')
  AND posted_match.installment_total = JSON_VALUE(t.metadata_json, '$.raw.creditCardMetadata.totalInstallments')
  AND posted_match.account_key = COALESCE(t.account_id, '')
  AND posted_match.amount_cents = t.amount_cents
  AND posted_match.desc_key = LOWER(TRIM(t.raw_description))
  AND ABS(DATE_DIFF(posted_match.transaction_date, t.transaction_date, DAY)) <= 40
WHERE legacy_match.matched IS NULL
  AND ofx_match.matched IS NULL
  AND posted_match.matched IS NULL;
