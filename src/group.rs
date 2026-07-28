//! Grouping and aggregation.
//!
//! A grouped query folds many records into one row per group. Records arrive
//! one at a time and the accumulators are all constant-space except
//! `count_distinct`, which has to remember what it has seen, so memory grows
//! with the number of groups rather than the number of lines.
//!
//! Groups are kept in the order their first record arrived. That makes the
//! output of a grouped query without `order by` reproducible, and it usually
//! matches the order the reader expects: the first level to appear is the
//! first row.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::ast::{Agg, Query, Selection, ValueExpr};
use crate::eval::{compare_values, eval_value, to_number};
use crate::record::Record;

/// One aggregate the query asked for, keyed by the text it was written as.
/// Two identical aggregates in the select list share a single accumulator.
#[derive(Debug, Clone)]
pub struct AggSpec {
    pub key: String,
    pub agg: Agg,
    /// `None` for `count(*)`.
    pub arg: Option<ValueExpr>,
}

/// Collect every distinct aggregate in the query's `select` and `having`.
pub fn collect_aggregates(query: &Query) -> Vec<AggSpec> {
    let mut out: Vec<AggSpec> = Vec::new();
    if let Selection::Items(items) = &query.select {
        for item in items {
            walk_value(&item.value, &mut out);
        }
    }
    if let Some(having) = &query.having {
        walk_expr(having, &mut out);
    }
    out
}

fn walk_value(v: &ValueExpr, out: &mut Vec<AggSpec>) {
    match v {
        ValueExpr::Field(_) | ValueExpr::Lit(_) => {}
        ValueExpr::Call { args, .. } => args.iter().for_each(|a| walk_value(a, out)),
        ValueExpr::Arith { left, right, .. } => {
            walk_value(left, out);
            walk_value(right, out);
        }
        ValueExpr::Agg { agg, arg } => {
            let key = v.to_string();
            if !out.iter().any(|s| s.key == key) {
                out.push(AggSpec {
                    key,
                    agg: *agg,
                    arg: arg.as_ref().map(|a| (**a).clone()),
                });
            }
        }
    }
}

fn walk_expr(e: &crate::ast::Expr, out: &mut Vec<AggSpec>) {
    use crate::ast::Expr;
    match e {
        Expr::Or(a, b) | Expr::And(a, b) => {
            walk_expr(a, out);
            walk_expr(b, out);
        }
        Expr::Not(a) => walk_expr(a, out),
        Expr::Compare { left, right, .. } => {
            walk_value(left, out);
            walk_value(right, out);
        }
        Expr::Like { left, pattern, .. } => {
            walk_value(left, out);
            walk_value(pattern, out);
        }
        Expr::Truthy(v) => walk_value(v, out),
    }
}

/// The running state of one aggregate within one group.
#[derive(Debug)]
enum State {
    Count(u64),
    Distinct(HashSet<String>),
    /// Running total and the number of values that contributed, so `sum` and
    /// `avg` share one accumulator shape.
    Total { sum: f64, n: u64 },
    Extreme(Option<Value>),
    Pick(Option<Value>),
}

impl State {
    fn new(agg: Agg) -> State {
        match agg {
            Agg::Count => State::Count(0),
            Agg::CountDistinct => State::Distinct(HashSet::new()),
            Agg::Sum | Agg::Avg => State::Total { sum: 0.0, n: 0 },
            Agg::Min | Agg::Max => State::Extreme(None),
            Agg::First | Agg::Last => State::Pick(None),
        }
    }

    /// Fold in one record's value. `value` is `None` when the record has
    /// nothing for the aggregate's argument, which every aggregate skips —
    /// `avg` over a field half the lines are missing is the average of the
    /// lines that have it, not of zeroes.
    fn push(&mut self, agg: Agg, value: Option<&Value>) {
        match self {
            State::Count(n) => {
                if agg == Agg::Count && value.is_none() {
                    return;
                }
                *n += 1;
            }
            State::Distinct(seen) => {
                if let Some(v) = value {
                    seen.insert(distinct_key(v));
                }
            }
            State::Total { sum, n } => {
                if let Some(x) = value.and_then(to_number) {
                    *sum += x;
                    *n += 1;
                }
            }
            State::Extreme(best) => {
                let Some(v) = value else { return };
                let replace = match best {
                    None => true,
                    Some(cur) => match compare_values(v, cur) {
                        Some(ord) => {
                            if agg == Agg::Min {
                                ord.is_lt()
                            } else {
                                ord.is_gt()
                            }
                        }
                        // A value that cannot be compared with the incumbent
                        // never displaces it.
                        None => false,
                    },
                };
                if replace {
                    *best = Some(v.clone());
                }
            }
            State::Pick(held) => {
                let Some(v) = value else { return };
                if agg == Agg::Last || held.is_none() {
                    *held = Some(v.clone());
                }
            }
        }
    }

    fn finish(self, agg: Agg) -> Option<Value> {
        match self {
            State::Count(n) => Some(Value::from(n)),
            State::Distinct(seen) => Some(Value::from(seen.len() as u64)),
            State::Total { sum, n } => {
                if n == 0 {
                    // No numeric value ever arrived, so there is nothing to
                    // report — an empty sum is not zero.
                    return None;
                }
                let out = if agg == Agg::Avg { sum / n as f64 } else { sum };
                serde_json::Number::from_f64(out).map(Value::Number)
            }
            State::Extreme(v) | State::Pick(v) => v,
        }
    }
}

/// A key that distinguishes values the way `count_distinct` should: by what
/// they are, not by how they were spelled.
fn distinct_key(v: &Value) -> String {
    match v {
        Value::String(s) => format!("s{s}"),
        other => format!("j{other}"),
    }
}

/// One group: its key values and one accumulator per aggregate.
struct Group {
    keys: Vec<Option<Value>>,
    states: Vec<State>,
}

/// Accumulates records into groups and renders them back out as rows.
pub struct Grouper<'q> {
    query: &'q Query,
    specs: Vec<AggSpec>,
    order: Vec<Group>,
    /// Group key text to its index in `order`, so lookup is not a scan.
    index: HashMap<String, usize>,
}

impl<'q> Grouper<'q> {
    pub fn new(query: &'q Query) -> Self {
        Grouper {
            query,
            specs: collect_aggregates(query),
            order: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Fold one record into its group, creating the group if it is new.
    pub fn accept(&mut self, record: &Record) {
        let keys: Vec<Option<Value>> = self
            .query
            .group_by
            .iter()
            .map(|k| eval_value(&k.value, record))
            .collect();

        let ident = key_identity(&keys);
        let idx = match self.index.get(&ident) {
            Some(i) => *i,
            None => {
                let i = self.order.len();
                self.order.push(Group {
                    keys,
                    states: self.specs.iter().map(|s| State::new(s.agg)).collect(),
                });
                self.index.insert(ident, i);
                i
            }
        };

        let group = &mut self.order[idx];
        for (spec, state) in self.specs.iter().zip(group.states.iter_mut()) {
            let value = match &spec.arg {
                // `count(*)` counts records, so it always has something to fold.
                None => Some(Value::Bool(true)),
                Some(arg) => eval_value(arg, record),
            };
            state.push(spec.agg, value.as_ref());
        }
    }

    /// Render the groups as records: one per group, carrying the group keys
    /// under both the expression they were written as and their label, and
    /// each aggregate under its own text. `select` and `having` are ordinary
    /// expressions over these.
    pub fn finish(self) -> Vec<Record> {
        let group_by = &self.query.group_by;
        let specs = self.specs;
        self.order
            .into_iter()
            .map(|group| {
                let mut rec = Map::new();
                for (item, value) in group_by.iter().zip(group.keys.into_iter()) {
                    let Some(value) = value else { continue };
                    rec.insert(item.value.to_string(), value.clone());
                    if item.label != item.value.to_string() {
                        rec.insert(item.label.clone(), value);
                    }
                }
                for (spec, state) in specs.iter().zip(group.states.into_iter()) {
                    if let Some(v) = state.finish(spec.agg) {
                        rec.insert(spec.key.clone(), v);
                    }
                }
                rec
            })
            .collect()
    }
}

/// Text that identifies a group key tuple. A missing key and a key that is
/// present but null are different groups, so the absent case gets its own
/// marker rather than being rendered as `null`.
fn key_identity(keys: &[Option<Value>]) -> String {
    let mut out = String::new();
    for k in keys {
        match k {
            None => out.push('\u{0}'),
            Some(v) => {
                out.push('\u{1}');
                out.push_str(&distinct_key(v));
            }
        }
        out.push('\u{2}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::record::parse_line;
    use serde_json::json;

    fn group(query: &str, lines: &[&str]) -> Vec<Record> {
        let q = parse(query).unwrap();
        let mut g = Grouper::new(&q);
        for line in lines {
            if let Some((rec, _)) = parse_line(line) {
                g.accept(&rec);
            }
        }
        g.finish()
    }

    const LOG: &[&str] = &[
        r#"{"level":"info","ms":10,"user":"ana"}"#,
        r#"{"level":"error","ms":200,"user":"bo"}"#,
        r#"{"level":"error","ms":50,"user":"ana"}"#,
        r#"{"level":"info","user":"ana"}"#,
        r#"{"level":"error","ms":"90","user":"cy"}"#,
    ];

    #[test]
    fn counts_records_per_group() {
        let rows = group("select level, count(*) group by level", LOG);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["level"], json!("info"));
        assert_eq!(rows[0]["count(*)"], json!(2));
        assert_eq!(rows[1]["level"], json!("error"));
        assert_eq!(rows[1]["count(*)"], json!(3));
    }

    #[test]
    fn groups_keep_the_order_they_first_appeared() {
        let rows = group("select level, count(*) group by level", LOG);
        let levels: Vec<&str> = rows.iter().map(|r| r["level"].as_str().unwrap()).collect();
        assert_eq!(levels, vec!["info", "error"]);
    }

    #[test]
    fn count_of_a_value_skips_records_that_lack_it() {
        let rows = group("select level, count(ms) group by level", LOG);
        assert_eq!(rows[0]["count(ms)"], json!(1));
        assert_eq!(rows[1]["count(ms)"], json!(3));
    }

    #[test]
    fn sum_and_avg_use_the_values_that_are_there() {
        let rows = group("select level, sum(ms), avg(ms) group by level", LOG);
        // info has one value, 10.
        assert_eq!(rows[0]["sum(ms)"], json!(10.0));
        assert_eq!(rows[0]["avg(ms)"], json!(10.0));
        // error has 200, 50 and the string "90", which casts to a number.
        assert_eq!(rows[1]["sum(ms)"], json!(340.0));
        assert_eq!(
            rows[1]["avg(ms)"].as_f64().unwrap(),
            340.0 / 3.0
        );
    }

    #[test]
    fn min_and_max_span_the_group() {
        let rows = group("select level, min(ms), max(ms) group by level", LOG);
        assert_eq!(rows[1]["min(ms)"], json!(50));
        assert_eq!(rows[1]["max(ms)"], json!(200));
    }

    #[test]
    fn first_and_last_pick_the_ends_of_the_group() {
        let rows = group("select level, first(user), last(user) group by level", LOG);
        assert_eq!(rows[1]["first(user)"], json!("bo"));
        assert_eq!(rows[1]["last(user)"], json!("cy"));
    }

    #[test]
    fn count_distinct_counts_values_not_records() {
        let rows = group("select level, count_distinct(user) group by level", LOG);
        assert_eq!(rows[0]["count_distinct(user)"], json!(1));
        assert_eq!(rows[1]["count_distinct(user)"], json!(3));
    }

    #[test]
    fn an_aggregate_with_no_group_by_folds_the_whole_input() {
        let rows = group("select count(*), max(ms)", LOG);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["count(*)"], json!(5));
        assert_eq!(rows[0]["max(ms)"], json!(200));
    }

    #[test]
    fn a_sum_with_nothing_numeric_is_absent_rather_than_zero() {
        let lines = [r#"{"level":"info","ms":"soon"}"#];
        let rows = group("select level, sum(ms), count(*) group by level", &lines);
        assert!(!rows[0].contains_key("sum(ms)"));
        assert_eq!(rows[0]["count(*)"], json!(1));
    }

    #[test]
    fn a_missing_key_is_its_own_group() {
        let lines = [
            r#"{"level":"info"}"#,
            r#"{"msg":"no level here"}"#,
            r#"{"level":null}"#,
        ];
        let rows = group("select level, count(*) group by level", &lines);
        assert_eq!(rows.len(), 3);
        // The record without the key has no value to print for it.
        assert!(!rows[1].contains_key("level"));
        assert_eq!(rows[2]["level"], json!(null));
    }

    #[test]
    fn values_of_different_types_do_not_share_a_group() {
        let lines = [r#"{"k":"1"}"#, r#"{"k":1}"#];
        let rows = group("select k, count(*) group by k", &lines);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn a_computed_key_groups_by_its_result() {
        let lines = [
            r#"{"level":"ERROR"}"#,
            r#"{"level":"error"}"#,
            r#"{"level":"info"}"#,
        ];
        let rows = group("select lower(level), count(*) group by lower(level)", &lines);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["lower(level)"], json!("error"));
        assert_eq!(rows[0]["count(*)"], json!(2));
    }

    #[test]
    fn a_key_alias_is_available_alongside_the_expression() {
        let lines = [r#"{"level":"ERROR"}"#];
        let q = parse("select lower(level) as lvl, count(*) group by lower(level) as lvl").unwrap();
        let mut g = Grouper::new(&q);
        let (rec, _) = parse_line(lines[0]).unwrap();
        g.accept(&rec);
        let rows = g.finish();
        assert_eq!(rows[0]["lvl"], json!("error"));
        assert_eq!(rows[0]["lower(level)"], json!("error"));
    }

    #[test]
    fn two_keys_make_a_compound_group() {
        let lines = [
            r#"{"level":"error","host":"a"}"#,
            r#"{"level":"error","host":"b"}"#,
            r#"{"level":"error","host":"a"}"#,
        ];
        let rows = group("select level, host, count(*) group by level, host", &lines);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["count(*)"], json!(2));
    }

    #[test]
    fn repeating_an_aggregate_shares_one_accumulator() {
        let q = parse("select count(*) group by level having count(*) > 1").unwrap();
        let specs = collect_aggregates(&q);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].key, "count(*)");
    }

    #[test]
    fn aggregates_are_collected_from_select_and_having() {
        let q = parse("select level, count(*) group by level having max(ms) > 100").unwrap();
        let specs = collect_aggregates(&q);
        let keys: Vec<&str> = specs.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, vec!["count(*)", "max(ms)"]);
    }
}
