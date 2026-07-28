//! Query execution: filter, project, order, limit.
//!
//! Execution has two modes. Without `order by` the engine is streaming: each
//! record is filtered and projected as it arrives, and once `limit` rows have
//! been emitted the engine reports that it is done so the caller can stop
//! reading. With `order by` the engine has to see every record before it can
//! sort, so rows are buffered until [`Engine::finish`].

use std::cmp::Ordering;

use serde_json::{Map, Value};

use crate::ast::{Query, Selection};
use crate::eval::{compare_values, eval, eval_value};
use crate::record::{lookup, Record};

/// One output row: the projected subset of a record.
pub type Row = Map<String, Value>;

pub struct Engine<'q> {
    query: &'q Query,
    /// Rows held back for sorting. Empty in streaming mode.
    buffer: Vec<Row>,
    emitted: usize,
    streaming: bool,
}

impl<'q> Engine<'q> {
    pub fn new(query: &'q Query) -> Self {
        Engine {
            query,
            buffer: Vec::new(),
            emitted: 0,
            streaming: query.order_by.is_none(),
        }
    }

    /// The fixed column list, when the query names its fields. `select *`
    /// returns `None` because the columns depend on the data.
    pub fn columns(&self) -> Option<Vec<String>> {
        match &self.query.select {
            Selection::All => None,
            Selection::Items(items) => Some(items.iter().map(|i| i.label.clone()).collect()),
        }
    }

    /// Offer one record. Returns a row to print immediately, if any.
    pub fn accept(&mut self, record: &Record) -> Option<Row> {
        if self.is_done() {
            return None;
        }
        if let Some(filter) = &self.query.filter {
            if !eval(filter, record) {
                return None;
            }
        }
        let row = project(&self.query.select, record);
        if self.streaming {
            self.emitted += 1;
            Some(row)
        } else {
            // Keep the sort key alongside the row so ordering can use a field
            // that was not selected.
            self.buffer.push(row_with_sort_key(row, record, self.query));
            None
        }
    }

    /// True once no further input can change the output.
    pub fn is_done(&self) -> bool {
        self.streaming && matches!(self.query.limit, Some(n) if self.emitted >= n)
    }

    /// Drain the buffered rows, sorted and limited. Empty in streaming mode.
    pub fn finish(&mut self) -> Vec<Row> {
        if self.streaming {
            return Vec::new();
        }
        let Some(order) = &self.query.order_by else {
            return std::mem::take(&mut self.buffer);
        };
        let mut rows = std::mem::take(&mut self.buffer);
        // `sort_by` is stable, so records that tie keep their input order.
        rows.sort_by(|a, b| {
            compare_sort_keys(a.get(SORT_KEY), b.get(SORT_KEY), order.descending)
        });
        for row in &mut rows {
            row.remove(SORT_KEY);
        }
        if let Some(n) = self.query.limit {
            rows.truncate(n);
        }
        rows
    }
}

/// Key under which the sort value is stashed. The leading NUL makes a
/// collision with a real log field impossible.
const SORT_KEY: &str = "\u{0}sort";

fn row_with_sort_key(mut row: Row, record: &Record, query: &Query) -> Row {
    if let Some(order) = &query.order_by {
        if let Some(v) = lookup(record, &order.path.segments) {
            row.insert(SORT_KEY.to_string(), v.clone());
        }
    }
    row
}

/// Order two sort keys. A record missing the sort field sorts last in both
/// directions — absent data is not "the largest value", it is uninteresting.
/// Incomparable pairs tie, leaving the stable sort to preserve input order.
fn compare_sort_keys(a: Option<&Value>, b: Option<&Value>, descending: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let ord = compare_values(x, y).unwrap_or(Ordering::Equal);
            if descending {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

/// Build the output row for a record.
fn project(selection: &Selection, record: &Record) -> Row {
    match selection {
        Selection::All => record.clone(),
        Selection::Items(items) => {
            let mut out = Map::new();
            for item in items {
                if let Some(v) = eval_value(&item.value, record) {
                    out.insert(item.label.clone(), v);
                }
                // A missing value is simply absent from the row. The writers
                // render it as an empty cell.
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::record::parse_line;
    use serde_json::json;

    fn run(query: &str, lines: &[&str]) -> Vec<Row> {
        let q = parse(query).unwrap();
        let mut engine = Engine::new(&q);
        let mut out = Vec::new();
        for line in lines {
            if engine.is_done() {
                break;
            }
            if let Some((rec, _)) = parse_line(line) {
                if let Some(row) = engine.accept(&rec) {
                    out.push(row);
                }
            }
        }
        out.extend(engine.finish());
        out
    }

    const LOG: &[&str] = &[
        r#"{"ts":"2026-07-27T10:00:00Z","level":"info","msg":"server started","duration_ms":3}"#,
        r#"{"ts":"2026-07-27T10:00:01Z","level":"error","msg":"upstream timeout","duration_ms":1500,"user":{"id":7}}"#,
        r#"level=warn msg="disk 82% full" duration_ms=12"#,
        r#"{"ts":"2026-07-27T10:00:03Z","level":"error","msg":"upstream timeout","duration_ms":40,"user":{"id":9}}"#,
        r#"level=info msg="request served" duration_ms=250"#,
        r#"not a log line at all"#,
        r#"{"level":"error","msg":"disk write failed"}"#,
    ];

    #[test]
    fn select_star_returns_whole_records() {
        let rows = run("select *", LOG);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0]["msg"], json!("server started"));
    }

    #[test]
    fn select_fields_projects_in_query_order() {
        let rows = run("select msg, level", LOG);
        let keys: Vec<&str> = rows[0].keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["msg", "level"]);
    }

    #[test]
    fn where_filters_records() {
        let rows = run(r#"select msg where level = "error""#, LOG);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| !r["msg"].as_str().unwrap().is_empty()));
    }

    #[test]
    fn end_to_end_compound_filter() {
        let rows = run(
            r#"select level, msg where level = "error" and duration_ms > 100"#,
            LOG,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["msg"], json!("upstream timeout"));
    }

    #[test]
    fn filter_spans_both_input_formats() {
        let rows = run("select msg where duration_ms > 10", LOG);
        let msgs: Vec<&str> = rows.iter().map(|r| r["msg"].as_str().unwrap()).collect();
        assert_eq!(
            msgs,
            vec!["upstream timeout", "disk 82% full", "upstream timeout", "request served"]
        );
    }

    #[test]
    fn limit_caps_the_output() {
        let rows = run("select msg limit 2", LOG);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn limit_zero_emits_nothing() {
        assert!(run("select * limit 0", LOG).is_empty());
    }

    #[test]
    fn streaming_engine_stops_early() {
        let q = parse("select * limit 1").unwrap();
        let mut e = Engine::new(&q);
        assert!(!e.is_done());
        let (rec, _) = parse_line(LOG[0]).unwrap();
        assert!(e.accept(&rec).is_some());
        assert!(e.is_done());
        assert!(e.accept(&rec).is_none());
    }

    #[test]
    fn ordering_engine_is_not_done_until_finished() {
        let q = parse("select * order by level limit 1").unwrap();
        let mut e = Engine::new(&q);
        let (rec, _) = parse_line(LOG[0]).unwrap();
        assert!(e.accept(&rec).is_none());
        assert!(!e.is_done());
        assert_eq!(e.finish().len(), 1);
    }

    #[test]
    fn order_by_ascending_and_descending() {
        let asc = run("select duration_ms order by duration_ms", LOG);
        let got: Vec<f64> = asc
            .iter()
            .filter_map(|r| r.get("duration_ms").and_then(|v| v.as_f64()))
            .collect();
        assert_eq!(got, vec![3.0, 12.0, 40.0, 250.0, 1500.0]);

        let desc = run("select duration_ms order by duration_ms desc", LOG);
        let got: Vec<f64> = desc
            .iter()
            .filter_map(|r| r.get("duration_ms").and_then(|v| v.as_f64()))
            .collect();
        assert_eq!(got, vec![1500.0, 250.0, 40.0, 12.0, 3.0]);
    }

    #[test]
    fn order_by_a_field_that_was_not_selected() {
        let rows = run("select msg order by duration_ms desc limit 1", LOG);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["msg"], json!("upstream timeout"));
        assert!(!rows[0].contains_key(SORT_KEY));
        assert_eq!(rows[0].len(), 1);
    }

    #[test]
    fn records_missing_the_sort_field_go_last_in_both_directions() {
        for q in [
            "select level, duration_ms order by duration_ms",
            "select level, duration_ms order by duration_ms desc",
        ] {
            let rows = run(q, LOG);
            assert!(!rows.last().unwrap().contains_key("duration_ms"), "{q}");
        }
    }

    #[test]
    fn order_then_limit_applies_after_sorting() {
        let rows = run("select duration_ms order by duration_ms desc limit 2", LOG);
        let got: Vec<f64> = rows
            .iter()
            .map(|r| r["duration_ms"].as_f64().unwrap())
            .collect();
        assert_eq!(got, vec![1500.0, 250.0]);
    }

    #[test]
    fn dotted_projection_uses_the_written_path_as_the_column_name() {
        let rows = run("select user.id where user.id > 0", LOG);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["user.id"], json!(7));
    }

    #[test]
    fn missing_projected_field_is_absent_from_the_row() {
        let rows = run(r#"select ts, msg where msg = "disk write failed""#, LOG);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].contains_key("ts"));
        assert!(rows[0].contains_key("msg"));
    }

    #[test]
    fn like_query_end_to_end() {
        let rows = run(r#"select msg where msg like "%timeout%""#, LOG);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn columns_reflect_the_selection() {
        let q = parse("select a, b.c").unwrap();
        assert_eq!(
            Engine::new(&q).columns(),
            Some(vec!["a".to_string(), "b.c".to_string()])
        );
        let q = parse("select *").unwrap();
        assert_eq!(Engine::new(&q).columns(), None);
    }

    #[test]
    fn ordering_is_stable_for_ties() {
        let lines = [
            r#"{"k":1,"id":"a"}"#,
            r#"{"k":1,"id":"b"}"#,
            r#"{"k":1,"id":"c"}"#,
        ];
        let rows = run("select id order by k", &lines);
        let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
