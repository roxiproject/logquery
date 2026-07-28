//! Query execution: filter, group, project, order, limit.
//!
//! Execution has three modes. The plainest is streaming: each record is
//! filtered and projected as it arrives, and once `limit` rows have been
//! emitted the engine reports that it is done so the caller can stop reading.
//! `order by` has to see every record before it can sort, so rows are buffered
//! until [`Engine::finish`]. A grouped query folds records into groups as they
//! arrive and produces its rows only at the end, since no group is complete
//! until the input is.

use std::cmp::Ordering;

use serde_json::{Map, Value};

use crate::ast::{OrderBy, Query, Selection};
use crate::eval::{compare_values, eval, eval_value};
use crate::group::{Grouper, Plan};
use crate::record::Record;

/// One output row: the projected subset of a record.
pub type Row = Map<String, Value>;

pub struct Engine<'q> {
    query: &'q Query,
    /// Rows held back for sorting. Empty in streaming mode.
    buffer: Vec<Row>,
    emitted: usize,
    streaming: bool,
    /// Present only for a grouped query.
    grouper: Option<Grouper<'q>>,
    /// The grouped query's clauses, rewritten to read from the folded rows.
    plan: Option<Plan>,
}

impl<'q> Engine<'q> {
    pub fn new(query: &'q Query) -> Self {
        let grouped = query.is_grouped();
        Engine {
            query,
            buffer: Vec::new(),
            emitted: 0,
            streaming: !grouped && query.order_by.is_none(),
            grouper: grouped.then(|| Grouper::new(query)),
            plan: grouped.then(|| Plan::new(query)),
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
        if let Some(grouper) = &mut self.grouper {
            grouper.accept(record);
            return None;
        }
        let row = project(&self.query.select, record);
        if self.streaming {
            self.emitted += 1;
            Some(row)
        } else {
            // Keep the sort key alongside the row so ordering can use a field
            // that was not selected.
            self.buffer
                .push(row_with_sort_key(row, record, self.query.order_by.as_ref()));
            None
        }
    }

    /// True once no further input can change the output.
    pub fn is_done(&self) -> bool {
        self.streaming && matches!(self.query.limit, Some(n) if self.emitted >= n)
    }

    /// Drain the buffered rows, sorted and limited. Empty in streaming mode.
    pub fn finish(&mut self) -> Vec<Row> {
        if let Some(grouper) = self.grouper.take() {
            let plan = self.plan.take().expect("a grouped engine has a plan");
            self.buffer = grouper
                .finish()
                .into_iter()
                .filter(|group| match &plan.having {
                    Some(cond) => eval(cond, group),
                    None => true,
                })
                .map(|group| {
                    let row = project(&plan.select, &group);
                    // The sort key of a grouped row comes from the group, so
                    // `order by count(*)` can order on a column the select
                    // list did not have to name.
                    row_with_sort_key(row, &group, plan.order_by.as_ref())
                })
                .collect();
        } else if self.streaming {
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

fn row_with_sort_key(mut row: Row, record: &Record, order_by: Option<&OrderBy>) -> Row {
    if let Some(order) = order_by {
        // The projected row is searched first so that ordering can name an
        // alias the select list introduced. A grouped record then holds its
        // columns — group keys and aggregates alike — under the text they
        // were written as. Failing both, the expression is evaluated.
        let written = order.value.to_string();
        let value = row
            .get(&written)
            .or_else(|| record.get(&written))
            .cloned()
            .or_else(|| eval_value(&order.value, record));
        if let Some(v) = value {
            row.insert(SORT_KEY.to_string(), v);
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
                // A group key is stored under the text it was written as, so
                // look for that before evaluating, which would otherwise walk
                // a dotted key as a nested path.
                let value = match record.get(&item.value.to_string()) {
                    Some(v) => Some(v.clone()),
                    None => eval_value(&item.value, record),
                };
                if let Some(v) = value {
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
    fn grouped_query_counts_by_level() {
        let rows = run("select level, count(*) as n group by level", LOG);
        assert_eq!(rows.len(), 3);
        let got: Vec<(&str, u64)> = rows
            .iter()
            .map(|r| (r["level"].as_str().unwrap(), r["n"].as_u64().unwrap()))
            .collect();
        assert_eq!(got, vec![("info", 2), ("error", 3), ("warn", 1)]);
    }

    #[test]
    fn grouped_columns_come_from_the_select_list() {
        let q = parse("select level, count(*) as n group by level").unwrap();
        assert_eq!(
            Engine::new(&q).columns(),
            Some(vec!["level".to_string(), "n".to_string()])
        );
    }

    #[test]
    fn a_grouped_engine_emits_nothing_until_it_finishes() {
        let q = parse("select level, count(*) group by level").unwrap();
        let mut e = Engine::new(&q);
        let (rec, _) = parse_line(LOG[0]).unwrap();
        assert!(e.accept(&rec).is_none());
        assert!(!e.is_done());
        assert_eq!(e.finish().len(), 1);
    }

    #[test]
    fn where_runs_before_the_grouping() {
        let rows = run(
            r#"select level, count(*) as n where duration_ms > 10 group by level"#,
            LOG,
        );
        let got: Vec<(&str, u64)> = rows
            .iter()
            .map(|r| (r["level"].as_str().unwrap(), r["n"].as_u64().unwrap()))
            .collect();
        assert_eq!(got, vec![("error", 2), ("warn", 1), ("info", 1)]);
    }

    #[test]
    fn having_filters_whole_groups() {
        let rows = run("select level, count(*) as n group by level having count(*) > 2", LOG);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["level"], json!("error"));
    }

    #[test]
    fn having_can_use_an_aggregate_the_select_list_omits() {
        let rows = run("select level group by level having max(duration_ms) > 1000", LOG);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["level"], json!("error"));
        assert_eq!(rows[0].len(), 1);
    }

    #[test]
    fn grouped_rows_order_and_limit() {
        let rows = run(
            "select level, count(*) as n group by level order by n desc limit 2",
            LOG,
        );
        let got: Vec<&str> = rows.iter().map(|r| r["level"].as_str().unwrap()).collect();
        assert_eq!(got, vec!["error", "info"]);
    }

    #[test]
    fn grouped_rows_order_by_an_aggregate_that_was_not_selected() {
        let rows = run(
            "select level group by level order by max(duration_ms) desc",
            LOG,
        );
        let got: Vec<&str> = rows.iter().map(|r| r["level"].as_str().unwrap()).collect();
        assert_eq!(got, vec!["error", "info", "warn"]);
        assert!(!rows[0].contains_key(SORT_KEY));
        assert_eq!(rows[0].len(), 1);
    }

    #[test]
    fn order_by_a_computed_value() {
        let rows = run("select msg order by len(msg) desc limit 1", LOG);
        assert_eq!(rows[0]["msg"], json!("disk write failed"));
    }

    #[test]
    fn a_bare_aggregate_folds_the_whole_input_to_one_row() {
        let rows = run("select count(*) as lines, max(duration_ms) as slowest", LOG);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["lines"], json!(6));
        assert_eq!(rows[0]["slowest"], json!(1500));
    }

    #[test]
    fn grouping_by_a_computed_value_end_to_end() {
        let rows = run(
            r#"select upper(level) as level, count(*) as n group by upper(level) having count(*) > 1"#,
            LOG,
        );
        let got: Vec<(&str, u64)> = rows
            .iter()
            .map(|r| (r["level"].as_str().unwrap(), r["n"].as_u64().unwrap()))
            .collect();
        assert_eq!(got, vec![("INFO", 2), ("ERROR", 3)]);
    }

    #[test]
    fn time_window_grouping_counts_per_window() {
        let lines = [
            r#"{"ts":"2026-07-27T10:00:10Z","level":"error"}"#,
            r#"{"ts":"2026-07-27T10:04:59Z","level":"error"}"#,
            r#"{"ts":"2026-07-27T10:05:00Z","level":"error"}"#,
            r#"{"ts":"2026-07-27T10:31:00Z","level":"info"}"#,
        ];
        let rows = run(
            "select format_time(bucket(ts, 5m)) as window, count(*) as n \
             group by bucket(ts, 5m)",
            &lines,
        );
        let got: Vec<(&str, u64)> = rows
            .iter()
            .map(|r| (r["window"].as_str().unwrap(), r["n"].as_u64().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("2026-07-27T10:00:00Z", 2),
                ("2026-07-27T10:05:00Z", 1),
                ("2026-07-27T10:30:00Z", 1),
            ]
        );
    }

    #[test]
    fn a_window_and_a_field_make_a_compound_group() {
        let lines = [
            r#"{"ts":"2026-07-27T10:00:10Z","level":"error"}"#,
            r#"{"ts":"2026-07-27T10:01:10Z","level":"info"}"#,
            r#"{"ts":"2026-07-27T10:02:10Z","level":"error"}"#,
        ];
        let rows = run(
            "select bucket(ts, 5m), level, count(*) as n group by bucket(ts, 5m), level",
            &lines,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["n"], json!(2));
        assert_eq!(rows[0]["level"], json!("error"));
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
