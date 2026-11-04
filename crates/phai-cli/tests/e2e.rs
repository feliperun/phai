use assert_cmd::prelude::*;
use chrono::{Datelike, Duration, Utc};
use predicates::prelude::*;
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn cargo_bin() -> Command {
    Command::cargo_bin("phai").expect("phai binary")
}

fn envs<'a>(cmd: &'a mut Command, config_dir: &Path, data_dir: &Path) -> &'a mut Command {
    cmd.env("FINANCE_OS_CONFIG_DIR", config_dir)
        .env("FINANCE_OS_DATA_DIR", data_dir)
        // Tests must never trigger the self-updater: it overwrites the
        // very `target/debug/phai` binary cargo just built, causing
        // subsequent assertions to run against an old release artifact
        // and producing false negatives.
        .env("FINANCE_OS_NO_AUTO_UPDATE", "1")
        // Keep post-sync enrichment offline and deterministic even when the
        // developer's shell carries real LLM credentials.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, content).expect("write file");
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// `--days` window that still reaches the fixed March 2026 fixture rows, so
/// report assertions do not expire as the calendar moves on.
fn days_covering_fixture() -> String {
    let fixture_start = chrono::NaiveDate::from_ymd_opt(2026, 3, 1).expect("fixture start");
    ((Utc::now().date_naive() - fixture_start).num_days() + 1).to_string()
}

fn setup_local_auth_migrate(config_dir: &Path, data_dir: &Path) {
    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        config_dir,
        data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        config_dir,
        data_dir,
    )
    .assert()
    .success();
}

fn seed_fixture_sync(temp: &TempDir, config_dir: &Path, data_dir: &Path) {
    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking" },
    { "id": "shared_credit", "pluggyAccountId": "fixture-credit" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\nshared_credit,secondary,credit,fintech,Shared Credit Card,fixture-credit,item-2,3,10\n",
    );

    let fixture_path = repo_root().join("examples/pluggy_fixture.json");

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31"),
        config_dir,
        data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("- transactions: 4"));
}

#[test]
fn transaction_anatomy_fields_and_pending_commands_work() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let (raw, description): (String, Option<String>) = conn
        .query_row(
            "SELECT raw_description, description
             FROM transactions
             WHERE transaction_id = 'pluggy-fixture-001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("anatomy row");
    assert_eq!(raw, "Supermercado Angeloni");
    assert!(description.is_none());

    envs(
        cargo_bin()
            .arg("tx")
            .arg("set-anatomy")
            .arg("--transaction-id")
            .arg("pluggy-fixture-001")
            .arg("--description")
            .arg("Compra mensal de mercado")
            .arg("--merchant-name")
            .arg("Mercado Exemplo")
            .arg("--purpose")
            .arg("Reposição da cozinha"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let (description, merchant, purpose): (String, String, String) = conn
        .query_row(
            "SELECT description, merchant_name, purpose
             FROM transactions
             WHERE transaction_id = 'pluggy-fixture-001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("updated anatomy row");
    assert_eq!(description, "Compra mensal de mercado");
    assert_eq!(merchant, "Mercado Exemplo");
    assert_eq!(purpose, "Reposição da cozinha");

    let pending = envs(
        cargo_bin()
            .arg("tx")
            .arg("pending-human")
            .arg("--kind")
            .arg("description")
            .arg("--json")
            .arg("--limit")
            .arg("20"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let rows: Value = serde_json::from_slice(&pending).expect("pending json");
    assert!(rows
        .as_array()
        .expect("array")
        .iter()
        .all(|row| row["transaction_id"] != "pluggy-fixture-001"));

    let review_summary = envs(
        cargo_bin()
            .arg("tx")
            .arg("review-human")
            .arg("--summary")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let summary: Value = serde_json::from_slice(&review_summary).expect("review summary json");
    assert_eq!(summary["missingDescriptionCount"], 3);
    assert_eq!(summary["missingMerchantCount"], 3);
    assert!(summary["suggestedNextCommand"]
        .as_str()
        .expect("next command")
        .contains("tx review-human --kind all"));

    let review_queue = envs(
        cargo_bin()
            .arg("tx")
            .arg("review-human")
            .arg("--kind")
            .arg("all")
            .arg("--json")
            .arg("--limit")
            .arg("20"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let review_rows: Value = serde_json::from_slice(&review_queue).expect("review queue json");
    assert!(review_rows
        .as_array()
        .expect("array")
        .iter()
        .any(|row| row["transaction_id"] != "pluggy-fixture-001"));

    let review_result = envs(
        cargo_bin()
            .arg("tx")
            .arg("review-human")
            .arg("--transaction-id")
            .arg("pluggy-fixture-003")
            .arg("--description")
            .arg("Refeição revisada")
            .arg("--merchant-name")
            .arg("Restaurante Exemplo")
            .arg("--category")
            .arg("alimentacao:restaurantes")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let applied_rows: Value = serde_json::from_slice(&review_result).expect("review result json");
    let applied = applied_rows
        .as_array()
        .expect("review result array")
        .first()
        .expect("one applied result");
    assert_eq!(applied["transactionId"], "pluggy-fixture-003");
    assert_eq!(applied["updatedDescription"], true);
    assert_eq!(applied["updatedMerchantName"], true);
    assert_eq!(applied["updatedCategory"], true);

    let (description, merchant, category): (String, String, String) = conn
        .query_row(
            "SELECT description, merchant_name, category_id
             FROM transactions
             WHERE transaction_id = 'pluggy-fixture-003'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("reviewed anatomy row");
    assert_eq!(description, "Refeição revisada");
    assert_eq!(merchant, "Restaurante Exemplo");
    assert_eq!(category, "alimentacao:restaurantes");
}

#[test]
fn review_human_queue_filters_by_month_account_category_and_merchant() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let review_queue = envs(
        cargo_bin()
            .arg("tx")
            .arg("review-human")
            .arg("--kind")
            .arg("all")
            .arg("--json")
            .arg("--limit")
            .arg("20")
            .arg("--month")
            .arg("2026-03")
            .arg("--account-id")
            .arg("shared_credit")
            .arg("--category")
            .arg("transporte:combustivel")
            .arg("--merchant")
            .arg("posto"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let rows: Value = serde_json::from_slice(&review_queue).expect("review queue json");
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["transactionId"], "pluggy-fixture-003");
    assert_eq!(rows[0]["accountId"], "shared_credit");
    assert_eq!(rows[0]["categoryId"], "transporte:combustivel");
}

#[test]
fn milestone_zero_local_sync_and_report() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("local-dev"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    // The new pulse format is a proactive headline-driven summary; it
    // does not list individual transactions in the body. The `--raw`
    // variant below is what scripts/skills should consume for the
    // backwards-compatible per-transaction view.
    envs(
        cargo_bin()
            .arg("report")
            .arg("daily-pulse")
            .arg("--days")
            .arg(days_covering_fixture()),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Pulso"))
    .stdout(predicate::str::contains("Mês até dia"));

    envs(
        cargo_bin()
            .arg("report")
            .arg("daily-pulse")
            .arg("--days")
            .arg(days_covering_fixture())
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Supermercado Angeloni"))
    .stdout(predicate::str::contains("Pagamento recebido"));

    envs(
        cargo_bin()
            .arg("report")
            .arg("monthly-spend")
            .arg("--month")
            .arg("2026-03")
            .arg("--csv"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::starts_with(
        "account_id,category_id,expense_count,expenses,month_ref\n",
    ))
    .stdout(predicate::str::contains("2026-03"));
}

#[test]
fn split_commands_are_bigquery_only_on_local_backend() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    let unsupported = "transaction split/detailing is supported only on the BigQuery backend";

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    for args in [
        vec![
            "tx",
            "split",
            "preview",
            "--transaction-id",
            "tx-1",
            "--payload",
            "missing.json",
        ],
        vec![
            "tx",
            "split",
            "apply",
            "--transaction-id",
            "tx-1",
            "--payload",
            "missing.json",
        ],
        vec!["tx", "split", "show", "--transaction-id", "tx-1"],
        vec![
            "tx",
            "split",
            "clear",
            "--transaction-id",
            "tx-1",
            "--reason",
            "test",
        ],
        vec!["report", "split-candidates", "--json"],
        vec!["report", "item-prices", "--query", "leite", "--json"],
    ] {
        envs(cargo_bin().args(args), &config_dir, &data_dir)
            .assert()
            .failure()
            .stderr(predicate::str::contains(unsupported));
    }
}

#[test]
fn import_legacy_is_idempotent() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    let finance_root = temp.path().join("legacy");
    fs::create_dir_all(finance_root.join("data/2026")).expect("create legacy data dir");

    write_file(
        &finance_root.join("data/contas.csv"),
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\n",
    );
    write_file(
        &finance_root.join("data/2026/transacoes.csv"),
        "pluggy_id,data,mes_ref,mes_movimento_ref,fatura_mes_ref,data_fechamento_fatura,data_vencimento,data_pagamento,status_pagamento,competencia_tipo,conta_id,conta_owner,conta_tipo,conta_banco,conta_label,valor,descricao,descricao_raw,categoria,subcategoria,descricao_canonica,contexto_finalidade,recorrencia_tipo,recorrencia_chave,recorrencia_frequencia,recorrencia_dia,recorrencia_origem,classificacao_fonte,classificacao_regra,tipo,data_hora_iso,pluggy_account_id,pluggy_amount_raw,pluggy_currency_code,pluggy_amount_in_account_currency,pluggy_balance,pluggy_category,pluggy_category_id,pluggy_provider_code,pluggy_provider_id,pluggy_status,pluggy_operation_type,pluggy_order,pluggy_created_at,pluggy_updated_at,payment_data_json,payment_method,payment_reason,payment_reference_number,payment_receiver_reference_id,payment_boleto_digitable_line,payment_boleto_barcode,payment_boleto_base_amount,payment_boleto_interest_amount,payment_boleto_penalty_amount,payment_boleto_discount_amount,payer_name,payer_branch_number,payer_account_number,payer_routing_number,payer_routing_number_ispb,payer_document_type,payer_document_value,receiver_name,receiver_branch_number,receiver_account_number,receiver_routing_number,receiver_routing_number_ispb,receiver_document_type,receiver_document_value,credit_card_metadata_json,credit_card_card_number,credit_card_payee_mcc,credit_card_installment_number,credit_card_total_installments,credit_card_total_amount,credit_card_purchase_date,credit_card_bill_id,merchant_json,merchant_name,merchant_business_name,merchant_cnpj,merchant_cnae,merchant_category,acquirer_data_json,raw_transaction_json\npluggy-1,2026-03-01,2026-03,2026-03,,,,,pago,conta_corrente,primary_checking,primary,checking,fintech,Primary Checking,-42.50,Mercado,Mercado,Alimentacao,Mercado,,, ,,,, ,rule,mercado_rule,DEBIT,2026-03-01T12:00:00.000Z,fixture-checking,,,,,,,,,POSTED,,,,2026-03-01T12:00:00.000Z,2026-03-01T12:00:00.000Z,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,\n",
    );
    write_file(
        &finance_root.join("contexto_transacoes.csv"),
        "match_type,match_value,valor_match,categoria,subcategoria,rotulo,finalidade,tipo_contexto,notas,ativo,revisado_em\ndescricao_contains,mercado,,Alimentacao,Mercado,Mercado do mês,Compras de mercado,contexto,,1,2026-03-01\n",
    );
    write_file(
        &finance_root.join("data/forecast_templates.csv"),
        "id,tipo,descricao,categoria,subcategoria,conta_id,valor,frequencia,dia_vencimento,inicio_mes,fim_mes,parcelas_total,match_contains,status,impacta_total,origem,notas\naluguel,recorrente,Aluguel,Moradia,Aluguel,primary_checking,1000.00,mensal,5,2026-03,2026-12,,aluguel,ativo,true,teste,\n",
    );
    write_file(
        &finance_root.join("rules.yaml"),
        "version: 1\ncategories:\n  - id: mercado_rule\n    match:\n      contains_any: [\"mercado\"]\n    set:\n      category: Alimentacao\n      subcategory: Mercado\n",
    );

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    for _ in 0..2 {
        envs(
            cargo_bin()
                .arg("admin")
                .arg("import-legacy")
                .arg("--finance-root")
                .arg(&finance_root),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("- transactions: 1"));
    }

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let tx_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))
        .expect("count transactions");
    let rule_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM rules", [], |row| row.get(0))
        .expect("count rules");
    let forecast_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM forecast", [], |row| row.get(0))
        .expect("count forecast");

    assert_eq!(tx_count, 1);
    assert_eq!(rule_count, 2);
    assert_eq!(forecast_count, 1);
}

#[test]
fn mutating_commands_feed_reporting_views() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("reserva_emergencia")
            .arg("--owner")
            .arg("primary")
            .arg("--account-type")
            .arg("savings")
            .arg("--bank")
            .arg("itau")
            .arg("--label")
            .arg("Reserva Emergencia"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Conta salva: reserva_emergencia"));

    envs(
        cargo_bin()
            .arg("rule")
            .arg("upsert")
            .arg("--rule-id")
            .arg("farmacia_rule")
            .arg("--body")
            .arg("if description contains farmacia then category saude:farmacia"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Rule salva: farmacia_rule"));

    envs(
        cargo_bin()
            .arg("forecast")
            .arg("upsert")
            .arg("--forecast-id")
            .arg("mercado_planejado")
            .arg("--date")
            .arg("2026-03-30")
            .arg("--description")
            .arg("Mercado planejado")
            .arg("--amount")
            .arg("200.00")
            .arg("--category")
            .arg("Alimentacao")
            .arg("--subcategory")
            .arg("Mercado")
            .arg("--account-id")
            .arg("primary_checking"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "Forecast salvo: mercado_planejado",
    ));

    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("manual-card-001")
            .arg("--account-id")
            .arg("shared_credit")
            .arg("--date")
            .arg("2026-03-27")
            .arg("--description")
            .arg("Farmacia Cartao")
            .arg("--amount=-87.45")
            .arg("--payment-status")
            .arg("pending"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "Transação manual salva: manual-card-001",
    ));

    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("manual-checking-001")
            .arg("--account-id")
            .arg("primary_checking")
            .arg("--date")
            .arg("2026-03-27")
            .arg("--description")
            .arg("Exame Laboratorio")
            .arg("--amount=-120.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "Transação manual salva: manual-checking-001",
    ));

    envs(
        cargo_bin()
            .arg("tx")
            .arg("categorize")
            .arg("--transaction-id")
            .arg("manual-checking-001")
            .arg("--category")
            .arg("Saude")
            .arg("--subcategory")
            .arg("Exames")
            .arg("--context")
            .arg("Exame de rotina"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("tx")
            .arg("set-context")
            .arg("--transaction-id")
            .arg("manual-checking-001")
            .arg("--context")
            .arg("Exame de rotina anual"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("report")
            .arg("monthly-spend")
            .arg("--month")
            .arg("2026-03"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    // human format: family groups use display labels (capitalized, with accents)
    .stdout(predicate::str::contains("Alimentação"))
    .stdout(predicate::str::contains("Saúde"))
    .stdout(predicate::str::contains("financeiro-pagamento-recebido").not());

    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-03"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    // human format: header shows "Caixa · março/2026" and saldo lines
    .stdout(predicate::str::contains("Caixa · março/2026"))
    .stdout(predicate::str::contains("Saldo inicial"))
    .stdout(predicate::str::contains("Saldo final"));

    envs(
        cargo_bin()
            .arg("report")
            .arg("forecast-vs-actual")
            .arg("--month")
            .arg("2026-03"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Mercado planejado"))
    .stdout(predicate::str::contains("realizado"));

    envs(
        cargo_bin()
            .arg("report")
            .arg("card-summary")
            .arg("--month")
            .arg("2026-03"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("shared_credit"))
    .stdout(predicate::str::contains("Cartões"));

    envs(
        cargo_bin()
            .arg("report")
            .arg("uncategorized")
            .arg("--limit")
            .arg("10"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Farmacia Cartao"))
    .stdout(predicate::str::contains("Exame Laboratorio").not());

    let health = envs(
        cargo_bin()
            .arg("report")
            .arg("data-health")
            .arg("--days")
            .arg(days_covering_fixture())
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("data-health output");
    assert!(health.status.success());
    let health_json: Value = serde_json::from_slice(&health.stdout).expect("data-health json");
    assert_eq!(health_json["uncategorizedCount"], 1);
    assert_eq!(health_json["windowPluggyRows"], 4);
    assert_eq!(health_json["windowOtherRows"], 2);
    assert!(health_json["flatCategoryRows"].as_u64().unwrap() > 0);
    assert_eq!(health_json["overlapCandidatesCount"], 0);

    let scenario = envs(
        cargo_bin()
            .arg("report")
            .arg("scenario")
            .arg("--month")
            .arg("2026-04")
            .arg("--history-months")
            .arg("1")
            .arg("--extra-expense")
            .arg("80")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("scenario output");
    assert!(scenario.status.success());
    let scenario_json: Value = serde_json::from_slice(&scenario.stdout).expect("scenario json");
    assert_eq!(scenario_json["targetMonth"], "2026-04");
    assert_eq!(scenario_json["baselineMonths"][0], "2026-03");
    assert_eq!(scenario_json["extraExpense"], "80");
    assert_ne!(scenario_json["carryoverOpenCardAmount"], "0");

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let saved_context: String = conn
        .query_row(
            "SELECT context FROM transactions WHERE transaction_id = 'manual-checking-001'",
            [],
            |row| row.get(0),
        )
        .expect("saved context");
    let saved_category: String = conn
        .query_row(
            "SELECT category_id FROM transactions WHERE transaction_id = 'manual-checking-001'",
            [],
            |row| row.get(0),
        )
        .expect("saved category");
    let account_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE account_id = 'reserva_emergencia'",
            [],
            |row| row.get(0),
        )
        .expect("count account");
    let rule_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM rules WHERE rule_id = 'farmacia_rule'",
            [],
            |row| row.get(0),
        )
        .expect("count rule");
    let forecast_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE forecast_id = 'mercado_planejado'",
            [],
            |row| row.get(0),
        )
        .expect("count forecast");

    assert_eq!(saved_context, "Exame de rotina anual");
    assert_eq!(saved_category, "saude:exames");
    assert_eq!(account_count, 1);
    assert_eq!(rule_count, 1);
    assert_eq!(forecast_count, 1);

    // Credit-card debit from Pluggy (positive +150) must be stored as negative
    let cc_debit_amount: String = conn
        .query_row(
            "SELECT amount FROM transactions WHERE transaction_id = 'pluggy-fixture-003'",
            [],
            |row| row.get(0),
        )
        .expect("credit card debit amount");
    let cc_debit_val: f64 = cc_debit_amount.parse().expect("parse cc debit");
    assert!(
        cc_debit_val < 0.0,
        "credit-card debit must be negated, got {cc_debit_amount}"
    );
    assert!(
        (cc_debit_val - (-150.0)).abs() < 0.01,
        "credit-card debit must be -150, got {cc_debit_amount}"
    );

    // Credit-card credit (refund +42.50) must stay positive
    let cc_credit_amount: String = conn
        .query_row(
            "SELECT amount FROM transactions WHERE transaction_id = 'pluggy-fixture-004'",
            [],
            |row| row.get(0),
        )
        .expect("credit card credit amount");
    let cc_credit_val: f64 = cc_credit_amount.parse().expect("parse cc credit");
    assert!(
        cc_credit_val > 0.0,
        "credit-card credit must stay positive, got {cc_credit_amount}"
    );
}

#[test]
fn report_card_closed_insights_includes_categories_recurring_subscriptions_and_installments() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let manual_rows = [
        (
            "cc-rec-2026-01",
            "2026-01-18",
            "Academia Fit",
            "-180.00",
            Some(("Saude", "Academia")),
            "posted",
        ),
        (
            "cc-rec-2026-02",
            "2026-02-18",
            "Academia Fit",
            "-180.00",
            Some(("Saude", "Academia")),
            "posted",
        ),
        (
            "cc-rec-2026-03",
            "2026-03-18",
            "Academia Fit",
            "-180.00",
            Some(("Saude", "Academia")),
            "posted",
        ),
        (
            "cc-sub-2026-03",
            "2026-03-21",
            "Assinatura Cloud Backup",
            "-49.90",
            Some(("Assinaturas", "Cloud")),
            "posted",
        ),
        (
            "cc-inst-2026-03",
            "2026-03-22",
            "Notebook Pro 03/10",
            "-450.00",
            Some(("Tecnologia", "Eletronicos")),
            "posted",
        ),
        (
            "cc-inst-2026-03-parc",
            "2026-03-23",
            "Yelumseg Parc8",
            "-197.71",
            Some(("Transporte", "Seguro Auto")),
            "posted",
        ),
        (
            "cc-inst-open-2026-03",
            "2026-03-24",
            "Amazon Marketplace Parcela 4/6",
            "-145.60",
            Some(("Shopping", "Marketplace")),
            "pending",
        ),
    ];

    for (tx_id, date, description, amount, category, payment_status) in manual_rows {
        let mut cmd = cargo_bin();
        cmd.arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg(tx_id)
            .arg("--account-id")
            .arg("shared_credit")
            .arg("--date")
            .arg(date)
            .arg("--description")
            .arg(description)
            .arg(format!("--amount={amount}"))
            .arg("--payment-status")
            .arg(payment_status);
        if let Some((cat, sub)) = category {
            cmd.arg("--category").arg(cat).arg("--subcategory").arg(sub);
        }
        envs(&mut cmd, &config_dir, &data_dir).assert().success();
    }

    envs(
        cargo_bin()
            .arg("report")
            .arg("card-closed-insights")
            .arg("--month")
            .arg("2026-03"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Fatura fechada"))
    .stdout(predicate::str::contains("Recorrentes"))
    .stdout(predicate::str::contains("Assinaturas"))
    .stdout(predicate::str::contains("Parcelamentos"))
    .stdout(predicate::str::contains("academia fit"))
    .stdout(predicate::str::contains("03/10"))
    .stdout(predicate::str::contains("parcela-8"))
    .stdout(predicate::str::contains("4/6"));

    let json_output = envs(
        cargo_bin()
            .arg("report")
            .arg("card-closed-insights")
            .arg("--month")
            .arg("2026-03")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("card-closed-insights json output");
    assert!(json_output.status.success());
    let payload: Value = serde_json::from_slice(&json_output.stdout).expect("valid json payload");
    assert_eq!(payload["monthRef"], "2026-03");
    assert!(!payload["accounts"].as_array().unwrap().is_empty());
    assert!(!payload["categories"].as_array().unwrap().is_empty());
    assert!(!payload["recurring"].as_array().unwrap().is_empty());
    assert!(!payload["subscriptions"].as_array().unwrap().is_empty());
    assert!(!payload["closedInstallments"].as_array().unwrap().is_empty());
    assert!(!payload["openInstallments"].as_array().unwrap().is_empty());
    assert!(payload["closedInstallments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["marker"] == "parcela-8"));
    assert!(payload["openInstallments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["marker"] == "4/6"));
}

#[test]
fn report_views_exclude_legacy_manual_statement_when_pluggy_match_exists() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("manual_statement_test_dup")
            .arg("--account-id")
            .arg("shared_credit")
            .arg("--date")
            .arg("2026-03-26")
            .arg("--description")
            .arg("Posto de Gasolina")
            .arg("--amount=-150.00")
            .arg("--payment-status")
            .arg("posted"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    conn.execute(
        "UPDATE transactions SET source = 'legacy' WHERE transaction_id = 'manual_statement_test_dup'",
        [],
    )
    .expect("set manual source as legacy");

    let json_output = envs(
        cargo_bin()
            .arg("report")
            .arg("monthly-spend")
            .arg("--month")
            .arg("2026-03")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("monthly-spend json output");
    assert!(json_output.status.success());
    let payload: Value = serde_json::from_slice(&json_output.stdout).expect("valid json payload");
    let sum_expenses = payload
        .as_array()
        .expect("array payload")
        .iter()
        .map(|item| {
            item["expenses"]
                .as_str()
                .expect("expense string")
                .parse::<f64>()
                .expect("expense decimal")
        })
        .sum::<f64>();
    assert!(
        (sum_expenses - 302.30).abs() < 0.01,
        "monthly spend must ignore deduped manual statement row; got {sum_expenses}"
    );
}

#[test]
fn card_summary_excludes_legacy_manual_shadow() {
    // Regression: v_card_summary must honour the same legacy/manual ↔
    // pluggy dedup filter applied by v_transactions_reportable. A previous
    // refactor pointed it directly at `transactions`, which caused
    // shadowed manual statement rows to be counted twice.
    //
    // (cashflow is exercised separately — under cash-basis semantics it
    // only sees checking accounts, so a credit-card duplicate does not
    // surface there.)
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("manual_statement_cashflow_dup")
            .arg("--account-id")
            .arg("shared_credit")
            .arg("--date")
            .arg("2026-03-26")
            .arg("--description")
            .arg("Posto de Gasolina")
            .arg("--amount=-150.00")
            .arg("--payment-status")
            .arg("posted"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute(
        "UPDATE transactions SET source = 'legacy' WHERE transaction_id = 'manual_statement_cashflow_dup'",
        [],
    )
    .expect("set manual source as legacy");

    // Only the Pluggy -150.00 charge belongs to shared_credit; with dedup the
    // legacy shadow drops, so total_charges must remain 150.00 (not 300.00).
    let card_json = envs(
        cargo_bin()
            .arg("report")
            .arg("card-summary")
            .arg("--month")
            .arg("2026-03")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("card-summary raw output");
    assert!(card_json.status.success());
    let cards: Value = serde_json::from_slice(&card_json.stdout).expect("valid cards json");
    let card_row = cards
        .as_array()
        .expect("cards array")
        .iter()
        .find(|c| c["account_id"].as_str() == Some("shared_credit"))
        .expect("shared_credit row in card-summary");
    let total_charges: f64 = card_row["total_charges"]
        .as_str()
        .expect("total_charges decimal string")
        .parse()
        .expect("decimal parse");
    assert!(
        (total_charges - 150.0).abs() < 0.01,
        "card-summary must ignore deduped manual statement row; got {total_charges}"
    );
}

#[test]
fn cashflow_details_expands_paid_card_bill_and_forecast() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    for (account_id, account_type, label) in [
        ("primary_checking", "checking", "Primary Checking"),
        ("visa_credit", "credit", "Visa Credit"),
    ] {
        envs(
            cargo_bin()
                .arg("account")
                .arg("upsert")
                .arg("--account-id")
                .arg(account_id)
                .arg("--owner")
                .arg("test")
                .arg("--account-type")
                .arg(account_type)
                .arg("--bank")
                .arg("fixture-bank")
                .arg("--label")
                .arg(label),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute(
        "UPDATE accounts
         SET metadata_json = '{\"billing_closing_day\":10}'
         WHERE account_id = 'visa_credit'",
        [],
    )
    .expect("set credit metadata");

    let upsert_tx = |id: &str,
                     account: &str,
                     date: &str,
                     description: &str,
                     amount: &str,
                     category: &str,
                     subcategory: Option<&str>| {
        let mut cmd = cargo_bin();
        cmd.arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg(id)
            .arg("--account-id")
            .arg(account)
            .arg("--date")
            .arg(date)
            .arg("--description")
            .arg(description)
            .arg(format!("--amount={amount}"))
            .arg("--category")
            .arg(category)
            .arg("--payment-status")
            .arg("posted");
        if let Some(value) = subcategory {
            cmd.arg("--subcategory").arg(value);
        }
        envs(&mut cmd, &config_dir, &data_dir).assert().success();
    };

    upsert_tx(
        "salary_may",
        "primary_checking",
        "2026-05-05",
        "Salario",
        "1000.00",
        "Receitas",
        Some("Salario"),
    );
    upsert_tx(
        "rent_may",
        "primary_checking",
        "2026-05-06",
        "Aluguel",
        "-300.00",
        "Moradia",
        Some("Aluguel"),
    );
    upsert_tx(
        "card_payment_may",
        "primary_checking",
        "2026-05-15",
        "Pagamento de fatura Visa",
        "-230.00",
        "credit-card-payment",
        None,
    );
    upsert_tx(
        "card_installment_may",
        "visa_credit",
        "2026-05-03",
        "Notebook 1/3",
        "-100.00",
        "Compras",
        Some("Parceladas"),
    );
    upsert_tx(
        "card_subscription_may",
        "visa_credit",
        "2026-05-04",
        "Streaming assinatura",
        "-50.00",
        "Assinaturas",
        Some("Streaming"),
    );
    upsert_tx(
        "card_market_may",
        "visa_credit",
        "2026-05-05",
        "Mercado no cartao",
        "-80.00",
        "Alimentacao",
        Some("Mercado"),
    );

    let output = envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-05")
            .arg("--details")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let report: Value = serde_json::from_slice(&output).expect("valid cashflow detail json");
    assert_eq!(report["summary"]["income"].as_str(), Some("1000.00"));
    assert_eq!(report["summary"]["expenses"].as_str(), Some("530.00"));
    assert_eq!(report["summary"]["net"].as_str(), Some("470.00"));
    assert_eq!(
        report["details"]["cardBills"][0]["installments"]["subtotal"].as_str(),
        Some("-100.00")
    );
    assert_eq!(
        report["details"]["cardBills"][0]["subscriptions"]["subtotal"].as_str(),
        Some("-50.00")
    );
    assert_eq!(
        report["details"]["cardBills"][0]["otherCategories"][0]["subtotal"].as_str(),
        Some("-80.00")
    );
    let detail_text = String::from_utf8(output).expect("utf8 json");
    assert!(
        !detail_text.contains("Pagamento de fatura Visa"),
        "cashflow details must replace the checking-account card payment with bill details"
    );

    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-05")
            .arg("--details"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Saídas por categoria"))
    .stdout(predicate::str::contains("Cartões"))
    .stdout(predicate::str::contains("total de cartão de crédito"))
    .stdout(predicate::str::contains("compras parceladas").or(predicate::str::contains("Compras")))
    .stdout(predicate::str::contains("Pagamento de fatura Visa").not());

    envs(
        cargo_bin()
            .arg("forecast")
            .arg("upsert")
            .arg("--forecast-id")
            .arg("rent_forecast_june")
            .arg("--date")
            .arg("2026-06-10")
            .arg("--description")
            .arg("Aluguel previsto")
            .arg("--amount")
            .arg("200.00")
            .arg("--category")
            .arg("Moradia")
            .arg("--subcategory")
            .arg("Aluguel")
            .arg("--account-id")
            .arg("primary_checking"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let forecast_output = envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-06")
            .arg("--forecast")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let forecast_report: Value =
        serde_json::from_slice(&forecast_output).expect("valid forecast cashflow json");
    assert_eq!(
        forecast_report["forecastSummary"]["expenses"].as_str(),
        Some("200.00")
    );
    assert_eq!(forecast_report["summary"]["net"].as_str(), Some("-200.00"));

    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-06")
            .arg("--details")
            .arg("--forecast"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Cartões"))
    .stdout(predicate::str::contains("total de cartão de crédito"))
    .stdout(predicate::str::contains("sem fatura paga no período"));
}

#[test]
fn report_ofx_consistency_compares_transactions_row_by_row() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    for (tx_id, date, description, amount) in [
        ("tx-ofx-1", "2026-05-01", "Padaria Centro", "-20.00"),
        ("tx-only-db", "2026-05-02", "Transação Extra", "-15.50"),
    ] {
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(tx_id)
                .arg("--date")
                .arg(date)
                .arg("--description")
                .arg(description)
                .arg(format!("--amount={amount}"))
                .arg("--payment-status")
                .arg("posted"),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }

    let ofx_file = temp.path().join("check.ofx");
    write_file(
        &ofx_file,
        r#"OFXHEADER:100
DATA:OFXSGML
VERSION:102
<OFX>
<CREDITCARDMSGSRSV1>
<CCSTMTTRNRS>
<CCSTMTRS>
<CCACCTFROM>
<ACCTID>fixture-card-1</ACCTID>
</CCACCTFROM>
<BANKTRANLIST>
<DTSTART>20260501000000[-3:BRT]</DTSTART>
<DTEND>20260502000000[-3:BRT]</DTEND>
<STMTTRN>
<TRNTYPE>DEBIT</TRNTYPE>
<DTPOSTED>20260501000000[-3:BRT]</DTPOSTED>
<TRNAMT>-20.00</TRNAMT>
<FITID>fit-1</FITID>
<MEMO>Padaria Centro</MEMO>
</STMTTRN>
<STMTTRN>
<TRNTYPE>DEBIT</TRNTYPE>
<DTPOSTED>20260502</DTPOSTED>
<TRNAMT>-40.00</TRNAMT>
<FITID>fit-2</FITID>
<MEMO>Supermercado Delta</MEMO>
</STMTTRN>
</BANKTRANLIST>
</CCSTMTRS>
</CCSTMTTRNRS>
</CREDITCARDMSGSRSV1>
</OFX>
"#,
    );

    envs(
        cargo_bin()
            .arg("report")
            .arg("ofx-consistency")
            .arg("--ofx")
            .arg(&ofx_file),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("OFX consistency check"))
    .stdout(predicate::str::contains("faltando na base: 1"))
    .stdout(predicate::str::contains("sobrando na base: 1"))
    .stdout(predicate::str::contains("Padaria Centro"))
    .stdout(predicate::str::contains("Supermercado Delta"));

    let json_output = envs(
        cargo_bin()
            .arg("report")
            .arg("ofx-consistency")
            .arg("--ofx")
            .arg(&ofx_file)
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("ofx-consistency json output");
    assert!(json_output.status.success());
    let payload: Value = serde_json::from_slice(&json_output.stdout).expect("valid json payload");
    assert_eq!(payload["ofxTransactions"], 2);
    assert_eq!(payload["matched"], 1);
    assert_eq!(payload["missingInFinance"], 1);
    assert_eq!(payload["extraInFinance"], 1);
    assert_eq!(payload["consistent"], false);
}

#[test]
fn sync_json_summary_counts_only_new_transactions() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking" },
    { "id": "shared_credit", "pluggyAccountId": "fixture-credit" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\nshared_credit,secondary,credit,fintech,Shared Credit Card,fixture-credit,item-2,3,10\n",
    );

    let fixture_path = repo_root().join("examples/pluggy_fixture.json");

    let first = envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31")
            .arg("--json-summary"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("first sync output");
    assert!(first.status.success());
    let first_json: Value = serde_json::from_slice(&first.stdout).expect("first sync json");
    assert_eq!(first_json["newTransactionsCount"], 4);
    assert_eq!(first_json["needsContextCount"], 0);
    assert_eq!(first_json["newTransactions"].as_array().unwrap().len(), 4);
    let first_items = first_json["newTransactions"]
        .as_array()
        .expect("new transactions array");
    let first_tx = first_items
        .iter()
        .find(|item| item["transactionId"] == "pluggy-fixture-001")
        .expect("fixture tx in summary");
    assert_eq!(first_tx["txType"], "debit");
    assert_eq!(first_tx["categorySource"], "pluggy");
    assert_eq!(first_tx["dayOfWeek"], "friday");
    assert_eq!(first_tx["accountLabel"], "Primary Checking");
    assert_eq!(
        first_tx["metadataJson"]["pluggy_account_id"],
        "fixture-checking"
    );

    let second = envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31")
            .arg("--json-summary"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("second sync output");
    assert!(second.status.success());
    let second_json: Value = serde_json::from_slice(&second.stdout).expect("second sync json");
    assert_eq!(second_json["newTransactionsCount"], 0);
    assert_eq!(second_json["needsContextCount"], 0);
    assert_eq!(second_json["newTransactions"].as_array().unwrap().len(), 0);
}

#[test]
fn sync_json_summary_includes_pending_metadata_fields() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\n",
    );

    let fixture_path = temp.path().join("pluggy_fixture_uncategorized.json");
    write_file(
        &fixture_path,
        r#"{
  "accounts": [
    {
      "id": "fixture-checking",
      "itemId": "item-1",
      "name": "Primary Checking",
      "type": "checking",
      "status": "ACTIVE",
      "updatedAt": "2026-03-15T12:00:00.000Z"
    }
  ],
  "transactions": [
    {
      "id": "uncat-fixture-001",
      "accountId": "fixture-checking",
      "date": "2026-03-16",
      "description": "Compra sem categoria",
      "amount": -19.90,
      "type": "debit",
      "status": "POSTED",
      "createdAt": "2026-03-16T12:00:00.000Z",
      "updatedAt": "2026-03-16T12:00:00.000Z"
    }
  ]
}"#,
    );

    let output = envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31")
            .arg("--json-summary"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("pending sync output");
    assert!(output.status.success());

    let summary: Value = serde_json::from_slice(&output.stdout).expect("pending summary json");
    assert_eq!(summary["needsContextCount"], 1);
    let pending = summary["needsContext"]
        .as_array()
        .and_then(|rows| rows.first())
        .expect("pending row");
    assert_eq!(pending["transactionId"], "uncat-fixture-001");
    assert_eq!(pending["txType"], "debit");
    assert_eq!(pending["dayOfWeek"], "monday");
    assert_eq!(pending["accountLabel"], "Primary Checking");
    assert_eq!(
        pending["metadataJson"]["pluggy_account_id"],
        "fixture-checking"
    );
}

#[test]
fn sync_notify_summary_outputs_human_readable_message() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking" },
    { "id": "shared_credit", "pluggyAccountId": "fixture-credit" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\nshared_credit,secondary,credit,fintech,Shared Credit Card,fixture-credit,item-2,3,10\n",
    );

    let fixture_path = repo_root().join("examples/pluggy_fixture.json");

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31")
            .arg("--notify-summary"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("💰 *4 novas transações*"))
    .stdout(predicate::str::contains("Supermercado Angeloni"))
    .stdout(predicate::str::contains("Saldo em conta"))
    .stdout(predicate::str::contains("Despesa real nova"))
    .stdout(predicate::str::contains("Pluggy sincronizado"))
    .stdout(predicate::str::contains("*Top categorias*"));
}

#[test]
fn sync_rejects_multiple_summary_output_modes() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--json-summary")
            .arg("--notify-summary"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "Use apenas uma saída de resumo: --json-summary ou --notify-summary",
    ));
}

#[test]
fn sync_json_summary_survives_missing_effective_view() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking" },
    { "id": "shared_credit", "pluggyAccountId": "fixture-credit" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\nshared_credit,secondary,credit,fintech,Shared Credit Card,fixture-credit,item-2,3,10\n",
    );

    let db_path = data_dir.join("phai.local.db");
    {
        let conn = Connection::open(&db_path).expect("open db");
        conn.execute("DROP VIEW v_transactions_effective", [])
            .expect("drop effective view");
    }

    let fixture_path = temp.path().join("pluggy_fixture_uncategorized.json");
    write_file(
        &fixture_path,
        r#"{
  "accounts": [
    {
      "id": "fixture-checking",
      "itemId": "item-1",
      "name": "Primary Checking",
      "type": "checking",
      "status": "ACTIVE",
      "updatedAt": "2026-03-15T12:00:00.000Z"
    },
    {
      "id": "fixture-credit",
      "itemId": "item-2",
      "name": "Shared Credit Card",
      "type": "credit",
      "status": "ACTIVE",
      "updatedAt": "2026-03-15T12:00:00.000Z"
    }
  ],
  "transactions": [
    {
      "id": "uncat-fixture-001",
      "accountId": "fixture-checking",
      "date": "2026-03-16",
      "description": "Compra sem categoria",
      "amount": -19.90,
      "type": "debit",
      "status": "POSTED",
      "createdAt": "2026-03-16T12:00:00.000Z",
      "updatedAt": "2026-03-16T12:00:00.000Z"
    },
    {
      "id": "pluggy-fixture-aux-002",
      "accountId": "fixture-credit",
      "date": "2026-03-16",
      "description": "Compra categorizada",
      "amount": -11.50,
      "type": "debit",
      "status": "POSTED",
      "category": "alimentacao:restaurantes",
      "createdAt": "2026-03-16T12:00:00.000Z",
      "updatedAt": "2026-03-16T12:00:00.000Z"
    }
  ]
}"#,
    );
    let output = envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31")
            .arg("--json-summary"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("partial sync output");
    assert!(output.status.success());

    let summary: Value = serde_json::from_slice(&output.stdout).expect("partial summary json");
    assert_eq!(summary["summaryStatus"], "partial");
    assert_eq!(summary["newTransactionsCount"], 2);
    assert_eq!(summary["needsContextCount"], 1);
    assert_eq!(summary["needsContextReturnedCount"], 1);
    let pending = summary["needsContext"]
        .as_array()
        .and_then(|rows| rows.first())
        .expect("pending row");
    assert_eq!(pending["transactionId"], "uncat-fixture-001");
    assert_eq!(pending["description"], "Compra sem categoria");
    let warnings = summary["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0]
            .as_str()
            .unwrap_or_default()
            .contains("needs_context_fallback_sync_only"),
        "warning should explain degraded summary: {warnings:?}"
    );

    let conn = Connection::open(&db_path).expect("reopen db");
    let tx_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))
        .expect("count transactions");
    assert_eq!(tx_count, 2);
}

#[test]
fn sync_rebinds_fixture_account_by_item_id() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "stale-checking", "pluggyItemId": "item-1" },
    { "id": "shared_credit", "pluggyAccountId": "stale-credit", "pluggyItemId": "item-2" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,stale-checking,item-1,,\nshared_credit,secondary,credit,fintech,Shared Credit Card,stale-credit,item-2,3,10\n",
    );

    let fixture_path = repo_root().join("examples/pluggy_fixture.json");

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("- transactions: 4"));

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let rebound_checking: String = conn
        .query_row(
            "SELECT pluggy_account_id FROM accounts WHERE account_id = 'primary_checking'",
            [],
            |row| row.get(0),
        )
        .expect("checking rebind");
    let rebound_credit: String = conn
        .query_row(
            "SELECT pluggy_account_id FROM accounts WHERE account_id = 'shared_credit'",
            [],
            |row| row.get(0),
        )
        .expect("credit rebind");

    assert_eq!(rebound_checking, "fixture-checking");
    assert_eq!(rebound_credit, "fixture-credit");
}

#[test]
fn rule_and_context_inspection_commands_work() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("rule")
            .arg("upsert")
            .arg("--rule-id")
            .arg("mercado_rule")
            .arg("--body")
            .arg("if description contains mercado then category alimentacao:mercado"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("rule")
            .arg("upsert")
            .arg("--rule-id")
            .arg("disabled_rule")
            .arg("--body")
            .arg("if description contains farmacia then category saude:farmacia")
            .arg("--status")
            .arg("disabled"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let rule_list = envs(
        cargo_bin().arg("rule").arg("list").arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("rule list output");
    assert!(rule_list.status.success());
    let listed_rules: Value = serde_json::from_slice(&rule_list.stdout).expect("rule list json");
    let listed_rules = listed_rules.as_array().expect("listed rules array");
    assert_eq!(listed_rules.len(), 1);
    assert_eq!(listed_rules[0]["rule_id"], "mercado_rule");

    let inspected = envs(
        cargo_bin()
            .arg("rule")
            .arg("inspect")
            .arg("--rule-id")
            .arg("disabled_rule")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("rule inspect output");
    assert!(inspected.status.success());
    let inspected_rule: Value =
        serde_json::from_slice(&inspected.stdout).expect("rule inspect json");
    assert_eq!(inspected_rule["status"], "disabled");

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("tx")
            .arg("set-context")
            .arg("--transaction-id")
            .arg("pluggy-fixture-001")
            .arg("--context")
            .arg("compras-do-mes"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let contexts = envs(
        cargo_bin().arg("tx").arg("list-context").arg("--json"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("list context output");
    assert!(contexts.status.success());
    let contexts_json: Value = serde_json::from_slice(&contexts.stdout).expect("list context json");
    let contexts_rows = contexts_json.as_array().expect("contexts array");
    let context_row = contexts_rows
        .iter()
        .find(|row| row["transaction_id"] == "pluggy-fixture-001")
        .expect("context row");
    assert_eq!(context_row["context"], "compras-do-mes");
    assert_eq!(context_row["account_label"], "Primary Checking");
}

#[test]
fn sync_applies_db_rules_without_hardcoded_personal_logic() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("rule")
            .arg("upsert")
            .arg("--rule-id")
            .arg("bill_payment_rule")
            .arg("--body")
            .arg(
                "if description contains \"pagamento de fatura\" then category credit-card-payment",
            ),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Dates relative to today so the test stays stable over time.
    let today = Utc::now().date_naive();
    let tx_date = today - Duration::days(5);
    let tx_date_str = tx_date.format("%Y-%m-%d").to_string();
    let tx_ts_str = format!("{tx_date_str}T12:00:00.000Z");
    let sync_start = (today - Duration::days(30)).format("%Y-%m-%d").to_string();
    let month_str = tx_date.format("%Y-%m").to_string();
    let to_str = today.format("%Y-%m-%d").to_string();

    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        &format!(
            r#"{{
  "syncStartDate": "{sync_start}",
  "accounts": [
    {{ "id": "primary_checking", "pluggyAccountId": "fixture-checking" }}
  ]
}}"#
        ),
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\n",
    );

    let fixture_path = temp.path().join("pluggy_fixture_rule.json");
    write_file(
        &fixture_path,
        &format!(
            r#"{{
  "accounts": [
    {{
      "id": "fixture-checking",
      "itemId": "item-1",
      "name": "Primary Checking",
      "type": "checking",
      "status": "ACTIVE",
      "updatedAt": "{tx_ts_str}"
    }}
  ],
  "transactions": [
    {{
      "id": "rule-fixture-001",
      "accountId": "fixture-checking",
      "date": "{tx_date_str}",
      "description": "Pagamento de fatura Visa",
      "amount": -500.00,
      "status": "POSTED",
      "createdAt": "{tx_ts_str}",
      "updatedAt": "{tx_ts_str}"
    }}
  ]
}}"#
        ),
    );

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg(&to_str),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("- transactions: 1"));

    // The transaction is classified as `credit-card-payment` (internal category),
    // so the default human-friendly summary hides it from the grouped view.
    // Use --raw to assert the JSON-shaped data still includes it.
    envs(
        cargo_bin()
            .arg("report")
            .arg("daily-pulse")
            .arg("--days")
            .arg("120")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Pagamento de fatura Visa"))
    .stdout(predicate::str::contains("\"credit-card-payment\""));

    envs(
        cargo_bin()
            .arg("report")
            .arg("monthly-spend")
            .arg("--month")
            .arg(&month_str),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("- linhas: 0"));

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let category_id: String = conn
        .query_row(
            "SELECT category_id FROM transactions WHERE transaction_id = 'rule-fixture-001'",
            [],
            |row| row.get(0),
        )
        .expect("category from db rule");
    let category_source: String = conn
        .query_row(
            "SELECT category_source FROM transactions WHERE transaction_id = 'rule-fixture-001'",
            [],
            |row| row.get(0),
        )
        .expect("category source from db rule");

    assert_eq!(category_id, "credit-card-payment");
    assert_eq!(category_source, "rule");
}

#[test]
fn sync_fails_when_pluggy_item_id_diverges_between_sources() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking", "pluggyItemId": "item-from-config" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-from-csv,,\n",
    );

    let fixture_path = repo_root().join("examples/pluggy_fixture.json");

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("diverge"));
}

#[test]
fn sync_fails_when_two_bindings_resolve_to_same_pluggy_account() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Two bindings both targeting the same fixture account id: collision must be detected.
    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking" },
    { "id": "duplicate_checking", "pluggyAccountId": "fixture-checking" }
  ]
}"#,
    );

    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nprimary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\nduplicate_checking,primary,checking,fintech,Duplicate,fixture-checking,item-1,,\n",
    );

    let fixture_path = repo_root().join("examples/pluggy_fixture.json");

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture_path)
            .arg("--to")
            .arg("2026-03-31"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .failure()
    .stderr(predicate::str::contains("Colisão"));
}

#[test]
fn rule_list_rejects_invalid_status_value() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("rule")
            .arg("list")
            .arg("--status")
            .arg("nope"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .failure();
}

#[test]
fn tx_list_context_text_output_includes_transaction_id() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("tx")
            .arg("set-context")
            .arg("--transaction-id")
            .arg("pluggy-fixture-001")
            .arg("--context")
            .arg("compras-do-mes"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("tx").arg("list-context"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("pluggy-fixture-001"));
}

// ─── Part A: account_snapshots ──────────────────────────────────────────────

/// Syncing twice on the same day must produce exactly one snapshot per account
/// (idempotency via UNIQUE idempotency_key with INSERT OR IGNORE).
#[test]
fn account_snapshot_idempotent_same_day() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    // Sync twice — snapshots must not duplicate.
    seed_fixture_sync(&temp, &config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM account_snapshots", [], |row| {
            row.get(0)
        })
        .expect("count snapshots");
    // Two accounts, synced twice in the same run → still 2 (idempotent).
    assert_eq!(count, 2, "expected 1 snapshot per account, not duplicates");
}

/// Each snapshot must record the balance from the fixture at sync time.
#[test]
fn account_snapshot_records_balance() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");

    // Checking account fixture balance is 1342.44.
    let checking_balance: Option<String> = conn
        .query_row(
            "SELECT balance FROM account_snapshots WHERE account_id = 'primary_checking'",
            [],
            |row| row.get(0),
        )
        .expect("query checking balance");
    assert!(
        checking_balance.is_some(),
        "checking snapshot must have a balance"
    );
    let balance_str = checking_balance.unwrap();
    assert!(
        balance_str.contains("1342"),
        "balance should be ~1342.44, got {balance_str}"
    );
}

/// Snapshot source column must be 'pluggy'.
#[test]
fn account_snapshot_source_is_pluggy() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");

    let distinct_sources: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT source FROM account_snapshots")
            .expect("prepare");
        stmt.query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect")
    };
    assert_eq!(distinct_sources, vec!["pluggy"]);
}

// ─── Part B: tx find ────────────────────────────────────────────────────────

/// `tx find` with no matching description returns empty results without error.
#[test]
fn tx_find_no_match_returns_empty() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("tx")
            .arg("find")
            .arg("--query")
            .arg("xyznonexistent"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("- linhas: 0"));
}

/// `tx find` with a matching description returns that transaction.
#[test]
fn tx_find_single_match() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    // "Supermercado Angeloni" is in the fixture.
    envs(
        cargo_bin()
            .arg("tx")
            .arg("find")
            .arg("--query")
            .arg("Angeloni"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("pluggy-fixture-001"));
}

/// `tx find` is case-insensitive.
#[test]
fn tx_find_case_insensitive() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    // "angeloni" lower-case should still match "Supermercado Angeloni".
    envs(
        cargo_bin()
            .arg("tx")
            .arg("find")
            .arg("--query")
            .arg("angeloni"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("pluggy-fixture-001"));
}

/// `tx find --json` returns valid JSON array.
#[test]
fn tx_find_json_output() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let output = envs(
        cargo_bin()
            .arg("tx")
            .arg("find")
            .arg("--query")
            .arg("recebido")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value = serde_json::from_slice(&output).expect("valid JSON");
    assert!(parsed.is_array(), "should be a JSON array");
}

/// `tx find` with multiple matching descriptions returns all of them.
#[test]
fn tx_find_multiple_matches() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    // Add a manual transaction that also matches "compra".
    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("manual-compra-001")
            .arg("--account-id")
            .arg("primary_checking")
            .arg("--date")
            .arg("2026-03-20")
            .arg("--description")
            .arg("Loja de Compras Alpha")
            .arg("--amount=-50.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("manual-compra-002")
            .arg("--account-id")
            .arg("primary_checking")
            .arg("--date")
            .arg("2026-03-21")
            .arg("--description")
            .arg("Loja de Compras Beta")
            .arg("--amount=-75.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let output = envs(
        cargo_bin()
            .arg("tx")
            .arg("find")
            .arg("--query")
            .arg("compras")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value = serde_json::from_slice(&output).expect("valid JSON");
    let arr = parsed.as_array().expect("array");
    assert!(
        arr.len() >= 2,
        "expected at least 2 matches, got {}",
        arr.len()
    );
}

// ─── Part B: tx pending ─────────────────────────────────────────────────────

/// `tx pending` only returns transactions with context IS NULL.
#[test]
fn tx_pending_returns_only_no_context_txs() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    // Assign context to one transaction so it should not appear in pending.
    envs(
        cargo_bin()
            .arg("tx")
            .arg("set-context")
            .arg("--transaction-id")
            .arg("pluggy-fixture-001")
            .arg("--context")
            .arg("compras"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let output = envs(
        cargo_bin().arg("tx").arg("pending").arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value = serde_json::from_slice(&output).expect("valid JSON");
    let arr = parsed.as_array().expect("array");

    // pluggy-fixture-001 must NOT appear (it has context now).
    let has_001 = arr
        .iter()
        .any(|v| v["transaction_id"].as_str() == Some("pluggy-fixture-001"));
    assert!(
        !has_001,
        "fixture-001 should not appear in pending (has context)"
    );

    // All returned transactions must have null context.
    for tx in arr {
        let ctx = &tx["context"];
        assert!(
            ctx.is_null(),
            "pending tx should have null context, got {ctx}"
        );
    }
}

/// `tx pending` respects the --limit flag.
#[test]
fn tx_pending_respects_limit() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let output = envs(
        cargo_bin()
            .arg("tx")
            .arg("pending")
            .arg("--limit")
            .arg("1")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value = serde_json::from_slice(&output).expect("valid JSON");
    let arr = parsed.as_array().expect("array");
    assert!(arr.len() <= 1, "limit 1 should return at most 1 result");
}

// ─── Part B: tx set-context-by-desc ─────────────────────────────────────────

/// `--dry-run` must not write any changes to the database.
#[test]
fn tx_set_context_by_desc_dry_run_writes_nothing() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    // Dry-run: should succeed and show what would change.
    envs(
        cargo_bin()
            .arg("tx")
            .arg("set-context-by-desc")
            .arg("--query")
            .arg("Angeloni")
            .arg("--context")
            .arg("mercado-teste")
            .arg("--dry-run"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("pluggy-fixture-001"));

    // Verify nothing was written to DB.
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let ctx: Option<String> = conn
        .query_row(
            "SELECT context FROM transactions WHERE transaction_id = 'pluggy-fixture-001'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    assert!(ctx.is_none(), "dry-run must not write context, got {ctx:?}");
}

/// Real `set-context-by-desc` applies the context and emits audit events.
#[test]
fn tx_set_context_by_desc_real_applies_context() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("tx")
            .arg("set-context-by-desc")
            .arg("--query")
            .arg("angeloni")
            .arg("--context")
            .arg("mercado-mes"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("1 transação"));

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let ctx: Option<String> = conn
        .query_row(
            "SELECT context FROM transactions WHERE transaction_id = 'pluggy-fixture-001'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(ctx.as_deref(), Some("mercado-mes"));

    // Audit event must exist.
    let audit_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE entity_id = 'pluggy-fixture-001' AND action = 'set_context_by_desc'",
            [],
            |row| row.get(0),
        )
        .expect("audit count");
    assert!(audit_count >= 1, "audit event must be recorded");
}

/// Re-running `set-context-by-desc` with the same context is a no-op (idempotent result,
/// context stays the same, no crash).
#[test]
fn tx_set_context_by_desc_idempotent_rerun() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    for _ in 0..2 {
        envs(
            cargo_bin()
                .arg("tx")
                .arg("set-context-by-desc")
                .arg("--query")
                .arg("Angeloni")
                .arg("--context")
                .arg("mercado-idem"),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(db_path).expect("open db");
    let ctx: Option<String> = conn
        .query_row(
            "SELECT context FROM transactions WHERE transaction_id = 'pluggy-fixture-001'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(ctx.as_deref(), Some("mercado-idem"));
}

/// `set-context-by-desc --json` returns a JSON array of result objects.
#[test]
fn tx_set_context_by_desc_json_output() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let output = envs(
        cargo_bin()
            .arg("tx")
            .arg("set-context-by-desc")
            .arg("--query")
            .arg("Angeloni")
            .arg("--context")
            .arg("mercado-json")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value = serde_json::from_slice(&output).expect("valid JSON");
    let arr = parsed.as_array().expect("JSON array");
    assert!(!arr.is_empty(), "should return at least one result");
    assert_eq!(arr[0]["newContext"].as_str(), Some("mercado-json"));
    assert_eq!(arr[0]["transactionId"].as_str(), Some("pluggy-fixture-001"));
}

fn setup_local(temp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    (config_dir, data_dir)
}

#[test]
fn budget_upsert_creates_and_updates() {
    let temp = TempDir::new().expect("tempdir");
    let (config_dir, data_dir) = setup_local(&temp);

    // First upsert — creates
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-1")
            .arg("--amount")
            .arg("1000.00")
            .arg("--alert-threshold-pct")
            .arg("80"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("Budget salvo:"))
    .stdout(predicate::str::contains("test-cat-1"));

    // List and verify
    envs(
        cargo_bin().arg("budget").arg("list").arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("test-cat-1"))
    .stdout(predicate::str::contains("1000"));

    // Second upsert with same key — updates amount
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-1")
            .arg("--amount")
            .arg("1500.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // List should still show only one budget and the updated amount
    let output = envs(
        cargo_bin().arg("budget").arg("list").arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let parsed: Value = serde_json::from_slice(&output).expect("valid json");
    let arr = parsed.as_array().expect("array");
    assert_eq!(
        arr.len(),
        1,
        "should have exactly one budget after two upserts"
    );
    let amount = arr[0]["amount"].as_str().expect("amount str");
    assert!(amount.contains("1500"), "amount should be updated to 1500");
}

#[test]
fn budget_list_filters_by_month() {
    let temp = TempDir::new().expect("tempdir");
    let (config_dir, data_dir) = setup_local(&temp);

    // Insert a default budget (no month)
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-1")
            .arg("--amount")
            .arg("500.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Insert a month-specific budget
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-2")
            .arg("--month")
            .arg("2026-04")
            .arg("--amount")
            .arg("800.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // List without filter returns both
    let output = envs(
        cargo_bin().arg("budget").arg("list").arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let parsed: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(parsed.as_array().unwrap().len(), 2);

    // List filtered by month returns both (default + explicit for that month)
    let output = envs(
        cargo_bin()
            .arg("budget")
            .arg("list")
            .arg("--month")
            .arg("2026-04")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let parsed: Value = serde_json::from_slice(&output).expect("valid json");
    assert!(
        !parsed.as_array().unwrap().is_empty(),
        "at least the default budget should appear"
    );
}

#[test]
fn budget_status_shows_usage_pct() {
    let temp = TempDir::new().expect("tempdir");
    let (config_dir, data_dir) = setup_local(&temp);

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    // Insert a budget for alimentacao (the fixture has grocery spend)
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("alimentacao")
            .arg("--month")
            .arg("2026-03")
            .arg("--amount")
            .arg("300.00")
            .arg("--alert-threshold-pct")
            .arg("50"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin()
            .arg("report")
            .arg("budget-status")
            .arg("--month")
            .arg("2026-03"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("alimentacao"))
    .stdout(predicate::str::contains("Orçamentos"));
}

#[test]
fn budget_status_json_is_parseable() {
    let temp = TempDir::new().expect("tempdir");
    let (config_dir, data_dir) = setup_local(&temp);

    seed_fixture_sync(&temp, &config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-json")
            .arg("--month")
            .arg("2026-03")
            .arg("--amount")
            .arg("100.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let output = envs(
        cargo_bin()
            .arg("report")
            .arg("budget-status")
            .arg("--month")
            .arg("2026-03")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value =
        serde_json::from_slice(&output).expect("report budget-status --json must be valid JSON");
    let arr = parsed.as_array().expect("must be an array");
    assert!(
        arr.iter().any(|row| row["category_id"] == "test-cat-json"),
        "test-cat-json should appear in budget status"
    );
    // Check required fields exist
    let row = arr
        .iter()
        .find(|r| r["category_id"] == "test-cat-json")
        .unwrap();
    assert!(row.get("budget_amount").is_some());
    assert!(row.get("actual_amount").is_some());
    assert!(row.get("usage_pct").is_some());
    assert!(row.get("projected_pct").is_some());
    assert!(row.get("alert").is_some());
}

#[test]
fn budget_default_applies_to_every_month() {
    let temp = TempDir::new().expect("tempdir");
    let (config_dir, data_dir) = setup_local(&temp);

    // Default budget (no month_ref)
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-default")
            .arg("--amount")
            .arg("200.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Should appear in budget-status for any queried month
    let output = envs(
        cargo_bin()
            .arg("report")
            .arg("budget-status")
            .arg("--month")
            .arg("2025-01")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value = serde_json::from_slice(&output).expect("valid json");
    let arr = parsed.as_array().expect("array");
    assert!(
        arr.iter()
            .any(|row| row["category_id"] == "test-cat-default"),
        "default budget should appear for any queried month"
    );
}

#[test]
fn budget_explicit_month_takes_precedence_over_default() {
    let temp = TempDir::new().expect("tempdir");
    let (config_dir, data_dir) = setup_local(&temp);

    // Default budget
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-prec")
            .arg("--amount")
            .arg("200.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Month-specific override
    envs(
        cargo_bin()
            .arg("budget")
            .arg("upsert")
            .arg("--category-id")
            .arg("test-cat-prec")
            .arg("--month")
            .arg("2026-04")
            .arg("--amount")
            .arg("999.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let output = envs(
        cargo_bin()
            .arg("report")
            .arg("budget-status")
            .arg("--month")
            .arg("2026-04")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let parsed: Value = serde_json::from_slice(&output).expect("valid json");
    let arr = parsed.as_array().expect("array");
    let row = arr
        .iter()
        .find(|r| r["category_id"] == "test-cat-prec")
        .expect("test-cat-prec must appear");
    let budget = row["budget_amount"].as_str().unwrap_or_default();
    assert!(
        budget.contains("999"),
        "explicit month budget (999) should take precedence over default (200), got: {budget}"
    );
}

/// Regression test for installment detection with Pluggy-style data.
///
/// Real Pluggy responses often have a normalised `description` (no "X/Y") and
/// store the installment number in `creditCardMetadata.installmentNumber` /
/// `totalInstallments`, or in `descriptionRaw`.  The fix must surface those
/// transactions in both `report installments` and `cards --installments-only`.
#[test]
fn installments_detected_from_pluggy_credit_card_metadata() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    setup_local_auth_migrate(&config_dir, &data_dir);

    // Pluggy fixture: two credit-card installment transactions.
    //   tx-inst-meta: description is clean; installment info only in
    //                 creditCardMetadata (most common Pluggy pattern).
    //   tx-inst-raw:  description is clean; installment info in descriptionRaw.
    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-04-01",
  "accounts": [
    { "id": "cc-test", "pluggyAccountId": "pluggy-cc-test" }
  ]
}"#,
    );
    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\ncc-test,primary,credit,fintech,Test Card,pluggy-cc-test,item-9,3,10\n",
    );
    let fixture = temp.path().join("inst_fixture.json");
    write_file(
        &fixture,
        r#"{
  "accounts": [
    {
      "id": "pluggy-cc-test",
      "item_id": "item-9",
      "name": "Test Card",
      "type": "credit",
      "status": "ACTIVE",
      "balance": -500.00,
      "currency_code": "BRL",
      "updated_at": "2026-04-10T00:00:00Z"
    }
  ],
  "transactions": [
    {
      "id": "tx-inst-meta",
      "accountId": "pluggy-cc-test",
      "date": "2026-04-05",
      "description": "Notebook Pro",
      "amount": -450.00,
      "type": "debit",
      "status": "posted",
      "created_at": "2026-04-05T00:00:00Z",
      "updated_at": "2026-04-05T00:00:00Z",
      "creditCardMetadata": {
        "installmentNumber": 3,
        "totalInstallments": 10,
        "totalAmount": 4500.00
      }
    },
    {
      "id": "tx-inst-raw",
      "accountId": "pluggy-cc-test",
      "date": "2026-04-08",
      "description": "Amazon Marketplace",
      "amount": -200.00,
      "type": "debit",
      "status": "posted",
      "created_at": "2026-04-08T00:00:00Z",
      "updated_at": "2026-04-08T00:00:00Z",
      "descriptionRaw": "Amazon Marketplace 2/6"
    }
  ]
}"#,
    );

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture)
            .arg("--to")
            .arg("2026-04-30"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // report installments must find both active chains.
    let output = envs(
        cargo_bin().arg("report").arg("installments").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let chains: Value = serde_json::from_slice(&output).expect("valid json");
    let arr = chains.as_array().expect("chains array");
    assert!(
        !arr.is_empty(),
        "report installments deve retornar parcelas ativas, mas retornou vazio\nchains json: {}",
        String::from_utf8_lossy(&output)
    );

    // creditCardMetadata chain: current=3, total=10, remaining=7.
    // The JSON uses camelCase keys (baseDescription, accountId, …).
    let meta_chain = arr
        .iter()
        .find(|c| {
            c["baseDescription"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("notebook")
        })
        .unwrap_or_else(|| {
            panic!(
                "cadeia 'Notebook Pro' não encontrada\ncadeias encontradas: {}",
                serde_json::to_string_pretty(arr).unwrap()
            )
        });
    assert_eq!(meta_chain["total"], 10, "total deve ser 10");
    assert_eq!(meta_chain["current"], 3, "current deve ser 3");
    assert!(
        meta_chain["remaining"].as_u64().unwrap_or(0) > 0,
        "remaining deve ser > 0"
    );

    // descriptionRaw chain: current=2, total=6, remaining=4.
    let raw_chain = arr
        .iter()
        .find(|c| {
            c["baseDescription"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("amazon")
        })
        .expect("cadeia 'Amazon Marketplace' não encontrada");
    assert_eq!(raw_chain["total"], 6, "total deve ser 6");
    assert_eq!(raw_chain["current"], 2, "current deve ser 2");

    // cards --installments-only must surface both transactions.
    let card_output = envs(
        cargo_bin()
            .arg("report")
            .arg("cards")
            .arg("--month")
            .arg("2026-04")
            .arg("--installments-only")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let card_json: Value = serde_json::from_slice(&card_output).expect("valid json");
    let txs = card_json["transactions"]
        .as_array()
        .expect("transactions array");
    assert_eq!(
        txs.len(),
        2,
        "cards --installments-only deve retornar as 2 transações parceladas, obteve {}",
        txs.len()
    );
}

/// Regression test: `cards --installments-only` must preserve the underlying
/// bill's payment status. The filter narrows the *displayed* transactions
/// (totals/counts/category breakdown), but should NOT shrink the total used
/// for matching against checking-account bill payments — otherwise a bill
/// that was actually paid in full would be reported as "em aberto" /
/// "ATRASADO" simply because the installment-only subtotal doesn't match
/// any payment line.
#[test]
fn cards_installments_only_preserves_paid_status() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    setup_local_auth_migrate(&config_dir, &data_dir);

    // closing_day=3 → April transactions on/after the 3rd belong to the
    // bill that closes 2026-05-03; close_date < today (2026-05-19+) so the
    // bill is treated as closed and payment matching kicks in.
    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-03-15",
  "accounts": [
    { "id": "chk", "pluggyAccountId": "pluggy-chk" },
    { "id": "cc", "pluggyAccountId": "pluggy-cc" }
  ]
}"#,
    );
    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\nchk,primary,checking,fintech,Checking,pluggy-chk,item-chk,,\ncc,primary,credit,fintech,Card,pluggy-cc,item-cc,3,10\n",
    );
    let fixture = temp.path().join("paid_with_installments.json");
    write_file(
        &fixture,
        r#"{
  "accounts": [
    {
      "id": "pluggy-chk",
      "item_id": "item-chk",
      "name": "Checking",
      "type": "checking",
      "status": "ACTIVE",
      "balance": 5000.00,
      "currency_code": "BRL",
      "updated_at": "2026-05-10T00:00:00Z"
    },
    {
      "id": "pluggy-cc",
      "item_id": "item-cc",
      "name": "Card",
      "type": "credit",
      "status": "ACTIVE",
      "balance": -1000.00,
      "currency_code": "BRL",
      "updated_at": "2026-05-10T00:00:00Z"
    }
  ],
  "transactions": [
    {
      "id": "cc-inst-1",
      "accountId": "pluggy-cc",
      "date": "2026-04-05",
      "description": "Notebook",
      "amount": -200.00,
      "type": "debit",
      "status": "posted",
      "created_at": "2026-04-05T00:00:00Z",
      "updated_at": "2026-04-05T00:00:00Z",
      "creditCardMetadata": { "installmentNumber": 1, "totalInstallments": 5, "totalAmount": 1000.00 }
    },
    {
      "id": "cc-inst-2",
      "accountId": "pluggy-cc",
      "date": "2026-04-08",
      "description": "Geladeira",
      "amount": -200.00,
      "type": "debit",
      "status": "posted",
      "created_at": "2026-04-08T00:00:00Z",
      "updated_at": "2026-04-08T00:00:00Z",
      "creditCardMetadata": { "installmentNumber": 2, "totalInstallments": 10, "totalAmount": 2000.00 }
    },
    {
      "id": "cc-plain",
      "accountId": "pluggy-cc",
      "date": "2026-04-15",
      "description": "Supermercado",
      "amount": -600.00,
      "type": "debit",
      "status": "posted",
      "created_at": "2026-04-15T00:00:00Z",
      "updated_at": "2026-04-15T00:00:00Z"
    },
    {
      "id": "pay-1",
      "accountId": "pluggy-chk",
      "date": "2026-05-10",
      "description": "Pagamento de fatura cartao",
      "amount": -1000.00,
      "type": "debit",
      "status": "posted",
      "created_at": "2026-05-10T00:00:00Z",
      "updated_at": "2026-05-10T00:00:00Z"
    }
  ]
}"#,
    );

    envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture)
            .arg("--to")
            .arg("2026-05-15"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Pluggy sync rebuilds account metadata from the API payload and only
    // *preserves* an existing billing_closing_day across re-syncs — it does
    // not import the value from the accounts CSV on first sync. Inject it
    // directly so the bill clustering uses the synthetic close date that
    // makes target_month=2026-05 resolve to a closed bill with a payment.
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE accounts SET metadata_json = json_set(metadata_json, '$.billing_closing_day', '3') WHERE account_id = 'cc'",
        [],
    )
    .expect("inject billing_closing_day");
    drop(conn);

    // Baseline: without the filter, the bill should be flagged as paid.
    let baseline = envs(
        cargo_bin()
            .arg("report")
            .arg("cards")
            .arg("--month")
            .arg("2026-05")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let baseline_json: Value = serde_json::from_slice(&baseline).expect("valid json");
    let acct = &baseline_json["summary"]["accounts"][0];
    assert_eq!(
        acct["status"]["state"],
        "paid",
        "baseline (sem filtro) deve mostrar a fatura como paga, summary: {}",
        serde_json::to_string_pretty(&baseline_json["summary"]).unwrap()
    );

    // With --installments-only: the displayed totals shrink, but the
    // status must still come from the full bill — so "paid" not "open".
    let filtered = envs(
        cargo_bin()
            .arg("report")
            .arg("cards")
            .arg("--month")
            .arg("2026-05")
            .arg("--installments-only")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let filtered_json: Value = serde_json::from_slice(&filtered).expect("valid json");
    let acct = &filtered_json["summary"]["accounts"][0];
    assert_eq!(
        acct["status"]["state"], "paid",
        "fatura paga não deve aparecer como 'open'/'overdue' só porque o filtro reduziu o total exibido, summary: {}",
        serde_json::to_string_pretty(&filtered_json["summary"]).unwrap()
    );
    assert_eq!(
        acct["transaction_count"], 2,
        "transaction_count deve refletir só as parceladas, obteve {}",
        acct["transaction_count"]
    );
    let txs = filtered_json["transactions"]
        .as_array()
        .expect("transactions array");
    assert_eq!(
        txs.len(),
        2,
        "transactions deve conter só as 2 parceladas, obteve {}",
        txs.len()
    );
}

#[test]
fn amount_cents_exact_integer_sum_regression() {
    // ADR-0003: verify that amount_cents produces exact integer sums
    // and that views no longer exhibit floating-point drift.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");

    envs(
        cargo_bin()
            .arg("auth")
            .arg("setup")
            .arg("--backend")
            .arg("local")
            .arg("--actor-id")
            .arg("test-actor"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    envs(
        cargo_bin().arg("admin").arg("migrate"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Cash-basis cashflow only sees checking accounts. Seed one so the
    // manual transactions below are picked up by the new aggregator.
    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("prec_checking")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("checking")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Precision Checking"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Insert transactions with edge-case cent values that would expose
    // floating-point drift under CAST(amount AS REAL) aggregation.
    let txs = [
        ("prec-001", "2026-05-01", "Um centavo", "-0.01"),
        ("prec-002", "2026-05-02", "Dois centavos", "-0.02"),
        ("prec-003", "2026-05-03", "Tres centavos", "-0.03"),
        ("prec-004", "2026-05-04", "Mil duzentos", "-1234.56"),
        ("prec-005", "2026-05-05", "Salario", "5000.00"),
        ("prec-006", "2026-05-06", "Cashback troco", "1.99"),
    ];

    for (id, date, desc, amount) in &txs {
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(id)
                .arg("--account-id")
                .arg("prec_checking")
                .arg("--date")
                .arg(date)
                .arg("--description")
                .arg(desc)
                .arg(format!("--amount={amount}")),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }

    // Verify amount_cents column directly via SQLite.
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");

    // Every row must have a non-NULL amount_cents that matches ROUND(amount * 100).
    let mismatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM transactions
             WHERE amount_cents IS NULL
                OR amount_cents != CAST(ROUND(CAST(amount AS REAL) * 100) AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .expect("count mismatches");
    assert_eq!(mismatch_count, 0, "amount_cents mismatch detected");

    // Verify exact aggregation via `report cashflow --month 2026-05 --json`.
    let cashflow_json = envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-05")
            .arg("--json"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let cf: Value = serde_json::from_slice(&cashflow_json).expect("valid cashflow json");
    assert_eq!(cf["month_ref"].as_str(), Some("2026-05"));

    // Inflows: salary 5000.00 + uncategorized credit 1.99 = 5001.99.
    // JSON serialization of Decimal uses exact string representation.
    assert_eq!(
        cf["inflows"].as_str().expect("inflows string"),
        "5001.99",
        "inflows deve ser exato, sem drift de ponto flutuante"
    );

    // Outflows: 0.01 + 0.02 + 0.03 + 1234.56 = 1234.62 (exact).
    assert_eq!(
        cf["outflows"].as_str().expect("outflows string"),
        "1234.62",
        "outflows deve ser exato, sem drift de ponto flutuante"
    );

    // Net = inflows - outflows = 5001.99 - 1234.62 = 3767.37
    assert_eq!(
        cf["net"].as_str().expect("net string"),
        "3767.37",
        "net deve ser exato"
    );

    // Also verify the v_cashflow view via direct SQLite query as a
    // belt-and-suspenders integer-cent check at the storage layer.
    let (view_income, view_expenses, view_net): (String, String, String) = conn
        .query_row(
            "SELECT income, expenses, net FROM v_cashflow WHERE month_ref = '2026-05'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("v_cashflow query");

    assert_eq!(view_income, "5001.99", "v_cashflow income exato");
    assert_eq!(view_expenses, "1234.62", "v_cashflow expenses exato");
    assert_eq!(view_net, "3767.37", "v_cashflow net exato");
}

#[test]
fn cashflow_is_cash_basis_on_checking_accounts() {
    // The new `report cashflow` is a single-month cash-basis summary
    // restricted to checking accounts. Verify the filtering rules:
    //   - credit-card swipes are EXCLUDED (they live on credit accounts)
    //   - credit-card-payment from checking IS counted as an outflow
    //   - transfer-internal between own accounts is EXCLUDED
    //   - opening / closing balance is anchored on the latest snapshot
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    // Two accounts: one checking, one credit.
    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("checking_a")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("checking")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Checking A"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("credit_a")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Credit A"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Transactions in 2026-05:
    //   +5000.00 salary on checking         → inflow
    //   - 100.00 grocery on checking        → outflow
    //   - 800.00 credit-card-payment        → outflow (cash event)
    //   - 250.00 swipe on credit_a          → EXCLUDED (not a cash event)
    //   - 200.00 transfer-internal          → EXCLUDED
    struct TxCase {
        id: &'static str,
        account: &'static str,
        date: &'static str,
        desc: &'static str,
        amount: &'static str,
        category: Option<&'static str>,
    }
    let tx_cases = [
        TxCase {
            id: "cb-salary",
            account: "checking_a",
            date: "2026-05-10",
            desc: "Salario",
            amount: "5000.00",
            category: None,
        },
        TxCase {
            id: "cb-market",
            account: "checking_a",
            date: "2026-05-12",
            desc: "Supermercado",
            amount: "-100.00",
            category: None,
        },
        TxCase {
            id: "cb-cardpay",
            account: "checking_a",
            date: "2026-05-15",
            desc: "Pagamento fatura",
            amount: "-800.00",
            category: Some("credit-card-payment"),
        },
        TxCase {
            id: "cb-swipe",
            account: "credit_a",
            date: "2026-05-08",
            desc: "Posto",
            amount: "-250.00",
            category: None,
        },
        TxCase {
            id: "cb-transfer",
            account: "checking_a",
            date: "2026-05-20",
            desc: "Transferencia interna",
            amount: "-200.00",
            category: Some("transfer-internal"),
        },
    ];
    for case in &tx_cases {
        let mut cmd = cargo_bin();
        cmd.arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg(case.id)
            .arg("--account-id")
            .arg(case.account)
            .arg("--date")
            .arg(case.date)
            .arg("--description")
            .arg(case.desc)
            .arg(format!("--amount={}", case.amount));
        if let Some(cat) = case.category {
            cmd.arg("--category").arg(cat);
        }
        envs(&mut cmd, &config_dir, &data_dir).assert().success();
    }

    // Seed a snapshot for the checking account at 2026-04-30 (last day of
    // previous month) so opening_balance can be anchored. Closing then
    // rolls forward by sum(amount) on checking up to today.
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute(
        "INSERT INTO account_snapshots (
            snapshot_id, account_id, snapshot_date, balance, credit_limit,
            currency_code, source, actor_id, idempotency_key, metadata_json, created_at
         ) VALUES (
            'snap-checking-a', 'checking_a', '2026-04-30', '10000.00', NULL,
            'BRL', 'manual', 'test-actor', 'snap-checking-a-key',
            '{}', '2026-04-30T12:00:00Z'
         )",
        [],
    )
    .expect("insert snapshot");
    drop(conn);

    // ── Happy path: anchored balances, cash-basis filtering ──
    let raw = envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-05")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let cf: Value = serde_json::from_slice(&raw).expect("valid cashflow json");

    assert_eq!(cf["month_ref"].as_str(), Some("2026-05"));
    assert_eq!(
        cf["inflows"].as_str(),
        Some("5000.00"),
        "inflows = salário (swipe de cartão e transfer-internal não contam)"
    );
    assert_eq!(
        cf["outflows"].as_str(),
        Some("900.00"),
        "outflows = mercado + pagamento da fatura; swipe de cartão e transfer-internal não contam"
    );
    assert_eq!(cf["net"].as_str(), Some("4100.00"));
    assert_eq!(
        cf["opening_balance"].as_str(),
        Some("10000.00"),
        "opening = snapshot em 2026-04-30 com delta zero (nenhuma tx anterior)"
    );
    // Closing = snapshot + sum(checking_a transactions in May up to today).
    // May checking_a deltas: +5000 -100 -800 -200 = +3900. Closing = 13900.00.
    assert_eq!(
        cf["closing_balance"].as_str(),
        Some("13900.00"),
        "closing = snapshot + delta de todas as transações na conta corrente (inclusive transfer-internal — só são excluídas do cashflow agregado)"
    );
    assert_eq!(cf["snapshot_complete"], Value::Bool(true));
    assert_eq!(cf["accounts_considered"], Value::Number(1.into()));

    // ── Edge case: drop the snapshot → balances go null ──
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute("DELETE FROM account_snapshots", [])
        .expect("drop snapshots");
    drop(conn);

    let raw2 = envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow")
            .arg("--month")
            .arg("2026-05")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let cf2: Value = serde_json::from_slice(&raw2).expect("valid cashflow json");
    assert_eq!(cf2["opening_balance"], Value::Null);
    assert_eq!(cf2["closing_balance"], Value::Null);
    assert_eq!(cf2["snapshot_complete"], Value::Bool(false));
    // Inflows/outflows still computable from transaction data alone.
    assert_eq!(cf2["inflows"].as_str(), Some("5000.00"));
    assert_eq!(cf2["outflows"].as_str(), Some("900.00"));
}

#[test]
fn cashflow_chart_emits_svg_and_optional_sparkline() {
    // End-to-end check for `report cashflow-chart`: SVG is written to disk
    // with the expected structural markers, --text emits a sparkline to
    // stdout, and --forecast adds dashed overlays + ⇢ marker rows.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("chk")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("checking")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Checking"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // A couple of transactions in the current calendar month — only purpose
    // is to exercise the report end-to-end. Use the very recent past to
    // stay within the default 6-month window regardless of when the test
    // is run.
    let today = chrono::Local::now().date_naive();
    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("chart-in")
            .arg("--account-id")
            .arg("chk")
            .arg("--date")
            .arg(today.format("%Y-%m-%d").to_string())
            .arg("--description")
            .arg("Entrada")
            .arg("--amount=2500.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("chart-out")
            .arg("--account-id")
            .arg("chk")
            .arg("--date")
            .arg(today.format("%Y-%m-%d").to_string())
            .arg("--description")
            .arg("Saida")
            .arg("--amount=-1200.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Seed a snapshot for the leftmost month so the saldo line gets an
    // anchor. We just put it at "today minus 200 days" — plenty of head
    // room for the default 6-month window.
    let snap_date = today - chrono::Duration::days(200);
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute(
        "INSERT INTO account_snapshots (
            snapshot_id, account_id, snapshot_date, balance, credit_limit,
            currency_code, source, actor_id, idempotency_key, metadata_json, created_at
         ) VALUES (
            'snap-chk', 'chk', ?1, '8000.00', NULL,
            'BRL', 'manual', 'test-actor', 'snap-chk-key',
            '{}', '2025-01-01T12:00:00Z'
         )",
        [snap_date.format("%Y-%m-%d").to_string()],
    )
    .expect("insert snapshot");
    drop(conn);

    // ── Plain run: SVG written, no text on stdout ──
    let svg_path = temp.path().join("out.svg");
    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow-chart")
            .arg("--months")
            .arg("4")
            .arg("--output")
            .arg(&svg_path),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    let svg = fs::read_to_string(&svg_path).expect("svg file exists");
    assert!(svg.starts_with("<svg"), "SVG should open with <svg");
    assert!(svg.ends_with("</svg>"), "SVG should close with </svg>");
    assert!(svg.contains("Evolução de caixa"), "expected title");
    assert!(svg.contains("Saldo realizado"), "expected legend");
    assert!(
        svg.contains("Entradas realizadas"),
        "expected entradas legend"
    );
    assert!(svg.contains("Saídas realizadas"), "expected saidas legend");
    // No forecast → no hatched overlay or projected line
    assert!(!svg.contains("url(#hatch-in)"), "no --forecast → no hatch");
    assert!(
        !svg.contains("Saldo projetado"),
        "no --forecast → no projection"
    );

    // ── --text mode: sparkline shows up on stdout ──
    let svg_path2 = temp.path().join("out2.svg");
    let stdout = envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow-chart")
            .arg("--months")
            .arg("4")
            .arg("--output")
            .arg(&svg_path2)
            .arg("--text"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let stdout = String::from_utf8(stdout).expect("utf8 stdout");
    assert!(stdout.contains("SVG gravado"));
    assert!(stdout.contains("Evolução de caixa"));
    assert!(stdout.contains("Saldo"));
    assert!(stdout.contains("Entradas"));
    assert!(stdout.contains("Saídas"));

    // ── --forecast: dashed overlay shows up in SVG ──
    // Seed an active forecast in the current month.
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute(
        "INSERT INTO forecast (
            forecast_id, due_date, description, amount, category_id,
            account_id, status, recurrence, actor_id, idempotency_key,
            metadata_json, created_at, updated_at
         ) VALUES (
            'fc-in', ?1, 'Salario previsto', '3000.00', NULL,
            'chk', 'ativo', NULL, 'test-actor', 'fc-in-key',
            '{}', '2025-01-01T12:00:00Z', '2025-01-01T12:00:00Z'
         )",
        [today.format("%Y-%m-%d").to_string()],
    )
    .expect("insert forecast");
    conn.execute(
        "INSERT INTO forecast (
            forecast_id, due_date, description, amount, category_id,
            account_id, status, recurrence, actor_id, idempotency_key,
            metadata_json, created_at, updated_at
         ) VALUES (
            'fc-out', ?1, 'Aluguel previsto', '-1500.00', NULL,
            'chk', 'ativo', NULL, 'test-actor', 'fc-out-key',
            '{}', '2025-01-01T12:00:00Z', '2025-01-01T12:00:00Z'
         )",
        [today.format("%Y-%m-%d").to_string()],
    )
    .expect("insert forecast");
    drop(conn);

    let svg_path3 = temp.path().join("out3.svg");
    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow-chart")
            .arg("--months")
            .arg("4")
            .arg("--output")
            .arg(&svg_path3)
            .arg("--forecast"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    let svg3 = fs::read_to_string(&svg_path3).expect("svg file exists");
    // Hatched forecast extension stacked on top of inflow/outflow bars.
    assert!(
        svg3.contains("url(#hatch-in)"),
        "--forecast → hatched extension on entradas"
    );
    assert!(
        svg3.contains("url(#hatch-out)"),
        "--forecast → hatched extension on saídas"
    );
    // Projected (dashed) saldo line.
    assert!(svg3.contains("stroke-dasharray=\"6 4\""));
    // Legend gains the three forecast entries.
    assert!(svg3.contains("Entradas previstas"));
    assert!(svg3.contains("Saídas previstas"));
    assert!(svg3.contains("Saldo projetado"));

    // ── --months-ahead requires --forecast ──
    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow-chart")
            .arg("--months-ahead")
            .arg("3"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .failure();

    // ── --months-ahead with --forecast extends the window with future months ──
    let svg_path4 = temp.path().join("out4.svg");
    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow-chart")
            .arg("--months")
            .arg("3")
            .arg("--months-ahead")
            .arg("3")
            .arg("--output")
            .arg(&svg_path4)
            .arg("--forecast"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    let svg4 = fs::read_to_string(&svg_path4).expect("svg file exists");
    // Future band callout
    assert!(svg4.contains(">projetado<"));
    // Subtitle shows realized + projected split
    assert!(svg4.contains("realizados +"));

    // ── --no-svg without --text should fail ──
    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow-chart")
            .arg("--no-svg"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .failure();
}

#[test]
fn cashflow_chart_is_accrual_includes_card_swipes() {
    // ADR-0024: the cashflow chart sources its bars from v_cashflow (accrual,
    // all reportable accounts) — unlike the cash-basis `report cashflow`,
    // credit-card swipes DO count here, in the month they posted. A store
    // with only a credit card (no checking account, no snapshots) must still
    // render non-zero bars. This is the regression guard for the bug where
    // the chart read the checking-only path and rendered all zeros.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    // Only a credit card — no checking account, no snapshots.
    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("card_x")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Card X"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // A single card swipe of -1000.00 in the current month.
    let today = chrono::Local::now().date_naive();
    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("swipe-x")
            .arg("--account-id")
            .arg("card_x")
            .arg("--date")
            .arg(today.format("%Y-%m-%d").to_string())
            .arg("--description")
            .arg("Compra cartão")
            .arg("--amount=-1000.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let svg_path = temp.path().join("accrual.svg");
    envs(
        cargo_bin()
            .arg("report")
            .arg("cashflow-chart")
            .arg("--months")
            .arg("2")
            .arg("--output")
            .arg(&svg_path),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    let svg = fs::read_to_string(&svg_path).expect("svg file exists");
    // The card swipe shows up as a saída bar labelled "1.0k" (brl_k of 1000).
    // Under the old checking-only cash-basis path this would be absent.
    assert!(
        svg.contains("1.0k"),
        "card swipe must appear as a saída bar (accrual), got SVG without it"
    );
}

#[test]
fn forecast_refresh_installments_materialises_remaining_parcelas() {
    // End-to-end: seed a credit account with three parcelas of a 12-month
    // chain (3/12), run `fin forecast refresh-installments`, then assert
    // that one template + nine forecasts (one per remaining parcela) got
    // upserted with stable IDs (running twice must be a no-op).
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("card_a")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Card A"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Three parcelas of a 12-month chain — Pluggy descriptions carry the
    // "X/N" marker so the detector picks them up.
    for (n, day) in [(1u32, 5u32), (2, 5), (3, 5)] {
        let date = format!("2026-0{n}-{day:02}");
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(format!("parcel-{n:02}"))
                .arg("--account-id")
                .arg("card_a")
                .arg("--date")
                .arg(&date)
                .arg("--description")
                .arg(format!("Magazine Luiza Notebook XYZ {n}/12"))
                .arg("--amount=-300.00"),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }

    // First run — produces 1 template + 9 forecasts.
    let stdout = envs(
        cargo_bin()
            .arg("forecast")
            .arg("refresh-installments")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let report: Value = serde_json::from_slice(&stdout).expect("valid refresh json");
    assert_eq!(report["chains_seen"].as_u64(), Some(1));
    assert_eq!(report["chains_active"].as_u64(), Some(1));
    assert_eq!(report["templates_upserted"].as_u64(), Some(1));
    assert_eq!(report["forecasts_upserted"].as_u64(), Some(9));

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let tpl_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast_template WHERE kind = 'installment'",
            [],
            |r| r.get(0),
        )
        .expect("count templates");
    assert_eq!(tpl_count, 1);
    let fc_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count forecasts");
    assert_eq!(fc_count, 9, "9 remaining parcelas (4..=12)");
    drop(conn);

    // Second run — idempotent: same counts, no duplicates.
    envs(
        cargo_bin()
            .arg("forecast")
            .arg("refresh-installments")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    let conn = Connection::open(&db_path).expect("open db");
    let tpl_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM forecast_template", [], |r| r.get(0))
        .expect("count templates");
    assert_eq!(tpl_count, 1, "second run must not duplicate templates");
    let fc_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count forecasts");
    assert_eq!(fc_count, 9, "second run must not duplicate forecasts");
}

#[test]
fn forecast_refresh_collapses_duplicate_template_from_id_drift() {
    // Regression for the idempotency incident (ADR-0022): when a template's
    // derived id drifts (e.g. the hashing changed across a release), the same
    // commitment forks into two active templates and `refresh` re-materialises
    // both forever. `forecast refresh` must self-heal — collapse the duplicate
    // to `descartado` and deactivate its forecasts — keeping one active row.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("card_a")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Card A"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    for (n, day) in [(1u32, 5u32), (2, 5), (3, 5)] {
        let date = format!("2026-0{n}-{day:02}");
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(format!("parcel-{n:02}"))
                .arg("--account-id")
                .arg("card_a")
                .arg("--date")
                .arg(&date)
                .arg("--description")
                .arg(format!("Magazine Luiza Notebook XYZ {n}/12"))
                .arg("--amount=-300.00"),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }

    // Establish the canonical installment template + its 9 forecasts.
    envs(
        cargo_bin()
            .arg("forecast")
            .arg("refresh-installments")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Simulate id drift: clone the template under a *new* id with the *same*
    // natural identity (account + base description + total) and a later
    // created_at, plus two active forecasts hanging off it.
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let original_id: String = conn
        .query_row(
            "SELECT template_id FROM forecast_template WHERE kind = 'installment'",
            [],
            |r| r.get(0),
        )
        .expect("original template id");
    assert!(original_id.starts_with("installment-"));
    conn.execute_batch(&format!(
        "INSERT INTO forecast_template
           (template_id, kind, description, merchant_pattern, category_id, account_id, amount,
            amount_lower, amount_upper, cadence, next_due_day, start_date, end_date, remaining_count,
            source, confidence, status, metadata_json, actor_id, idempotency_key, created_at, updated_at)
         SELECT 'installment-drifted-dupe', kind, description, merchant_pattern, category_id, account_id,
            amount, amount_lower, amount_upper, cadence, next_due_day, start_date, end_date,
            remaining_count, source, confidence, 'ativo', metadata_json, actor_id,
            'forecast-template-installment-drifted-dupe',
            datetime(created_at, '+1 hour'), datetime(updated_at, '+1 hour')
         FROM forecast_template WHERE template_id = '{original_id}';

         INSERT INTO forecast
           (forecast_id, due_date, description, amount, category_id, account_id, status, recurrence,
            actor_id, idempotency_key, metadata_json, created_at, updated_at, template_id,
            realized_transaction_id, realized_at)
         SELECT 'dupe-' || forecast_id, due_date, description, amount, category_id, account_id, 'ativo',
            recurrence, actor_id, 'idem-dupe-' || forecast_id, metadata_json, created_at, updated_at,
            'installment-drifted-dupe', realized_transaction_id, realized_at
         FROM forecast WHERE template_id = '{original_id}' LIMIT 2;"
    ))
    .expect("seed drifted duplicate");

    let installment_templates: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast_template WHERE kind = 'installment'",
            [],
            |r| r.get(0),
        )
        .expect("count installment templates");
    assert_eq!(installment_templates, 2, "duplicate seeded");
    drop(conn);

    // Full refresh must self-heal.
    let stdout = envs(
        cargo_bin().arg("forecast").arg("refresh").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let report: Value = serde_json::from_slice(&stdout).expect("valid refresh json");
    assert!(
        report["templates_deduped"].as_u64().unwrap_or(0) >= 1,
        "refresh report must record the dedup"
    );

    let conn = Connection::open(&db_path).expect("open db");
    let original_status: String = conn
        .query_row(
            "SELECT status FROM forecast_template WHERE template_id = ?1",
            [&original_id],
            |r| r.get(0),
        )
        .expect("original status");
    assert_eq!(original_status, "ativo", "canonical (oldest) stays active");
    let dupe_status: String = conn
        .query_row(
            "SELECT status FROM forecast_template WHERE template_id = 'installment-drifted-dupe'",
            [],
            |r| r.get(0),
        )
        .expect("dupe status");
    assert_eq!(dupe_status, "descartado", "younger duplicate demoted");
    let active_dupe_forecasts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id = 'installment-drifted-dupe' \
             AND status = 'ativo'",
            [],
            |r| r.get(0),
        )
        .expect("count active dupe forecasts");
    assert_eq!(active_dupe_forecasts, 0, "dupe forecasts deactivated");
    let active_installment_templates: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast_template WHERE kind = 'installment' AND status = 'ativo'",
            [],
            |r| r.get(0),
        )
        .expect("count active installment templates");
    assert_eq!(
        active_installment_templates, 1,
        "exactly one active template per identity"
    );

    // Running again is a no-op for the dedup pass.
    let stdout2 = envs(
        cargo_bin().arg("forecast").arg("refresh").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let report2: Value = serde_json::from_slice(&stdout2).expect("valid refresh json");
    assert_eq!(
        report2["templates_deduped"].as_u64(),
        Some(0),
        "second refresh finds nothing to dedup"
    );
}

#[test]
fn forecast_suggest_accept_dismiss_subscription_round_trip() {
    // Layer 2/3 (ADR-0016): seed three months of a stable monthly debit
    // (Spotify-like), expect the detector to flag it, persist as
    // 'proposto', then exercise accept → materialise + dismiss → status
    // change.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("chk_test")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("checking")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Checking"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // 3 months of a stable monthly outflow (R$ 21.90 ± tiny rounding noise).
    let today = chrono::Local::now().date_naive();
    let amounts = ["-21.90", "-21.90", "-22.10"];
    for (i, amount) in amounts.iter().enumerate() {
        // Calendar-month steps (not *30 days) so each offset lands in a
        // distinct YYYY-MM regardless of today's date — keeps the test
        // deterministic at month boundaries.
        let date = today
            .checked_sub_months(chrono::Months::new(i as u32 + 1))
            .expect("date in range");
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(format!("sub-{i}"))
                .arg("--account-id")
                .arg("chk_test")
                .arg("--date")
                .arg(date.format("%Y-%m-%d").to_string())
                .arg("--description")
                .arg("Streaming Service Monthly")
                .arg(format!("--amount={amount}")),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }

    // First suggest run — produces one 'proposto' template.
    let stdout = envs(
        cargo_bin()
            .arg("forecast")
            .arg("suggest")
            .arg("--raw")
            .arg("--lookback-months")
            .arg("6"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let templates: Value = serde_json::from_slice(&stdout).expect("valid suggest json");
    let templates = templates.as_array().expect("array");
    assert!(
        !templates.is_empty(),
        "expected at least one proposto template, got 0"
    );
    let template_id = templates[0]["template_id"]
        .as_str()
        .expect("template_id")
        .to_string();
    assert_eq!(templates[0]["status"].as_str(), Some("proposto"));
    assert!(["subscription", "fixed"].contains(&templates[0]["kind"].as_str().unwrap_or("")));

    // Re-running suggest must NOT create duplicates (idempotent on
    // template_id derived from account+merchant+kind).
    let stdout2 = envs(
        cargo_bin().arg("forecast").arg("suggest").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let templates2: Value = serde_json::from_slice(&stdout2).expect("valid json");
    assert_eq!(
        templates2.as_array().unwrap().len(),
        templates.len(),
        "second suggest should not duplicate"
    );

    // Accept the template — flips status to ativo, materialises 6 forecasts.
    envs(
        cargo_bin()
            .arg("forecast")
            .arg("accept")
            .arg("--template-id")
            .arg(&template_id)
            .arg("--materialize-months")
            .arg("6"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let status: String = conn
        .query_row(
            "SELECT status FROM forecast_template WHERE template_id = ?1",
            [&template_id],
            |r| r.get(0),
        )
        .expect("read status");
    assert_eq!(status, "ativo");
    let fc_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id = ?1",
            [&template_id],
            |r| r.get(0),
        )
        .expect("count forecasts");
    assert_eq!(fc_count, 6, "6 forecasts ahead");
    drop(conn);

    // Dismiss a new candidate (we re-seed a different merchant first so we
    // have a proposto to dismiss).
    for (i, amount) in ["-90.00", "-90.00", "-90.00"].iter().enumerate() {
        // Calendar-month steps (not *30 days) so each offset lands in a
        // distinct YYYY-MM regardless of today's date — keeps the test
        // deterministic at month boundaries.
        let date = today
            .checked_sub_months(chrono::Months::new(i as u32 + 1))
            .expect("date in range");
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(format!("gym-{i}"))
                .arg("--account-id")
                .arg("chk_test")
                .arg("--date")
                .arg(date.format("%Y-%m-%d").to_string())
                .arg("--description")
                .arg("Academia XYZ")
                .arg(format!("--amount={amount}")),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }
    let stdout3 = envs(
        cargo_bin().arg("forecast").arg("suggest").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let proposed: Value = serde_json::from_slice(&stdout3).expect("valid json");
    let gym_template = proposed
        .as_array()
        .unwrap()
        .iter()
        .find(|t| {
            t["status"].as_str() == Some("proposto")
                && t["description"]
                    .as_str()
                    .map(|s| s.contains("Academia"))
                    .unwrap_or(false)
        })
        .expect("Academia proposto");
    let gym_id = gym_template["template_id"].as_str().unwrap().to_string();

    envs(
        cargo_bin()
            .arg("forecast")
            .arg("dismiss")
            .arg("--template-id")
            .arg(&gym_id),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let conn = Connection::open(&db_path).expect("open db");
    let gym_status: String = conn
        .query_row(
            "SELECT status FROM forecast_template WHERE template_id = ?1",
            [&gym_id],
            |r| r.get(0),
        )
        .expect("read status");
    assert_eq!(gym_status, "descartado");

    // A subsequent suggest must NOT re-propose the dismissed merchant.
    let stdout4 = envs(
        cargo_bin().arg("forecast").arg("suggest").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let after: Value = serde_json::from_slice(&stdout4).expect("valid json");
    assert!(
        !after
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["status"].as_str() == Some("proposto")
                && t["description"]
                    .as_str()
                    .map(|s| s.contains("Academia"))
                    .unwrap_or(false)),
        "Academia must not reappear as proposto after dismiss"
    );
}

#[test]
fn forecast_suggest_envelope_groups_by_category() {
    // Layer 4 (ADR-0016): a category with 4+ months of varied-merchant
    // spend should produce an envelope candidate (kind='envelope',
    // merchant_pattern=NULL, category_id set, account_id=NULL).
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("chk_test")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("checking")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Checking"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // 5 months × 3 merchants per month, all in category 'mercado'. Totals
    // stable around R$ 800 (low CV).
    let today = chrono::Local::now().date_naive();
    let merchants_per_month = [
        ("Mercado A", "-280.00"),
        ("Mercado B", "-260.00"),
        ("Padaria C", "-260.00"),
    ];
    for month_offset in 1..=5_i64 {
        let date = today
            .checked_sub_months(chrono::Months::new(month_offset as u32))
            .expect("date in range");
        for (i, (merchant, amount)) in merchants_per_month.iter().enumerate() {
            envs(
                cargo_bin()
                    .arg("tx")
                    .arg("upsert-manual")
                    .arg("--transaction-id")
                    .arg(format!("mkt-{month_offset}-{i}"))
                    .arg("--account-id")
                    .arg("chk_test")
                    .arg("--date")
                    .arg(date.format("%Y-%m-%d").to_string())
                    .arg("--description")
                    .arg(*merchant)
                    .arg(format!("--amount={amount}"))
                    .arg("--category")
                    .arg("mercado"),
                &config_dir,
                &data_dir,
            )
            .assert()
            .success();
        }
    }

    let stdout = envs(
        cargo_bin()
            .arg("forecast")
            .arg("suggest")
            .arg("--raw")
            .arg("--lookback-months")
            .arg("6"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let templates: Value = serde_json::from_slice(&stdout).expect("valid json");
    let envelope = templates
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"].as_str() == Some("envelope"))
        .expect("expected envelope candidate");
    assert_eq!(envelope["category_id"].as_str(), Some("mercado"));
    assert!(envelope["merchant_pattern"].is_null());
    assert!(envelope["account_id"].is_null());

    // Accept the envelope → must materialise 6 monthly forecasts.
    let envelope_id = envelope["template_id"].as_str().unwrap().to_string();
    envs(
        cargo_bin()
            .arg("forecast")
            .arg("accept")
            .arg("--template-id")
            .arg(&envelope_id)
            .arg("--materialize-months")
            .arg("6"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let fc_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id = ?1",
            [&envelope_id],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(fc_count, 6, "6 monthly envelope forecasts");
}

#[test]
fn forecast_scenario_eval_projects_with_and_without_commitment() {
    // Layer 5 (ADR-0016): scenario eval — read-only what-if. Seeds a
    // current saldo and a few future outflow forecasts, then runs the
    // scenario command and checks the baseline / scenario / delta /
    // breach detection.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("chk_test")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("checking")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Checking"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Seed today's snapshot at R$ 10.000.
    let today = chrono::Local::now().date_naive();
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute(
        "INSERT INTO account_snapshots (
            snapshot_id, account_id, snapshot_date, balance, credit_limit,
            currency_code, source, actor_id, idempotency_key, metadata_json, created_at
         ) VALUES (
            'snap-1', 'chk_test', ?1, '10000.00', NULL,
            'BRL', 'manual', 'test', 'snap-1-key', '{}', '2025-01-01T12:00:00Z'
         )",
        [today.format("%Y-%m-%d").to_string()],
    )
    .expect("insert snapshot");

    // Run scenario: -R$ 500/mês por 6 meses, com minimum-balance = 8000.
    // Baseline saldo final stays at ~R$ 10.000; com cenário cai ~R$ 3.000
    // (6 × R$ 500) → eventually deve quebrar abaixo de R$ 8.000.
    let stdout = envs(
        cargo_bin()
            .arg("forecast")
            .arg("scenario")
            .arg("--amount=-500")
            .arg("--description")
            .arg("hypothetical recurring expense")
            .arg("--start")
            .arg(today.format("%Y-%m").to_string())
            .arg("--months")
            .arg("6")
            .arg("--project-months")
            .arg("6")
            .arg("--minimum-balance")
            .arg("8000")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let payload: Value = serde_json::from_slice(&stdout).expect("valid json");

    assert_eq!(payload["initial_balance"].as_str(), Some("10000.00"));
    assert_eq!(
        payload["baseline_final_balance"].as_str(),
        Some("10000.00"),
        "no forecasts seeded → baseline stays put"
    );
    // 6 months × -500 = -3000 → 10000 - 3000 = 7000.
    assert_eq!(payload["scenario_final_balance"].as_str(), Some("7000.00"));
    assert_eq!(payload["delta_total"].as_str(), Some("-3000.00"));
    // Saldo cai abaixo de 8.000 em algum momento — campo deve ser preenchido.
    assert!(
        !payload["first_breach_month"].is_null(),
        "expected a breach month when saldo dips below minimum"
    );

    drop(conn);
}

/// End-to-end coverage for the post-`sync pluggy` forecast orchestrator
/// (ADR-0016 Phase 6). The fixture seeds a credit-card installment chain
/// (`Notebook XYZ 3/6`), runs `sync pluggy`, and asserts that:
///
/// - the orchestrator's one-line summary prints to stdout,
/// - `forecast_template` got one detected installment row,
/// - `forecast` got 3 materialised parcelas (current=3, total=6 → 3 remaining),
/// - the materialised rows carry the template's FK and are in status `ativo`,
/// - `--no-forecast-refresh` on a subsequent sync prints the opt-out line
///   and does not re-materialise anything.
#[test]
fn sync_pluggy_runs_forecast_orchestrator_on_installments() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    // Local fixture with a parcelamento chain so Layer 1 (installments) has
    // something to detect. Two installments out of 6 → orchestrator must
    // materialise 3 remaining parcelas.
    let pluggy_config = temp.path().join("pluggy-config.json");
    write_file(
        &pluggy_config,
        r#"{
  "syncStartDate": "2026-02-01",
  "accounts": [
    { "id": "primary_checking", "pluggyAccountId": "fixture-checking" },
    { "id": "shared_credit", "pluggyAccountId": "fixture-credit" }
  ]
}"#,
    );
    let accounts_csv = temp.path().join("contas.csv");
    write_file(
        &accounts_csv,
        "id,owner,type,bank,label,pluggy_account_id,pluggy_item_id,billing_closing_day,billing_due_day\n\
         primary_checking,primary,checking,fintech,Primary Checking,fixture-checking,item-1,,\n\
         shared_credit,secondary,credit,fintech,Shared Credit Card,fixture-credit,item-2,3,10\n",
    );

    let fixture = temp.path().join("installment-fixture.json");
    write_file(
        &fixture,
        r#"{
  "accounts": [
    {
      "id": "fixture-checking",
      "item_id": "item-1",
      "name": "Primary Checking",
      "type": "checking",
      "status": "ACTIVE",
      "balance": 5000.00,
      "currency_code": "BRL",
      "updated_at": "2026-03-27T09:00:00Z"
    },
    {
      "id": "fixture-credit",
      "item_id": "item-2",
      "name": "Shared Credit Card",
      "type": "credit",
      "status": "ACTIVE",
      "balance": -1200.00,
      "currency_code": "BRL",
      "updated_at": "2026-03-27T09:00:00Z"
    }
  ],
  "transactions": [
    {
      "id": "pluggy-installment-001",
      "accountId": "fixture-credit",
      "date": "2026-02-12",
      "description": "Notebook XYZ 2/6",
      "amount": -200.00,
      "type": "debit",
      "status": "posted",
      "category": "electronics",
      "created_at": "2026-02-12T10:00:00Z",
      "updated_at": "2026-02-12T10:00:00Z"
    },
    {
      "id": "pluggy-installment-002",
      "accountId": "fixture-credit",
      "date": "2026-03-12",
      "description": "Notebook XYZ 3/6",
      "amount": -200.00,
      "type": "debit",
      "status": "posted",
      "category": "electronics",
      "created_at": "2026-03-12T10:00:00Z",
      "updated_at": "2026-03-12T10:00:00Z"
    }
  ]
}"#,
    );

    // First sync: orchestrator runs by default.
    let output = envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture)
            .arg("--to")
            .arg("2026-03-31")
            .arg("--no-enrich"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("sync output");
    assert!(output.status.success(), "sync failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Forecast: "),
        "expected the orchestrator one-liner in stdout, got:\n{stdout}"
    );

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");

    // Exactly one installment template was detected and persisted.
    let (template_id, kind, source, account_id): (String, String, String, Option<String>) = conn
        .query_row(
            "SELECT template_id, kind, source, account_id
             FROM forecast_template
             WHERE kind = 'installment'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("installment template row");
    assert_eq!(kind, "installment");
    assert_eq!(source, "detected");
    assert_eq!(account_id.as_deref(), Some("shared_credit"));

    // Three materialised forecasts (parcelas 4, 5, 6), all tied to that template
    // and in status `ativo`.
    let materialised_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id = ?1 AND status = 'ativo'",
            rusqlite::params![&template_id],
            |row| row.get(0),
        )
        .expect("count materialised forecasts");
    assert_eq!(
        materialised_count, 3,
        "expected 3 remaining parcelas materialised (current=3, total=6)"
    );

    // Audit log saw the orchestrator's writes — `installment` template upsert
    // plus per-forecast upserts. Exact counts depend on the orchestrator,
    // but at least one event of each kind must be present.
    let template_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log
             WHERE entity_type = 'forecast_template' AND entity_id = ?1",
            rusqlite::params![&template_id],
            |row| row.get(0),
        )
        .expect("count template audit events");
    assert!(
        template_events >= 1,
        "expected at least one audit event for the installment template"
    );

    drop(conn);

    // Second sync, with the opt-out: no orchestrator one-liner, no new
    // forecasts. Re-running with no new transactions must remain idempotent.
    let opt_out = envs(
        cargo_bin()
            .arg("sync")
            .arg("pluggy")
            .arg("--pluggy-config")
            .arg(&pluggy_config)
            .arg("--accounts-csv")
            .arg(&accounts_csv)
            .arg("--fixture")
            .arg(&fixture)
            .arg("--to")
            .arg("2026-03-31")
            .arg("--no-enrich")
            .arg("--no-forecast-refresh"),
        &config_dir,
        &data_dir,
    )
    .output()
    .expect("sync output");
    assert!(opt_out.status.success(), "opt-out sync failed: {opt_out:?}");
    let opt_out_stdout = String::from_utf8_lossy(&opt_out.stdout);
    assert!(
        opt_out_stdout.contains("Forecast refresh automático: pulado"),
        "expected opt-out line in stdout, got:\n{opt_out_stdout}"
    );
    assert!(
        !opt_out_stdout.contains("Forecast: "),
        "opt-out sync should not print the orchestrator one-liner"
    );

    // No-op idempotency: re-counting the materialised parcelas yields the same 3.
    let conn = Connection::open(&db_path).expect("reopen db");
    let still_three: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id = ?1 AND status = 'ativo'",
            rusqlite::params![&template_id],
            |row| row.get(0),
        )
        .expect("re-count materialised forecasts");
    assert_eq!(still_three, 3);
    drop(conn);
}

#[test]
fn forecast_refresh_materialises_open_card_bill() {
    // End-to-end: a credit card with billing_closing_day=3 / billing_due_day=10
    // has a pending purchase on 2026-06-15 (after closing day 3 → cycle "2026-07").
    // `fin forecast refresh` must create one forecast row with:
    //   due_date = 2026-07-10, amount = -150.00, recurrence = card-cycle
    // Running twice must be idempotent (no duplicate rows).
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    // Create a credit account.
    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("cc-bill-test")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Test Card"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Inject billing metadata directly (account upsert has no billing-day flags).
    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE accounts SET metadata_json = json_set(metadata_json, \
         '$.billing_closing_day', '3', '$.billing_due_day', '10') \
         WHERE account_id = 'cc-bill-test'",
        [],
    )
    .expect("inject billing metadata");
    drop(conn);

    // Purchase on 2026-06-15: D=15 > C=3 → cycle_ref = 2026-07.
    envs(
        cargo_bin()
            .arg("tx")
            .arg("upsert-manual")
            .arg("--transaction-id")
            .arg("open-purchase-01")
            .arg("--account-id")
            .arg("cc-bill-test")
            .arg("--date")
            .arg("2026-06-15")
            .arg("--description")
            .arg("Supermercado XYZ")
            .arg("--amount=-150.00")
            .arg("--payment-status")
            .arg("pending"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // First refresh — must report 1 open-bill forecast.
    let stdout = envs(
        cargo_bin().arg("forecast").arg("refresh").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let report: Value = serde_json::from_slice(&stdout).expect("valid refresh json");
    assert_eq!(
        report["open_bill_forecasts"].as_u64(),
        Some(1),
        "first refresh must produce 1 open-bill forecast",
    );

    let conn = Connection::open(&db_path).expect("open sqlite");
    let (due_date, amount_cents, recurrence): (String, f64, String) = conn
        .query_row(
            "SELECT due_date, CAST(amount AS REAL) * 100, recurrence \
             FROM forecast \
             WHERE recurrence = 'card-cycle' AND account_id = 'cc-bill-test'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("open-bill forecast row");
    assert_eq!(due_date, "2026-07-10");
    assert!(
        (amount_cents - (-15000.0_f64)).abs() < 1.0,
        "amount should be -15000 cents, got {amount_cents}"
    );
    assert_eq!(recurrence, "card-cycle");

    // Count total rows with this recurrence — must be exactly 1.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE recurrence = 'card-cycle'",
            [],
            |row| row.get(0),
        )
        .expect("count card-cycle forecasts");
    assert_eq!(count, 1);
    drop(conn);

    // Second refresh — idempotent: same 1 row, no duplicates.
    envs(
        cargo_bin().arg("forecast").arg("refresh").arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let conn = Connection::open(&db_path).expect("reopen sqlite");
    let count_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE recurrence = 'card-cycle'",
            [],
            |row| row.get(0),
        )
        .expect("count after second refresh");
    assert_eq!(count_after, 1, "second refresh must not duplicate forecast");
    drop(conn);
}

// ── phai mcp — stdio MCP server ─────────────────────────────────────────────

/// Full protocol round-trip against a seeded SQLite store: initialize,
/// tools/list, then a tools/call that re-execs the binary and must return the
/// same JSON the CLI's `--raw` path produces.
#[test]
fn mcp_serves_reports_over_stdio() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    seed_fixture_sync(&temp, &config_dir, &data_dir);

    let mut child = envs(&mut cargo_bin(), &config_dir, &data_dir)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn phai mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    fn ask(
        stdin: &mut impl Write,
        lines: &mut impl Iterator<Item = std::io::Result<String>>,
        msg: &str,
    ) -> Value {
        stdin.write_all(msg.as_bytes()).expect("write");
        stdin.write_all(b"\n").expect("write nl");
        stdin.flush().expect("flush");
        let line = lines.next().expect("reply").expect("read reply");
        serde_json::from_str(&line).expect("reply is JSON")
    }

    let init = ask(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "phai");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // Notification: must produce no reply (the next reply belongs to id 2).
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .expect("write notification");

    let list = ask(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert!(tools.iter().any(|t| t["name"] == "monthly_spend"));

    let call = ask(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"monthly_spend","arguments":{"month":"2026-03"}}}"#,
    );
    assert_eq!(call["result"]["isError"], false, "call failed: {call}");
    let text = call["result"]["content"][0]["text"].as_str().expect("text");
    let payload: Value = serde_json::from_str(text).expect("tool output is JSON");
    assert!(payload.is_object() || payload.is_array());

    // Bad input is rejected before any subprocess runs.
    let bad = ask(
        &mut stdin,
        &mut lines,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"monthly_spend","arguments":{"month":"march"}}}"#,
    );
    assert_eq!(bad["result"]["isError"], true);

    drop(stdin); // EOF → clean exit
    let status = child.wait().expect("wait");
    assert!(status.success());
}

/// `phai invite create` reads a passphrase off stdin, seals the service account
/// into a `PHAI1E-` token, and prints only the token on stdout (guidance goes to
/// stderr). The owner's configured project/dataset can be overridden by flags.
#[test]
fn invite_create_emits_sealed_token() {
    use std::io::Write;
    use std::process::Stdio;

    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    let sa_path = temp.path().join("phai-family.json");
    write_file(
        &sa_path,
        r#"{"type":"service_account","project_id":"demo","private_key":"-----FAKE-----","client_email":"phai-family@demo.iam.gserviceaccount.com"}"#,
    );

    let mut cmd = cargo_bin();
    let mut child = envs(&mut cmd, &config_dir, &data_dir)
        .arg("invite")
        .arg("create")
        .arg("--service-account-path")
        .arg(&sa_path)
        .arg("--actor-id")
        .arg("esposa")
        .arg("--label")
        .arg("MacBook Esposa")
        .arg("--project-id")
        .arg("demo")
        .arg("--dataset-id")
        .arg("phai")
        .arg("--passphrase-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn phai invite create");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"hunter2pass\n")
        .expect("write passphrase");

    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "invite create should succeed");
    let token = String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_string();
    assert!(
        token.starts_with("PHAI1E-"),
        "stdout must be just the token, got: {token}"
    );

    // The emitted token decrypts back to the same coordinates with the passphrase.
    let invite = phai_core::open_invite(&token, "hunter2pass").expect("open invite");
    assert_eq!(invite.project_id, "demo");
    assert_eq!(invite.dataset_id, "phai");
    assert_eq!(invite.actor_id, "esposa");
    assert_eq!(invite.role, "rw");
    assert!(phai_core::open_invite(&token, "wrong").is_err());
}

/// A JSON file that is not a service-account key is rejected before any sealing.
#[test]
fn invite_create_rejects_non_service_account_json() {
    use std::io::Write;
    use std::process::Stdio;

    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    let sa_path = temp.path().join("not-a-key.json");
    write_file(&sa_path, r#"{"hello":"world"}"#);

    let mut cmd = cargo_bin();
    let mut child = envs(&mut cmd, &config_dir, &data_dir)
        .arg("invite")
        .arg("create")
        .arg("--service-account-path")
        .arg(&sa_path)
        .arg("--actor-id")
        .arg("esposa")
        .arg("--label")
        .arg("Test")
        .arg("--project-id")
        .arg("demo")
        .arg("--dataset-id")
        .arg("phai")
        .arg("--passphrase-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"pw\n")
        .expect("write");

    let out = child.wait_with_output().expect("wait");
    assert!(!out.status.success(), "non-SA JSON must be rejected");
}

// ---------------------------------------------------------------------------
// phai scenario — named what-if planning scenarios (ADR-0037)
// ---------------------------------------------------------------------------

/// `YYYY-MM` for `today + n` months.
fn month_ref_from_today(n: i64) -> String {
    let today = Utc::now().date_naive();
    let shifted = today
        .checked_add_months(chrono::Months::new(n as u32))
        .expect("month shift");
    shifted.format("%Y-%m").to_string()
}

#[test]
fn scenario_full_cycle_create_edit_show_diff_archive_delete() {
    // End-to-end over SQLite: seed an installment chain (relative to today so
    // the remaining parcelas are always in the projection horizon), then run
    // the whole scenario lifecycle: create → all five change kinds → show →
    // diff → orphan handling via prune → delete-change → archive → delete.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("card_a")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Card A"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Parcelas 1..3 of a 12x chain anchored at today-2m/-1m/today, day 5, so
    // the 9 remaining parcelas land on today+1m..today+9m regardless of when
    // the test runs.
    let today = Utc::now().date_naive();
    for (n, months_back) in [(1u32, 2u32), (2, 1), (3, 0)] {
        let date = today
            .checked_sub_months(chrono::Months::new(months_back))
            .expect("shift")
            .with_day(5)
            .expect("day 5");
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(format!("scn-parcel-{n:02}"))
                .arg("--account-id")
                .arg("card_a")
                .arg("--date")
                .arg(date.format("%Y-%m-%d").to_string())
                .arg("--description")
                .arg(format!("Loja Exemplo Sofa {n}/12"))
                .arg("--amount=-300.00"),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }
    envs(
        cargo_bin()
            .arg("forecast")
            .arg("refresh-installments")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let template_id: String = conn
        .query_row(
            "SELECT template_id FROM forecast_template WHERE kind = 'installment' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("template id");
    let forecast_for = |month: &str| -> String {
        conn.query_row(
            "SELECT forecast_id FROM forecast
             WHERE template_id IS NOT NULL AND strftime('%Y-%m', due_date) = ?1
             LIMIT 1",
            [month],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| panic!("forecast in month {month}"))
    };
    let fc_month1 = forecast_for(&month_ref_from_today(1));
    let fc_month5 = forecast_for(&month_ref_from_today(5));
    drop(conn);

    // Create the scenario.
    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("create")
            .arg("--name")
            .arg("plano de teste")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let created: Value = serde_json::from_slice(&stdout).expect("create json");
    let scenario_id = created["scenario_id"].as_str().expect("id").to_string();

    // 1. add_one_shot: trip in month+2.
    envs(
        cargo_bin()
            .arg("scenario")
            .arg("add")
            .arg(&scenario_id)
            .arg("--month")
            .arg(month_ref_from_today(2))
            .arg("--amount=-2000.00")
            .arg("--description")
            .arg("viagem")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    // 2. adjust_amount: month+1 parcela from -300 to -100 (delta +200).
    envs(
        cargo_bin()
            .arg("scenario")
            .arg("adjust")
            .arg(&scenario_id)
            .arg("--forecast")
            .arg(&fc_month1)
            .arg("--amount=-100.00"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    // 3. skip_forecast: month+5 parcela (delta +300).
    envs(
        cargo_bin()
            .arg("scenario")
            .arg("skip")
            .arg(&scenario_id)
            .arg("--forecast")
            .arg(&fc_month5),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    // 4. end_template from month+3 (kills remaining parcelas from there on).
    envs(
        cargo_bin()
            .arg("scenario")
            .arg("end-template")
            .arg(&scenario_id)
            .arg("--template")
            .arg(&template_id)
            .arg("--from")
            .arg(month_ref_from_today(3)),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    // 5. hypothetical_installment: 10x of -300 starting month+1.
    envs(
        cargo_bin()
            .arg("scenario")
            .arg("installment")
            .arg(&scenario_id)
            .arg("--start")
            .arg(month_ref_from_today(1))
            .arg("--amount=-300.00")
            .arg("--months")
            .arg("10")
            .arg("--description")
            .arg("sofá novo"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Show: five changes, no orphans, deterministic deltas.
    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("show")
            .arg(&scenario_id)
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let shown: Value = serde_json::from_slice(&stdout).expect("show json");
    assert_eq!(shown["changes"].as_array().map(Vec::len), Some(5));
    assert_eq!(
        shown["orphaned_change_ids"].as_array().map(Vec::len),
        Some(0)
    );
    let delta_of = |payload: &Value, month: &str| -> Option<String> {
        payload["monthly_delta"].as_array().and_then(|rows| {
            rows.iter()
                .find(|row| row["month"].as_str() == Some(month))
                .and_then(|row| row["delta"].as_str().map(str::to_string))
        })
    };
    // month+1: adjust +200.00, hypothetical parcela -300.00 → -100.00.
    assert_eq!(
        delta_of(&shown, &month_ref_from_today(1)).as_deref(),
        Some("-100.00")
    );
    // month+2: add -2000.00, hypothetical parcela -300.00 → -2300.00.
    assert_eq!(
        delta_of(&shown, &month_ref_from_today(2)).as_deref(),
        Some("-2300.00")
    );

    // Diff against baseline mirrors the monthly deltas.
    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("diff")
            .arg(&scenario_id)
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let diffed: Value = serde_json::from_slice(&stdout).expect("diff json");
    let diff_month2 = diffed["monthly_diff"].as_array().and_then(|rows| {
        rows.iter()
            .find(|row| row["month"].as_str() == Some(month_ref_from_today(2).as_str()))
            .and_then(|row| row["delta"].as_str().map(str::to_string))
    });
    assert_eq!(diff_month2.as_deref(), Some("-2300.00"));

    // Orphan handling: realize the adjusted forecast behind the engine's
    // back (simulates reconciliation) → show flags the change, prune marks it.
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute(
        "UPDATE forecast SET status = 'realizado' WHERE forecast_id = ?1",
        [&fc_month1],
    )
    .expect("realize forecast");
    drop(conn);
    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("show")
            .arg(&scenario_id)
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let shown: Value = serde_json::from_slice(&stdout).expect("show json");
    assert_eq!(
        shown["orphaned_change_ids"].as_array().map(Vec::len),
        Some(1)
    );
    envs(
        cargo_bin().arg("scenario").arg("prune").arg(&scenario_id),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .stdout(predicate::str::contains("1 mudança"));

    // delete-change removes one delta.
    let orphan_id = shown["orphaned_change_ids"][0]
        .as_str()
        .expect("orphan id")
        .to_string();
    envs(
        cargo_bin()
            .arg("scenario")
            .arg("delete-change")
            .arg(&orphan_id)
            .arg("--scenario")
            .arg(&scenario_id),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    // Archive blocks further edits.
    envs(
        cargo_bin().arg("scenario").arg("archive").arg(&scenario_id),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    envs(
        cargo_bin()
            .arg("scenario")
            .arg("add")
            .arg(&scenario_id)
            .arg("--month")
            .arg(month_ref_from_today(2))
            .arg("--amount=-1.00")
            .arg("--description")
            .arg("bloqueado"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .failure();

    // Delete removes the scenario and its changes.
    envs(
        cargo_bin().arg("scenario").arg("delete").arg(&scenario_id),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();
    let conn = Connection::open(&db_path).expect("open db");
    let scenarios: i64 = conn
        .query_row("SELECT COUNT(*) FROM plan_scenario", [], |r| r.get(0))
        .expect("count scenarios");
    let changes: i64 = conn
        .query_row("SELECT COUNT(*) FROM plan_change", [], |r| r.get(0))
        .expect("count changes");
    assert_eq!(scenarios, 0);
    assert_eq!(changes, 0);
}

#[test]
fn scenario_promote_applies_deltas_to_real_plan_idempotently() {
    // Promote a scenario carrying all five change kinds, then verify the
    // real plan: one-shot forecast created, forecast adjusted, forecast
    // discarded, template end_date set (+ future parcelas discarded), and
    // a manual installment template + parcelas created. A second promote
    // must be a no-op (changes already 'aplicado'), and orphaned changes
    // are skipped with a warning.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("card_a")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Card A"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let today = Utc::now().date_naive();
    for (n, months_back) in [(1u32, 2u32), (2, 1), (3, 0)] {
        let date = today
            .checked_sub_months(chrono::Months::new(months_back))
            .expect("shift")
            .with_day(5)
            .expect("day 5");
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(format!("promo-parcel-{n:02}"))
                .arg("--account-id")
                .arg("card_a")
                .arg("--date")
                .arg(date.format("%Y-%m-%d").to_string())
                .arg("--description")
                .arg(format!("Loja Exemplo Cadeira {n}/12"))
                .arg("--amount=-300.00"),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }
    envs(
        cargo_bin()
            .arg("forecast")
            .arg("refresh-installments")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let template_id: String = conn
        .query_row(
            "SELECT template_id FROM forecast_template WHERE kind = 'installment' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("template id");
    let forecast_for = |month: &str| -> String {
        conn.query_row(
            "SELECT forecast_id FROM forecast
             WHERE template_id IS NOT NULL AND strftime('%Y-%m', due_date) = ?1
             LIMIT 1",
            [month],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| panic!("forecast in month {month}"))
    };
    let fc_month1 = forecast_for(&month_ref_from_today(1));
    let fc_month2 = forecast_for(&month_ref_from_today(2));
    drop(conn);

    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("create")
            .arg("--name")
            .arg("promover teste")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let created: Value = serde_json::from_slice(&stdout).expect("create json");
    let scenario_id = created["scenario_id"].as_str().expect("id").to_string();

    // All five kinds. The skip target (month+2 parcela) is deliberately
    // *also* covered by end-template (from month+4) on other months so both
    // operations coexist.
    for args in [
        vec![
            "add".to_string(),
            scenario_id.clone(),
            "--month".to_string(),
            month_ref_from_today(2),
            "--amount=-2000.00".to_string(),
            "--description".to_string(),
            "viagem".to_string(),
        ],
        vec![
            "adjust".to_string(),
            scenario_id.clone(),
            "--forecast".to_string(),
            fc_month1.clone(),
            "--amount=-100.00".to_string(),
        ],
        vec![
            "skip".to_string(),
            scenario_id.clone(),
            "--forecast".to_string(),
            fc_month2.clone(),
        ],
        vec![
            "end-template".to_string(),
            scenario_id.clone(),
            "--template".to_string(),
            template_id.clone(),
            "--from".to_string(),
            month_ref_from_today(4),
        ],
        vec![
            "installment".to_string(),
            scenario_id.clone(),
            "--start".to_string(),
            month_ref_from_today(1),
            "--amount=-250.00".to_string(),
            "--months".to_string(),
            "6".to_string(),
            "--description".to_string(),
            "cadeira nova".to_string(),
        ],
    ] {
        let mut cmd = cargo_bin();
        cmd.arg("scenario");
        for arg in &args {
            cmd.arg(arg);
        }
        envs(&mut cmd, &config_dir, &data_dir).assert().success();
    }

    // Dry-run: reports the writes but touches nothing.
    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("promote")
            .arg(&scenario_id)
            .arg("--dry-run")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let dry: Value = serde_json::from_slice(&stdout).expect("dry json");
    assert_eq!(dry["applied"].as_array().map(Vec::len), Some(5));
    let conn = Connection::open(&db_path).expect("open db");
    let scn_forecasts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE forecast_id LIKE 'scenario-%' OR forecast_id LIKE 'scn-installment-%'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(scn_forecasts, 0, "dry-run must not write");
    drop(conn);

    // Real promotion.
    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("promote")
            .arg(&scenario_id)
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let summary: Value = serde_json::from_slice(&stdout).expect("promote json");
    assert_eq!(summary["applied"].as_array().map(Vec::len), Some(5));
    assert_eq!(summary["skipped_orphans"].as_array().map(Vec::len), Some(0));

    let conn = Connection::open(&db_path).expect("open db");
    // add_one_shot → real forecast row.
    let one_shot: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE description = 'viagem' AND status = 'ativo'",
            [],
            |r| r.get(0),
        )
        .expect("one shot");
    assert_eq!(one_shot, 1);
    // adjust_amount → amount overwritten.
    let adjusted: String = conn
        .query_row(
            "SELECT amount FROM forecast WHERE forecast_id = ?1",
            [&fc_month1],
            |r| r.get(0),
        )
        .expect("adjusted amount");
    assert_eq!(adjusted, "-100.00");
    // skip_forecast → discarded.
    let skipped: String = conn
        .query_row(
            "SELECT status FROM forecast WHERE forecast_id = ?1",
            [&fc_month2],
            |r| r.get(0),
        )
        .expect("skipped status");
    assert_eq!(skipped, "descartado");
    // end_template → end_date set + future parcelas discarded.
    let end_date: Option<String> = conn
        .query_row(
            "SELECT end_date FROM forecast_template WHERE template_id = ?1",
            [&template_id],
            |r| r.get(0),
        )
        .expect("end date");
    assert!(end_date.is_some(), "template end_date must be set");
    let future_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast
             WHERE template_id = ?1 AND status = 'ativo'
               AND strftime('%Y-%m', due_date) >= ?2",
            rusqlite::params![&template_id, &month_ref_from_today(4)],
            |r| r.get(0),
        )
        .expect("future active");
    assert_eq!(future_active, 0, "parcelas from cutoff must be discarded");
    // hypothetical_installment → manual template + 6 parcelas.
    let installment_forecasts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast WHERE template_id LIKE 'scn-installment-%'",
            [],
            |r| r.get(0),
        )
        .expect("installment forecasts");
    assert_eq!(installment_forecasts, 6);
    // Scenario is now 'promovido' and its changes 'aplicado'.
    let scn_status: String = conn
        .query_row(
            "SELECT status FROM plan_scenario WHERE scenario_id = ?1",
            [&scenario_id],
            |r| r.get(0),
        )
        .expect("scenario status");
    assert_eq!(scn_status, "promovido");
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM plan_change WHERE scenario_id = ?1 AND status != 'aplicado'",
            [&scenario_id],
            |r| r.get(0),
        )
        .expect("pending changes");
    assert_eq!(pending, 0);
    drop(conn);

    // Second promote → no-op.
    let stdout = envs(
        cargo_bin()
            .arg("scenario")
            .arg("promote")
            .arg(&scenario_id)
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let second: Value = serde_json::from_slice(&stdout).expect("second promote json");
    assert_eq!(second["applied"].as_array().map(Vec::len), Some(0));
}

// ---------------------------------------------------------------------------
// phai serve — baseline forecast endpoints for the unified sheet
// ---------------------------------------------------------------------------

/// Kills the spawned `phai serve` process when the test ends (pass or panic).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `phai serve` on a free port and wait until the store actor answers.
/// Returns the process guard and the `http://127.0.0.1:<port>` base URL.
async fn spawn_serve(config_dir: &Path, data_dir: &Path) -> (KillOnDrop, String) {
    // Grab a free port; the tiny bind/spawn race is acceptable for a test.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe port")
        .local_addr()
        .expect("local addr")
        .port();
    let mut cmd = cargo_bin();
    cmd.arg("serve").arg("--port").arg(port.to_string());
    envs(&mut cmd, config_dir, data_dir)
        .env("PHAI_NO_BROWSER", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = KillOnDrop(cmd.spawn().expect("spawn phai serve"));
    let base = format!("http://127.0.0.1:{port}");

    // Readiness: /api/forecasts round-trips through the store actor, so a 200
    // means migrations ran and the SQLite store is answering.
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for _ in 0..150 {
        if let Ok(resp) = client.get(format!("{base}/api/forecasts")).send().await {
            if resp.status().is_success() {
                return (child, base);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("phai serve did not become ready on {base}");
}

#[tokio::test]
async fn serve_baseline_discard_and_end_template_endpoints() {
    // The unified sheet's baseline writes, over a real installment chain:
    // POST /api/forecast/discard soft-discards a template-materialized
    // parcela (which /api/forecast/delete refuses), and
    // POST /api/forecast-template/end stamps the template's end_date and
    // discards the active parcelas from the cutoff month on.
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    setup_local_auth_migrate(&config_dir, &data_dir);

    envs(
        cargo_bin()
            .arg("account")
            .arg("upsert")
            .arg("--account-id")
            .arg("card_a")
            .arg("--owner")
            .arg("test")
            .arg("--account-type")
            .arg("credit")
            .arg("--bank")
            .arg("test-bank")
            .arg("--label")
            .arg("Card A"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let today = Utc::now().date_naive();
    for (n, months_back) in [(1u32, 2u32), (2, 1), (3, 0)] {
        let date = today
            .checked_sub_months(chrono::Months::new(months_back))
            .expect("shift")
            .with_day(5)
            .expect("day 5");
        envs(
            cargo_bin()
                .arg("tx")
                .arg("upsert-manual")
                .arg("--transaction-id")
                .arg(format!("sheet-parcel-{n:02}"))
                .arg("--account-id")
                .arg("card_a")
                .arg("--date")
                .arg(date.format("%Y-%m-%d").to_string())
                .arg("--description")
                .arg(format!("Loja Exemplo Cadeira {n}/12"))
                .arg("--amount=-300.00"),
            &config_dir,
            &data_dir,
        )
        .assert()
        .success();
    }
    envs(
        cargo_bin()
            .arg("forecast")
            .arg("refresh-installments")
            .arg("--raw"),
        &config_dir,
        &data_dir,
    )
    .assert()
    .success();

    let db_path = data_dir.join("phai.local.db");
    let conn = Connection::open(&db_path).expect("open db");
    let template_id: String = conn
        .query_row(
            "SELECT template_id FROM forecast_template WHERE kind = 'installment' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("template id");
    let forecast_for = |month: &str| -> String {
        conn.query_row(
            "SELECT forecast_id FROM forecast
             WHERE template_id IS NOT NULL AND strftime('%Y-%m', due_date) = ?1
             LIMIT 1",
            [month],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| panic!("forecast in month {month}"))
    };
    let fc_month1 = forecast_for(&month_ref_from_today(1));
    drop(conn);

    let (_serve, base) = spawn_serve(&config_dir, &data_dir).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    // The old delete endpoint keeps refusing template-materialized rows...
    let resp = client
        .post(format!("{base}/api/forecast/delete"))
        .json(&serde_json::json!({ "forecastId": fc_month1 }))
        .send()
        .await
        .expect("delete request");
    assert_eq!(resp.status().as_u16(), 409, "delete must refuse a parcela");

    // ...while discard soft-discards any active forecast, parcelas included.
    let resp = client
        .post(format!("{base}/api/forecast/discard"))
        .json(&serde_json::json!({ "forecastId": fc_month1 }))
        .send()
        .await
        .expect("discard request");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.expect("discard json");
    assert_eq!(body["status"], "descartado");

    // Discard is not repeatable (already inactive) and 404s on unknown ids.
    let resp = client
        .post(format!("{base}/api/forecast/discard"))
        .json(&serde_json::json!({ "forecastId": fc_month1 }))
        .send()
        .await
        .expect("repeat discard request");
    assert_eq!(resp.status().as_u16(), 409);
    let resp = client
        .post(format!("{base}/api/forecast/discard"))
        .json(&serde_json::json!({ "forecastId": "missing" }))
        .send()
        .await
        .expect("unknown discard request");
    assert_eq!(resp.status().as_u16(), 404);

    // End the recurrence from month+3: month+2 parcela survives, everything
    // due on/after the cutoff is discarded, end_date = last day of month+2.
    let resp = client
        .post(format!("{base}/api/forecast-template/end"))
        .json(&serde_json::json!({
            "templateId": template_id,
            "effectiveFrom": month_ref_from_today(3),
        }))
        .send()
        .await
        .expect("end request");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.expect("end json");
    assert_eq!(body["templateId"], template_id.as_str());
    assert!(
        body["discardedForecasts"].as_u64().unwrap_or(0) >= 1,
        "cutoff must discard future parcelas, got: {body}"
    );

    // Month validation (400) and unknown template (404).
    let resp = client
        .post(format!("{base}/api/forecast-template/end"))
        .json(&serde_json::json!({
            "templateId": template_id,
            "effectiveFrom": "setembro",
        }))
        .send()
        .await
        .expect("bad month request");
    assert_eq!(resp.status().as_u16(), 400);
    let resp = client
        .post(format!("{base}/api/forecast-template/end"))
        .json(&serde_json::json!({
            "templateId": "missing",
            "effectiveFrom": month_ref_from_today(3),
        }))
        .send()
        .await
        .expect("unknown template request");
    assert_eq!(resp.status().as_u16(), 404);

    // Persisted state: discard + end-template landed in SQLite with audits.
    let conn = Connection::open(&db_path).expect("reopen db");
    let discarded_status: String = conn
        .query_row(
            "SELECT status FROM forecast WHERE forecast_id = ?1",
            [&fc_month1],
            |r| r.get(0),
        )
        .expect("discarded parcela status");
    assert_eq!(discarded_status, "descartado");
    let end_date: Option<String> = conn
        .query_row(
            "SELECT end_date FROM forecast_template WHERE template_id = ?1",
            [&template_id],
            |r| r.get(0),
        )
        .expect("end date");
    let expected_end = phai_core::scenario::parse_month(&month_ref_from_today(3))
        .expect("cutoff")
        .pred_opt()
        .expect("end date")
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(end_date.as_deref(), Some(expected_end.as_str()));
    let future_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast
             WHERE template_id = ?1 AND status = 'ativo'
               AND strftime('%Y-%m', due_date) >= ?2",
            rusqlite::params![&template_id, &month_ref_from_today(3)],
            |r| r.get(0),
        )
        .expect("future active");
    assert_eq!(future_active, 0, "parcelas from cutoff must be discarded");
    let survivor_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM forecast
             WHERE template_id = ?1 AND status = 'ativo'
               AND strftime('%Y-%m', due_date) = ?2",
            rusqlite::params![&template_id, &month_ref_from_today(2)],
            |r| r.get(0),
        )
        .expect("survivor active");
    assert_eq!(survivor_active, 1, "parcela before the cutoff must survive");
    let audit_actions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_log
             WHERE action IN ('discard', 'end_template')",
            [],
            |r| r.get(0),
        )
        .expect("audit rows");
    assert!(
        audit_actions >= 3,
        "discard + end-template writes must be audited, got {audit_actions}"
    );
}
