//! Month-tab presentation as Sheets `batchUpdate` requests.
//!
//! Colours, fonts and number formats follow the household's own month tab
//! (Roboto 12, a dark green header, grey fixed bills, pink cards, purple
//! income, blue cash-flow block, a six-colour cycle for category blocks).
//! Every sync first clears merges, formats and dropdowns, then lays them
//! down again, so a tab never keeps the look of a previous layout.

use serde_json::{json, Value};

use super::{quoted, MonthFormat};
use crate::planning_sheet::{
    Range, Style, Validation, COLUMN_WIDTHS, ID_COLUMN, LISTS_TAB, STATUSES,
};

type Rgb = (f32, f32, f32);

const HEADER: Rgb = (0.22, 0.46, 0.11);
const LIGHT_TEXT: Rgb = (0.95, 0.95, 0.95);
const GREY: Rgb = (0.85, 0.85, 0.85);
const YELLOW: Rgb = (1.0, 0.95, 0.8);
const PINK: Rgb = (0.84, 0.65, 0.74);
const BLUE_TEXT: Rgb = (0.0, 0.0, 1.0);
const RED_FILL: Rgb = (0.92, 0.6, 0.6);
const PURPLE: Rgb = (0.71, 0.65, 0.84);
const PURPLE_LIGHT: Rgb = (0.85, 0.82, 0.91);
const FLOW: Rgb = (0.44, 0.66, 0.86);
const FLOW_LIGHT: Rgb = (0.81, 0.89, 0.95);
const RED_TEXT: Rgb = (1.0, 0.0, 0.0);
const PARCEL_TITLE: Rgb = (0.42, 0.66, 0.31);
const PARCEL_HEADER: Rgb = (0.58, 0.77, 0.49);
const PARCEL_BODY: Rgb = (0.85, 0.92, 0.83);
const PARCEL_TOTAL: Rgb = (0.71, 0.84, 0.66);
const BUDGET_TOTAL: Rgb = (0.72, 0.72, 0.72);
const NOTE_TEXT: Rgb = (0.4, 0.4, 0.4);
/// Title and body colour of each category block, in order.
const BLOCKS: [(Rgb, Rgb); 6] = [
    ((0.84, 0.65, 0.74), (0.92, 0.82, 0.86)),
    ((0.98, 0.8, 0.61), (0.99, 0.9, 0.8)),
    ((0.64, 0.77, 0.79), (0.82, 0.88, 0.89)),
    ((0.71, 0.84, 0.66), (0.85, 0.92, 0.83)),
    ((0.71, 0.65, 0.84), (0.85, 0.82, 0.91)),
    ((0.87, 0.49, 0.42), (0.9, 0.72, 0.69)),
];
const NUMBER: &str = "#,##0.00;(#,##0.00)";
const CURRENCY: &str = "[$R$ -416]#,##0.00";

fn color((red, green, blue): Rgb) -> Value {
    json!({"red": red, "green": green, "blue": blue})
}

/// A piece of `userEnteredFormat` and the field mask that writes only it.
struct Look {
    format: serde_json::Map<String, Value>,
    fields: Vec<&'static str>,
}

impl Look {
    fn new() -> Self {
        Look {
            format: serde_json::Map::new(),
            fields: Vec::new(),
        }
    }

    fn fill(mut self, rgb: Rgb) -> Self {
        self.format.insert("backgroundColor".into(), color(rgb));
        self.fields.push("userEnteredFormat.backgroundColor");
        self
    }

    fn text(mut self, key: &'static str, value: Value, field: &'static str) -> Self {
        let text = self.format.entry("textFormat").or_insert_with(|| json!({}));
        if let Some(map) = text.as_object_mut() {
            map.insert(key.into(), value);
        }
        self.fields.push(field);
        self
    }

    fn bold(self) -> Self {
        self.text("bold", json!(true), "userEnteredFormat.textFormat.bold")
    }

    fn ink(self, rgb: Rgb) -> Self {
        self.text(
            "foregroundColor",
            color(rgb),
            "userEnteredFormat.textFormat.foregroundColor",
        )
    }

    fn size(self, points: u32) -> Self {
        self.text(
            "fontSize",
            json!(points),
            "userEnteredFormat.textFormat.fontSize",
        )
    }

    fn italic(self) -> Self {
        self.text("italic", json!(true), "userEnteredFormat.textFormat.italic")
    }

    fn number(mut self, kind: &str, pattern: &str) -> Self {
        self.format.insert(
            "numberFormat".into(),
            json!({"type": kind, "pattern": pattern}),
        );
        self.fields.push("userEnteredFormat.numberFormat");
        self
    }

    fn align(mut self, how: &str) -> Self {
        self.format.insert("horizontalAlignment".into(), json!(how));
        self.fields.push("userEnteredFormat.horizontalAlignment");
        self
    }

    fn request(self, range: Value) -> Value {
        json!({"repeatCell": {
            "range": range,
            "cell": {"userEnteredFormat": Value::Object(self.format)},
            "fields": self.fields.join(","),
        }})
    }
}

fn look(style: Style) -> Look {
    table_look(style)
        .or_else(|| totals_look(style))
        .or_else(|| side_look(style))
        .or_else(|| block_look(style))
        .unwrap_or_else(Look::new)
}

/// The month table's bills, cards and income.
fn table_look(style: Style) -> Option<Look> {
    let base = Look::new();
    Some(match style {
        Style::Header => base.fill(HEADER).bold().ink(LIGHT_TEXT).align("CENTER"),
        Style::Fixed => base.fill(GREY),
        Style::Label => base.bold(),
        Style::FixedTotal => base.fill(YELLOW).bold(),
        Style::Card => base.fill(PINK).bold(),
        Style::CardMoney => base
            .number("CURRENCY", CURRENCY)
            .ink(BLUE_TEXT)
            .align("RIGHT"),
        Style::ExpenseTotal => base.fill(RED_FILL).bold(),
        Style::IncomeHeader | Style::IncomeTotal => base.fill(PURPLE).bold(),
        Style::Income => base.fill(PURPLE_LIGHT),
        _ => return None,
    })
}

/// Balance, cash flow and the number formats.
fn totals_look(style: Style) -> Option<Look> {
    let base = Look::new();
    Some(match style {
        Style::Balance => base.fill(GREY).bold(),
        Style::FlowHeader => base.fill(FLOW).bold(),
        Style::Flow => base.fill(FLOW_LIGHT),
        Style::FlowGap => base.bold().ink(RED_TEXT),
        Style::Money => base.number("NUMBER", NUMBER).align("RIGHT"),
        Style::Currency => base.number("CURRENCY", CURRENCY).align("RIGHT"),
        Style::Day => base.number("NUMBER", "#,##0").align("CENTER"),
        Style::Status => base.size(11).align("CENTER"),
        _ => return None,
    })
}

/// Installments, debit spending and the round-trip columns.
fn side_look(style: Style) -> Option<Look> {
    let base = Look::new();
    Some(match style {
        Style::ParcelTitle => base
            .fill(PARCEL_TITLE)
            .bold()
            .ink(LIGHT_TEXT)
            .align("CENTER"),
        Style::ParcelHeader => base.fill(PARCEL_HEADER).align("CENTER"),
        Style::ParcelBody => base.fill(PARCEL_BODY),
        Style::ParcelTotal => base.fill(PARCEL_TOTAL).bold(),
        Style::DebitTitle => base.fill(BUDGET_TOTAL).bold().align("CENTER"),
        Style::Meta => base.fill(PARCEL_HEADER).bold().align("CENTER"),
        Style::Note => base.italic().size(9).ink(NOTE_TEXT),
        _ => return None,
    })
}

/// Category blocks and their grand totals.
fn block_look(style: Style) -> Option<Look> {
    let base = Look::new();
    Some(match style {
        // Left so a long budget name shows its start; it spills over the block.
        Style::BlockTitle(i) => base.fill(block(i).0).bold().align("LEFT"),
        Style::BlockBody(i) => base.fill(block(i).1),
        Style::BlockTotal(i) => base.fill(block(i).0).bold(),
        Style::BudgetTotal => base.fill(BUDGET_TOTAL).bold(),
        _ => return None,
    })
}

fn block(index: u8) -> (Rgb, Rgb) {
    BLOCKS[index as usize % BLOCKS.len()]
}

fn grid_range(sheet_id: i64, range: &Range) -> Value {
    json!({"sheetId": sheet_id,
           "startRowIndex": range.rows.0, "endRowIndex": range.rows.1,
           "startColumnIndex": range.cols.0, "endColumnIndex": range.cols.1})
}

fn condition(validation: Validation) -> (Value, bool) {
    let list = |values: &[&str]| {
        json!({"type": "ONE_OF_LIST",
               "values": values.iter().map(|v| json!({"userEnteredValue": v})).collect::<Vec<_>>()})
    };
    let lists = |col: char| {
        json!({"type": "ONE_OF_RANGE",
               "values": [{"userEnteredValue": format!("={}!{col}2:{col}", quoted(LISTS_TAB))}]})
    };
    match validation {
        Validation::Status => (list(&STATUSES), true),
        Validation::Repeats => (list(&[crate::planning_sheet::MONTHLY]), false),
        Validation::Category => (lists('A'), false),
        Validation::Account => (lists('B'), false),
    }
}

/// Undo the previous layout's merges and dropdowns: sent before the values,
/// since a merged cell drops what is written into it.
pub(super) fn reset_requests(sheet_id: i64, _month: &MonthFormat) -> Vec<Value> {
    let whole = json!({"sheetId": sheet_id});
    vec![
        json!({"unmergeCells": {"range": whole}}),
        json!({"setDataValidation": {"range": whole}}),
    ]
}

pub(super) fn month_requests(sheet_id: i64, month: &MonthFormat) -> Vec<Value> {
    let whole = json!({"sheetId": sheet_id});
    let mut requests = vec![
        json!({"repeatCell": {"range": whole, "cell": {"userEnteredFormat": {}}, "fields": "userEnteredFormat"}}),
        json!({"repeatCell": {"range": whole,
            "cell": {"userEnteredFormat": {"textFormat": {"fontFamily": "Roboto", "fontSize": 12}}},
            "fields": "userEnteredFormat.textFormat.fontFamily,userEnteredFormat.textFormat.fontSize"}}),
    ];
    requests.extend(
        month
            .styles
            .iter()
            .map(|(style, range)| look(*style).request(grid_range(sheet_id, range))),
    );
    requests.extend(month.merges.iter().map(|range| {
        json!({"mergeCells": {"range": grid_range(sheet_id, range), "mergeType": "MERGE_ALL"}})
    }));
    requests.extend(month.validations.iter().map(|(validation, range)| {
        let (condition, strict) = condition(*validation);
        json!({"setDataValidation": {"range": grid_range(sheet_id, range),
            "rule": {"condition": condition, "strict": strict, "showCustomUi": true}}})
    }));
    requests.push(json!({"updateSheetProperties": {
        "properties": {"sheetId": sheet_id, "gridProperties": {"frozenRowCount": 1}, "tabColor": color(HEADER)},
        "fields": "gridProperties.frozenRowCount,tabColor"}}));
    requests.extend(COLUMN_WIDTHS.iter().enumerate().map(|(col, width)| {
        json!({"updateDimensionProperties": {
            "range": {"sheetId": sheet_id, "dimension": "COLUMNS", "startIndex": col, "endIndex": col + 1},
            "properties": {"pixelSize": width, "hiddenByUser": col == ID_COLUMN},
            "fields": "pixelSize,hiddenByUser"}})
    }));
    requests
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(requests: &[Value]) -> Vec<String> {
        requests
            .iter()
            .filter_map(|r| r.as_object()?.keys().next().cloned())
            .collect()
    }

    #[test]
    fn the_reset_undoes_merges_and_dropdowns() {
        assert_eq!(
            kinds(&reset_requests(7, &MonthFormat::default())),
            ["unmergeCells", "setDataValidation"]
        );
    }

    #[test]
    fn a_sync_clears_the_old_look_before_laying_down_the_new_one() {
        let month = MonthFormat {
            styles: vec![(
                Style::Card,
                Range {
                    rows: (5, 6),
                    cols: (0, 3),
                },
            )],
            merges: vec![Range {
                rows: (0, 1),
                cols: (6, 12),
            }],
            validations: vec![(
                Validation::Status,
                Range {
                    rows: (1, 3),
                    cols: (3, 4),
                },
            )],
        };
        let requests = month_requests(7, &month);
        assert_eq!(
            &kinds(&requests)[..6],
            &[
                "repeatCell",
                "repeatCell",
                "repeatCell",
                "mergeCells",
                "setDataValidation",
                "updateSheetProperties",
            ]
        );
        let card = &requests[2]["repeatCell"];
        assert_eq!(
            card["cell"]["userEnteredFormat"]["backgroundColor"],
            color(PINK)
        );
        assert_eq!(
            card["fields"],
            "userEnteredFormat.backgroundColor,userEnteredFormat.textFormat.bold"
        );
        let status = &requests[4]["setDataValidation"]["rule"];
        assert_eq!(status["strict"], true);
        assert_eq!(
            status["condition"]["values"][4]["userEnteredValue"],
            "Cancelado"
        );
    }

    #[test]
    fn column_widths_follow_the_template_and_the_id_column_is_hidden() {
        let requests = month_requests(7, &MonthFormat::default());
        let widths: Vec<_> = requests
            .iter()
            .filter_map(|r| r.get("updateDimensionProperties"))
            .collect();
        assert_eq!(widths.len(), COLUMN_WIDTHS.len());
        assert_eq!(widths[0]["properties"]["pixelSize"], 324);
        assert_eq!(widths[ID_COLUMN]["properties"]["hiddenByUser"], true);
    }

    #[test]
    fn category_blocks_cycle_through_the_palette() {
        assert_eq!(block(0), block(6));
        assert_ne!(block(0), block(1));
    }
}
