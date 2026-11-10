//! Thin Google Sheets API v4 client for the planning sheet (ADR-0041).
//!
//! Authenticates with the same service account as the BigQuery backend; the
//! person shares their spreadsheet with that account as an editor. Only the
//! handful of calls the planning sheet needs are wrapped: list tabs, read
//! tabs, and replace tabs (create, clear, write, format) in batches.

use std::cell::RefCell;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use yup_oauth2::{read_service_account_key, ServiceAccountAuthenticator};

use crate::planning_sheet::{Cell, Range, Style, Validation};

mod format;

const SHEETS_SCOPE: &str = "https://www.googleapis.com/auth/spreadsheets";
const SHEETS_API: &str = "https://sheets.googleapis.com/v4/spreadsheets";

/// How a tab is formatted after its values are written.
#[derive(Debug, Clone, PartialEq)]
pub enum TabFormat {
    /// A month tab: colours, number formats, merges, dropdowns, column
    /// widths, frozen header and hidden id column.
    Month(MonthFormat),
    /// Values only (hidden bookkeeping tabs).
    Plain,
}

/// The presentation half of a rendered month tab.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MonthFormat {
    pub styles: Vec<(Style, Range)>,
    pub merges: Vec<Range>,
    pub validations: Vec<(Validation, Range)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabWrite {
    pub title: String,
    pub cells: Vec<Vec<Cell>>,
    /// Hide the tab when creating it (manifest and lists).
    pub hidden: bool,
    pub format: TabFormat,
}

#[async_trait(?Send)]
pub trait SheetsApi {
    /// Titles of every tab in the spreadsheet.
    async fn tab_titles(&self) -> Result<Vec<String>>;
    /// Unformatted cell values of each tab, in the order asked. Every title
    /// must exist (see [`Self::tab_titles`]).
    async fn read_tabs(&self, titles: &[String]) -> Result<Vec<Vec<Vec<Value>>>>;
    /// Replace each tab's content, creating missing tabs, then format them.
    async fn write_tabs(&self, tabs: &[TabWrite]) -> Result<()>;
}

enum Token {
    ServiceAccount(PathBuf),
    Fixed(String),
}

pub struct HttpSheets {
    client: Client,
    base: String,
    token: Token,
    cached: RefCell<Option<String>>,
}

impl HttpSheets {
    pub fn new(service_account_path: PathBuf, spreadsheet_id: &str) -> Result<Self> {
        Self::build(
            format!("{SHEETS_API}/{spreadsheet_id}"),
            Token::ServiceAccount(service_account_path),
        )
    }

    /// Point at another endpoint with a fixed bearer token (a local mock).
    pub fn with_endpoint(base: &str, token: &str) -> Result<Self> {
        Self::build(
            base.trim_end_matches('/').to_string(),
            Token::Fixed(token.into()),
        )
    }

    fn build(base: String, token: Token) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .context("Falha ao construir cliente HTTP do Google Sheets")?,
            base,
            token,
            cached: RefCell::new(None),
        })
    }

    async fn bearer(&self) -> Result<String> {
        if let Some(token) = self.cached.borrow().as_ref() {
            return Ok(token.clone());
        }
        let token = match &self.token {
            Token::Fixed(token) => token.clone(),
            Token::ServiceAccount(path) => {
                let key = read_service_account_key(path)
                    .await
                    .context("Falha ao ler a service account para o Google Sheets")?;
                let auth = ServiceAccountAuthenticator::builder(key)
                    .build()
                    .await
                    .context("Falha ao autenticar no Google Sheets")?;
                auth.token(&[SHEETS_SCOPE])
                    .await?
                    .token()
                    .map(str::to_string)
                    .context("Token do Google Sheets ausente")?
            }
        };
        *self.cached.borrow_mut() = Some(token.clone());
        Ok(token)
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(self.bearer().await?)
            .query(query)
            .send()
            .await
            .context("Google Sheets indisponível")?;
        read_json(response).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(self.bearer().await?)
            .json(body)
            .send()
            .await
            .context("Google Sheets indisponível")?;
        read_json(response).await
    }

    /// `title → sheetId`.
    async fn sheet_ids(&self) -> Result<Vec<(String, i64)>> {
        let body = self
            .get("", &[("fields", "sheets.properties(sheetId,title)".into())])
            .await?;
        Ok(body["sheets"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|s| {
                let p = &s["properties"];
                Some((p["title"].as_str()?.to_string(), p["sheetId"].as_i64()?))
            })
            .collect())
    }
}

async fn read_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let message = body["error"]["message"].as_str().unwrap_or("sem detalhe");
        return Err(anyhow!("Google Sheets respondeu {status}: {message}"));
    }
    Ok(body)
}

/// `'Tab name'` as A1 notation for the whole tab.
pub(crate) fn quoted(title: &str) -> String {
    format!("'{}'", title.replace('\'', "''"))
}

#[async_trait(?Send)]
impl SheetsApi for HttpSheets {
    async fn tab_titles(&self) -> Result<Vec<String>> {
        Ok(self
            .sheet_ids()
            .await?
            .into_iter()
            .map(|(t, _)| t)
            .collect())
    }

    async fn read_tabs(&self, titles: &[String]) -> Result<Vec<Vec<Vec<Value>>>> {
        if titles.is_empty() {
            return Ok(Vec::new());
        }
        let mut query: Vec<(&str, String)> = titles.iter().map(|t| ("ranges", quoted(t))).collect();
        query.push(("valueRenderOption", "UNFORMATTED_VALUE".into()));
        query.push(("majorDimension", "ROWS".into()));
        let body = self.get("/values:batchGet", &query).await?;
        let ranges = body["valueRanges"].as_array().cloned().unwrap_or_default();
        Ok(titles
            .iter()
            .enumerate()
            .map(|(i, _)| rows_of(ranges.get(i)))
            .collect())
    }

    async fn write_tabs(&self, tabs: &[TabWrite]) -> Result<()> {
        if tabs.is_empty() {
            return Ok(());
        }
        let existing = self.sheet_ids().await?;
        let missing: Vec<Value> = tabs
            .iter()
            .filter(|t| !existing.iter().any(|(title, _)| *title == t.title))
            .map(|t| json!({"addSheet": {"properties": {"title": t.title, "hidden": t.hidden}}}))
            .collect();
        if !missing.is_empty() {
            self.post(":batchUpdate", &json!({"requests": missing}))
                .await?;
        }
        let ids = self.sheet_ids().await?;
        // Merges and strict dropdowns from the previous layout would swallow
        // or reject the new values, so they go before anything is written.
        let reset = month_tab_requests(tabs, &ids, format::reset_requests);
        if !reset.is_empty() {
            self.post(":batchUpdate", &json!({"requests": reset}))
                .await?;
        }
        let ranges: Vec<String> = tabs.iter().map(|t| quoted(&t.title)).collect();
        self.post("/values:batchClear", &json!({"ranges": ranges}))
            .await?;
        let data: Vec<Value> = tabs
            .iter()
            .map(|t| json!({"range": format!("{}!A1", quoted(&t.title)), "values": cell_values(&t.cells)}))
            .collect();
        self.post(
            "/values:batchUpdate",
            &json!({"valueInputOption": "USER_ENTERED", "data": data}),
        )
        .await?;
        let look = month_tab_requests(tabs, &ids, format::month_requests);
        if !look.is_empty() {
            self.post(":batchUpdate", &json!({"requests": look}))
                .await?;
        }
        Ok(())
    }
}

/// `build`'s requests for every month tab being written.
fn month_tab_requests(
    tabs: &[TabWrite],
    ids: &[(String, i64)],
    build: fn(i64, &MonthFormat) -> Vec<Value>,
) -> Vec<Value> {
    tabs.iter()
        .filter_map(|t| {
            let id = ids.iter().find(|(title, _)| *title == t.title)?.1;
            match &t.format {
                TabFormat::Month(month) => Some(build(id, month)),
                TabFormat::Plain => None,
            }
        })
        .flatten()
        .collect()
}

fn rows_of(range: Option<&Value>) -> Vec<Vec<Value>> {
    range
        .and_then(|r| r["values"].as_array())
        .map(|rows| {
            rows.iter()
                .map(|row| row.as_array().cloned().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

fn cell_values(cells: &[Vec<Cell>]) -> Vec<Vec<Value>> {
    cells
        .iter()
        .map(|row| row.iter().map(cell_value).collect())
        .collect()
}

fn cell_value(cell: &Cell) -> Value {
    match cell {
        Cell::Empty => Value::String(String::new()),
        // The leading apostrophe keeps text as typed: USER_ENTERED would
        // otherwise read `NOVEMBRO 2026` as a date or `2/3` as a fraction.
        // Sheets hides it and reads it back without it.
        Cell::Text(text) => Value::String(format!("'{text}")),
        // The JSON number is only the wire format; the amount was rounded as
        // a Decimal and the sheet stores it as typed.
        Cell::Number(n) => serde_json::from_str(&n.to_string()).unwrap_or(Value::Null),
        Cell::Formula(formula) => Value::String(formula.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sheets_list(titles: &[(&str, i64)]) -> Value {
        json!({"sheets": titles.iter().map(|(t, id)| json!({"properties": {"title": t, "sheetId": id}})).collect::<Vec<_>>()})
    }

    #[test]
    fn text_is_written_literally_so_sheets_never_reinterprets_it() {
        assert_eq!(
            cell_value(&Cell::Text("NOVEMBRO 2026".into())),
            json!("'NOVEMBRO 2026")
        );
        assert_eq!(cell_value(&Cell::Text("2/3".into())), json!("'2/3"));
        assert_eq!(cell_value(&Cell::Text("-ajuste".into())), json!("'-ajuste"));
    }

    #[test]
    fn numbers_and_formulas_keep_their_type() {
        assert_eq!(
            cell_value(&Cell::Number(Decimal::new(113263, 2))),
            json!(1132.63)
        );
        assert_eq!(
            cell_value(&Cell::Formula("=SUM(B4:B9)".into())),
            json!("=SUM(B4:B9)")
        );
        assert_eq!(cell_value(&Cell::Empty), json!(""));
    }

    const ID: &str = "/spreadsheets/abc";

    fn api(server: &MockServer) -> HttpSheets {
        HttpSheets::with_endpoint(&format!("{}{ID}", server.uri()), "t0k").unwrap()
    }

    fn ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({}))
    }

    #[tokio::test]
    async fn reads_tabs_unformatted_in_the_order_asked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{ID}/values:batchGet")))
            .and(query_param("valueRenderOption", "UNFORMATTED_VALUE"))
            .and(header("authorization", "Bearer t0k"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"valueRanges": [
                    {"range": "'MAIO 2030'!A1:M3", "values": [["MAIO 2030"], ["Item", "Previsto"]]},
                    {"range": "'_phai'!A1:K1"}
                ]})),
            )
            .mount(&server)
            .await;
        let tabs = api(&server)
            .read_tabs(&["MAIO 2030".to_string(), "_phai".to_string()])
            .await
            .unwrap();
        assert_eq!(tabs[0][1], vec![json!("Item"), json!("Previsto")]);
        assert!(tabs[1].is_empty(), "a tab with no values reads as empty");
    }

    #[tokio::test]
    async fn write_creates_missing_tabs_then_clears_writes_and_formats() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ID))
            .respond_with(ResponseTemplate::new(200).set_body_json(sheets_list(&[("_phai", 7)])))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(ID))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(sheets_list(&[("_phai", 7), ("MAIO 2030", 11)])),
            )
            .mount(&server)
            .await;
        let batch = format!("{ID}:batchUpdate");
        Mock::given(method("POST"))
            .and(path(batch.as_str()))
            .and(body_partial_json(json!({"requests": [{"addSheet": {"properties": {"title": "MAIO 2030", "hidden": false}}}]})))
            .respond_with(ok())
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{ID}/values:batchClear")))
            .and(body_partial_json(json!({"ranges": ["'MAIO 2030'"]})))
            .respond_with(ok())
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{ID}/values:batchUpdate")))
            .and(body_partial_json(
                json!({"valueInputOption": "USER_ENTERED",
                "data": [{"range": "'MAIO 2030'!A1", "values": [["'MAIO 2030", 10]]}]}),
            ))
            .respond_with(ok())
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(batch.as_str()))
            .and(body_partial_json(
                json!({"requests": [{"unmergeCells": {"range": {"sheetId": 11}}}]}),
            ))
            .respond_with(ok())
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(batch.as_str()))
            .and(body_partial_json(
                json!({"requests": [{"repeatCell": {"range": {"sheetId": 11}}}]}),
            ))
            .respond_with(ok())
            .expect(1)
            .mount(&server)
            .await;
        api(&server)
            .write_tabs(&[TabWrite {
                title: "MAIO 2030".to_string(),
                cells: vec![vec![
                    Cell::Text("MAIO 2030".into()),
                    Cell::Number(Decimal::new(10, 0)),
                ]],
                hidden: false,
                format: TabFormat::Month(MonthFormat::default()),
            }])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn api_errors_surface_the_google_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({"error": {"message": "The caller does not have permission"}}),
            ))
            .mount(&server)
            .await;
        let err = api(&server).tab_titles().await.unwrap_err().to_string();
        assert!(
            err.contains("403") && err.contains("does not have permission"),
            "{err}"
        );
    }
}
