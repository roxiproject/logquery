//! Expression evaluation and the type coercion rules.
//!
//! Logs are not a typed database. A field can be a number in one line and a
//! string in the next, and it can be absent entirely. The rules below are
//! chosen so that a query never panics and never silently matches something
//! surprising. They are documented verbatim in the README.
//!
//! **Missing fields.** A path that does not resolve is *missing*, which is not
//! the same as a JSON `null`. Any comparison involving a missing operand is
//! `false` — including `!=`. To test for presence, use the bare-field form:
//! `where err` is true when `err` is present and truthy, and `where not err`
//! is true when it is missing, null, or falsy.
//!
//! **Comparison.** Both sides are reduced to a comparable pair:
//!
//! * number vs number — numeric.
//! * string vs string — byte-wise lexicographic.
//! * bool vs bool — `false < true`.
//! * null vs null — equal; `<`, `<=`, `>`, `>=` are false.
//! * string vs number — the string is parsed as a number. If it parses, the
//!   comparison is numeric; if not, the pair is incomparable.
//! * bool vs number — the bool becomes `1` or `0`.
//! * bool vs string — the string is read as `true`/`false` (case-insensitive).
//! * null vs anything else — incomparable.
//! * array/object vs anything — equality only, by structural equality;
//!   ordering is incomparable.
//!
//! An incomparable pair makes `=` false, `!=` true, and every ordering
//! operator false.
//!
//! **Truthiness** (a bare field or literal used as a condition): missing and
//! null are false; a bool is itself; a number is true when non-zero (`NaN` is
//! false); a string is true when non-empty; arrays and objects are true when
//! non-empty.

use std::cmp::Ordering;

use serde_json::Value;

use crate::ast::{CmpOp, Expr, Literal, Operand};
use crate::record::{lookup, Record};

/// The result of resolving an operand against a record.
#[derive(Debug, Clone, PartialEq)]
enum Resolved<'a> {
    /// The path did not resolve.
    Missing,
    Val(&'a Value),
    /// A literal from the query, which has no home in the record.
    Owned(Value),
}

impl Resolved<'_> {
    fn value(&self) -> Option<&Value> {
        match self {
            Resolved::Missing => None,
            Resolved::Val(v) => Some(v),
            Resolved::Owned(v) => Some(v),
        }
    }
}

fn resolve<'a>(operand: &'a Operand, record: &'a Record) -> Resolved<'a> {
    match operand {
        Operand::Field(path) => match lookup(record, &path.segments) {
            Some(v) => Resolved::Val(v),
            None => Resolved::Missing,
        },
        Operand::Lit(lit) => Resolved::Owned(match lit {
            Literal::Str(s) => Value::String(s.clone()),
            Literal::Num(n) => serde_json::Number::from_f64(*n)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Null => Value::Null,
        }),
    }
}

/// Evaluate a filter expression against one record.
pub fn eval(expr: &Expr, record: &Record) -> bool {
    match expr {
        Expr::Or(a, b) => eval(a, record) || eval(b, record),
        Expr::And(a, b) => eval(a, record) && eval(b, record),
        Expr::Not(a) => !eval(a, record),
        Expr::Compare { op, left, right } => {
            compare(*op, &resolve(left, record), &resolve(right, record))
        }
        Expr::Like {
            left,
            pattern,
            negated,
        } => {
            let l = resolve(left, record);
            let p = resolve(pattern, record);
            let (Some(lv), Some(pv)) = (l.value(), p.value()) else {
                // Missing operand: false regardless of negation.
                return false;
            };
            let (Some(text), Some(pat)) = (as_text(lv), as_text(pv)) else {
                return false;
            };
            let m = like_match(&text, &pat);
            if *negated {
                !m
            } else {
                m
            }
        }
        Expr::Truthy(operand) => truthy(&resolve(operand, record)),
    }
}

fn truthy(r: &Resolved<'_>) -> bool {
    match r.value() {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn compare(op: CmpOp, left: &Resolved<'_>, right: &Resolved<'_>) -> bool {
    let (Some(l), Some(r)) = (left.value(), right.value()) else {
        // Missing on either side: never matches, for every operator.
        return false;
    };
    match compare_values(l, r) {
        Some(ord) => match op {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        },
        None => matches!(op, CmpOp::Ne),
    }
}

/// Order two values under the coercion rules, or `None` if incomparable.
///
/// Also used by `order by`, so that sorting and filtering agree.
pub fn compare_values(l: &Value, r: &Value) -> Option<Ordering> {
    use Value::*;
    match (l, r) {
        (Null, Null) => Some(Ordering::Equal),
        (Null, _) | (_, Null) => None,

        (Number(a), Number(b)) => cmp_f64(a.as_f64()?, b.as_f64()?),
        (String(a), String(b)) => Some(a.as_str().cmp(b.as_str())),
        (Bool(a), Bool(b)) => Some(a.cmp(b)),

        (String(s), Number(n)) => cmp_f64(parse_number(s)?, n.as_f64()?),
        (Number(n), String(s)) => cmp_f64(n.as_f64()?, parse_number(s)?),

        (Bool(b), Number(n)) => cmp_f64(bool_as_f64(*b), n.as_f64()?),
        (Number(n), Bool(b)) => cmp_f64(n.as_f64()?, bool_as_f64(*b)),

        (Bool(b), String(s)) => Some(b.cmp(&parse_bool(s)?)),
        (String(s), Bool(b)) => Some(parse_bool(s)?.cmp(b)),

        // Composites compare by structural equality only.
        (Array(_), Array(_)) | (Object(_), Object(_)) => {
            if l == r {
                Some(Ordering::Equal)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn cmp_f64(a: f64, b: f64) -> Option<Ordering> {
    // NaN is incomparable with everything, itself included.
    a.partial_cmp(&b)
}

fn bool_as_f64(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // Reject the textual float spellings so that `msg = 1` cannot match a
    // message that happens to read "inf".
    let lower = t.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "nan" | "inf" | "-inf" | "+inf" | "infinity" | "-infinity" | "+infinity"
    ) {
        return None;
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite())
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Render a value as text for `like`. Composites are not matched.
fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => None,
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// SQL `like` matching. `%` matches any run of characters including none; a
/// `%` preceded by a backslash is a literal percent sign. Matching is
/// case-sensitive and anchored at both ends, so `like "error"` is equality.
pub fn like_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let segments = split_pattern(pattern);
    match_segments(&text, &segments)
}

/// A pattern compiles to alternating literal chunks and wildcards.
#[derive(Debug, PartialEq)]
enum Seg {
    Lit(Vec<char>),
    Any,
}

fn split_pattern(pattern: &str) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    let mut lit: Vec<char> = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('%') | Some('\\')) => {
                lit.push(chars.next().expect("peeked"));
            }
            '%' => {
                if !lit.is_empty() {
                    out.push(Seg::Lit(std::mem::take(&mut lit)));
                }
                // Collapse runs of `%`.
                if out.last() != Some(&Seg::Any) {
                    out.push(Seg::Any);
                }
            }
            c => lit.push(c),
        }
    }
    if !lit.is_empty() {
        out.push(Seg::Lit(lit));
    }
    out
}

/// Greedy backtrack-free matcher: anchor the leading literal, anchor the
/// trailing literal, then scan forward for the middle ones.
fn match_segments(text: &[char], segs: &[Seg]) -> bool {
    // A pattern with no wildcard is exact equality.
    if !segs.iter().any(|s| matches!(s, Seg::Any)) {
        let mut flat: Vec<char> = Vec::new();
        for s in segs {
            if let Seg::Lit(lit) = s {
                flat.extend_from_slice(lit);
            }
        }
        return text == flat.as_slice();
    }
    let mut start = 0usize;
    let mut end = text.len();
    let mut segs = segs;

    // Leading literal must match at position 0.
    if let Some(Seg::Lit(lit)) = segs.first() {
        if text.len() < lit.len() || &text[..lit.len()] != lit.as_slice() {
            return false;
        }
        start = lit.len();
        segs = &segs[1..];
    }
    // Trailing literal must match at the end.
    if let Some(Seg::Lit(lit)) = segs.last() {
        if end.saturating_sub(start) < lit.len() || &text[end - lit.len()..] != lit.as_slice() {
            return false;
        }
        end -= lit.len();
        segs = &segs[..segs.len() - 1];
    }
    // What remains is `%` separated literals, all of which must appear in
    // order somewhere inside the window. Because each is bounded by wildcards,
    // taking the earliest occurrence is always safe.
    let mut pos = start;
    for seg in segs {
        let Seg::Lit(lit) = seg else { continue };
        match find_from(&text[..end], pos, lit) {
            Some(at) => pos = at + lit.len(),
            None => return false,
        }
    }
    true
}

fn find_from(text: &[char], from: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return Some(from);
    }
    if text.len() < needle.len() {
        return None;
    }
    (from..=text.len() - needle.len()).find(|&i| &text[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_expr;
    use serde_json::json;

    fn rec(v: Value) -> Record {
        v.as_object().unwrap().clone()
    }

    fn check(expr: &str, record: &Value) -> bool {
        let e = parse_expr(expr).unwrap();
        eval(&e, &rec(record.clone()))
    }

    // --- comparison operators -------------------------------------------

    #[test]
    fn all_operators_on_numbers() {
        let r = json!({"n": 10});
        assert!(check("n = 10", &r));
        assert!(!check("n = 11", &r));
        assert!(check("n != 11", &r));
        assert!(!check("n != 10", &r));
        assert!(check("n < 11", &r));
        assert!(!check("n < 10", &r));
        assert!(check("n <= 10", &r));
        assert!(check("n > 9", &r));
        assert!(!check("n > 10", &r));
        assert!(check("n >= 10", &r));
    }

    #[test]
    fn all_operators_on_strings() {
        let r = json!({"s": "beta"});
        assert!(check(r#"s = "beta""#, &r));
        assert!(check(r#"s != "alpha""#, &r));
        assert!(check(r#"s > "alpha""#, &r));
        assert!(check(r#"s < "gamma""#, &r));
        assert!(check(r#"s >= "beta""#, &r));
        assert!(check(r#"s <= "beta""#, &r));
    }

    #[test]
    fn integers_and_floats_compare_numerically() {
        let r = json!({"a": 2, "b": 2.0, "c": 2.5});
        assert!(check("a = b", &r));
        assert!(check("c > a", &r));
    }

    #[test]
    fn booleans_order_false_before_true() {
        let r = json!({"t": true, "f": false});
        assert!(check("t > f", &r));
        assert!(check("f < t", &r));
        assert!(check("t = true", &r));
        assert!(check("f != true", &r));
    }

    #[test]
    fn fields_can_be_compared_to_fields() {
        let r = json!({"a": 5, "b": 3});
        assert!(check("a > b", &r));
        assert!(!check("a < b", &r));
    }

    #[test]
    fn literal_on_the_left_works() {
        let r = json!({"d": 250});
        assert!(check("100 < d", &r));
    }

    // --- missing fields --------------------------------------------------

    #[test]
    fn every_operator_is_false_on_a_missing_field() {
        let r = json!({"level": "info"});
        for expr in [
            "missing = 1",
            "missing != 1",
            "missing < 1",
            "missing <= 1",
            "missing > 1",
            "missing >= 1",
            r#"missing = "x""#,
            "missing = null",
            "missing != null",
            "missing = missing",
            "missing != missing",
        ] {
            assert!(!check(expr, &r), "{expr} should be false");
        }
    }

    #[test]
    fn missing_nested_path_is_false_not_a_panic() {
        let r = json!({"user": {"id": 1}});
        assert!(!check("user.name.first = \"ada\"", &r));
        assert!(!check("user.id.x = 1", &r));
        assert!(!check("nope.nope.nope > 0", &r));
    }

    #[test]
    fn a_missing_field_is_falsy_so_not_tests_absence() {
        let r = json!({"level": "info"});
        assert!(check("not err", &r));
        assert!(!check("err", &r));
    }

    #[test]
    fn negating_a_comparison_on_a_missing_field_is_true() {
        // `not (missing = 1)` is the negation of false. This is deliberate and
        // is why absence checks should use the bare-field form.
        let r = json!({});
        assert!(check("not (missing = 1)", &r));
    }

    // --- null ------------------------------------------------------------

    #[test]
    fn null_equals_null() {
        let r = json!({"err": null});
        assert!(check("err = null", &r));
        assert!(!check("err != null", &r));
    }

    #[test]
    fn null_is_incomparable_with_other_types() {
        let r = json!({"err": null, "n": 1, "s": "x"});
        assert!(!check("err = n", &r));
        assert!(check("err != n", &r));
        assert!(!check("err < n", &r));
        assert!(!check("err > n", &r));
        assert!(!check("err = s", &r));
        assert!(!check("n = null", &r));
        assert!(check("n != null", &r));
    }

    #[test]
    fn present_null_is_falsy() {
        let r = json!({"err": null});
        assert!(!check("err", &r));
        assert!(check("not err", &r));
    }

    // --- cross-type coercion ---------------------------------------------

    #[test]
    fn numeric_strings_compare_as_numbers() {
        let r = json!({"code": "500", "dur": " 12.5 "});
        assert!(check("code = 500", &r));
        assert!(check("code > 499", &r));
        assert!(check("dur < 13", &r));
    }

    #[test]
    fn non_numeric_string_versus_number_is_incomparable() {
        let r = json!({"s": "error"});
        assert!(!check("s = 1", &r));
        assert!(check("s != 1", &r));
        assert!(!check("s < 1", &r));
        assert!(!check("s > 1", &r));
        assert!(!check("s >= 1", &r));
    }

    #[test]
    fn textual_infinity_and_nan_do_not_become_numbers() {
        let r = json!({"a": "inf", "b": "NaN", "c": "infinity"});
        assert!(!check("a > 0", &r));
        assert!(!check("b = 0", &r));
        assert!(!check("c > 0", &r));
    }

    #[test]
    fn empty_string_is_not_a_number() {
        let r = json!({"s": ""});
        assert!(!check("s = 0", &r));
        assert!(check("s != 0", &r));
    }

    #[test]
    fn two_strings_compare_lexicographically_even_when_numeric() {
        // "10" < "9" as text. Both sides are strings, so no number coercion.
        let r = json!({"a": "10", "b": "9"});
        assert!(check("a < b", &r));
    }

    #[test]
    fn bool_versus_number_coerces_to_one_and_zero() {
        let r = json!({"t": true, "f": false});
        assert!(check("t = 1", &r));
        assert!(check("f = 0", &r));
        assert!(check("t > 0", &r));
        assert!(!check("t = 2", &r));
    }

    #[test]
    fn bool_versus_string_reads_true_and_false() {
        let r = json!({"t": true});
        assert!(check(r#"t = "true""#, &r));
        assert!(check(r#"t = "TRUE""#, &r));
        assert!(!check(r#"t = "yes""#, &r));
        assert!(check(r#"t != "yes""#, &r));
    }

    #[test]
    fn composites_support_equality_only() {
        let r = json!({"tags": ["a", "b"], "meta": {"k": 1}});
        assert!(!check("tags = 1", &r));
        assert!(!check("tags > 1", &r));
        assert!(!check("meta = 1", &r));
        assert!(check("tags = tags", &r));
        assert!(!check("tags != tags", &r));
        assert!(!check("tags = meta", &r));
    }

    // --- truthiness -------------------------------------------------------

    #[test]
    fn truthiness_of_each_type() {
        let r = json!({
            "t": true, "f": false,
            "one": 1, "zero": 0,
            "s": "x", "empty": "",
            "arr": [1], "arr0": [],
            "obj": {"a": 1}, "obj0": {},
            "nil": null
        });
        for yes in ["t", "one", "s", "arr", "obj"] {
            assert!(check(yes, &r), "{yes} should be truthy");
        }
        for no in ["f", "zero", "empty", "arr0", "obj0", "nil", "absent"] {
            assert!(!check(no, &r), "{no} should be falsy");
        }
    }

    #[test]
    fn literal_conditions_are_evaluated() {
        let r = json!({});
        assert!(check("true", &r));
        assert!(!check("false", &r));
        assert!(!check("null", &r));
        assert!(check("1", &r));
        assert!(!check("0", &r));
    }

    // --- boolean structure -----------------------------------------------

    #[test]
    fn and_or_not_combine_correctly() {
        let r = json!({"level": "error", "dur": 250});
        assert!(check(r#"level = "error" and dur > 100"#, &r));
        assert!(!check(r#"level = "warn" and dur > 100"#, &r));
        assert!(check(r#"level = "warn" or dur > 100"#, &r));
        assert!(check(r#"not level = "warn""#, &r));
    }

    #[test]
    fn precedence_holds_at_evaluation_time() {
        let r = json!({"a": 1, "b": 0, "c": 1});
        // a or (b and c) => true
        assert!(check("a = 1 or b = 1 and c = 1", &r));
        // (a or b) and c => a=2 fails, b=1 fails => false
        assert!(!check("(a = 2 or b = 1) and c = 1", &r));
    }

    // --- like -------------------------------------------------------------

    #[test]
    fn like_matches_prefix_suffix_and_infix() {
        let r = json!({"msg": "connection timeout after 30s"});
        assert!(check(r#"msg like "connection%""#, &r));
        assert!(check(r#"msg like "%30s""#, &r));
        assert!(check(r#"msg like "%timeout%""#, &r));
        assert!(check(r#"msg like "conn%after%s""#, &r));
        assert!(!check(r#"msg like "%refused%""#, &r));
    }

    #[test]
    fn like_without_wildcards_is_equality() {
        let r = json!({"s": "abc"});
        assert!(check(r#"s like "abc""#, &r));
        assert!(!check(r#"s like "ab""#, &r));
    }

    #[test]
    fn like_is_case_sensitive() {
        let r = json!({"s": "Error"});
        assert!(!check(r#"s like "%error%""#, &r));
        assert!(check(r#"s like "%Error%""#, &r));
    }

    #[test]
    fn not_like_negates() {
        let r = json!({"msg": "GET /healthz"});
        assert!(check(r#"msg not like "%timeout%""#, &r));
        assert!(!check(r#"msg not like "%health%""#, &r));
        assert!(check(r#"not msg like "%timeout%""#, &r));
    }

    #[test]
    fn like_on_a_missing_field_is_false_both_ways() {
        let r = json!({});
        assert!(!check(r#"absent like "%x%""#, &r));
        assert!(!check(r#"absent not like "%x%""#, &r));
    }

    #[test]
    fn like_coerces_numbers_and_bools_to_text() {
        let r = json!({"code": 503, "ok": false});
        assert!(check(r#"code like "5%""#, &r));
        assert!(check(r#"ok like "fal%""#, &r));
    }

    #[test]
    fn like_on_null_or_composite_is_false() {
        let r = json!({"n": null, "a": [1]});
        assert!(!check(r#"n like "%""#, &r));
        assert!(!check(r#"a like "%""#, &r));
    }

    #[test]
    fn escaped_percent_is_a_literal() {
        let r = json!({"a": "100% done", "b": "100 done"});
        assert!(check(r#"a like "100\%%""#, &r));
        assert!(!check(r#"b like "100\%%""#, &r));
    }

    #[test]
    fn like_direct_matcher_edge_cases() {
        assert!(like_match("", "%"));
        assert!(like_match("", ""));
        assert!(!like_match("a", ""));
        assert!(like_match("abc", "%%%"));
        assert!(like_match("abc", "a%c"));
        assert!(!like_match("ac", "a%b%c"));
        assert!(like_match("aXbYc", "a%b%c"));
        assert!(!like_match("abc", "abcd"));
        assert!(!like_match("abcd", "abc"));
        assert!(!like_match("abc", "ab"));
        assert!(like_match("aaa", "%aa"));
        assert!(like_match("ab", "ab%"));
    }

    #[test]
    fn like_handles_multibyte_text() {
        assert!(like_match("héllo wörld", "%wörld"));
        assert!(like_match("日本語ログ", "%ログ"));
        assert!(!like_match("日本語", "%ログ"));
    }

    #[test]
    fn like_pattern_may_come_from_a_field() {
        let r = json!({"msg": "abc", "pat": "a%"});
        assert!(check("msg like pat", &r));
    }

    // --- dotted paths -----------------------------------------------------

    #[test]
    fn dotted_paths_are_usable_in_comparisons() {
        let r = json!({"user": {"id": 42, "name": "ada"}, "http": {"status": 500}});
        assert!(check("user.id = 42", &r));
        assert!(check(r#"user.name like "a%""#, &r));
        assert!(check("http.status >= 500", &r));
        assert!(check("user.id < http.status", &r));
    }

    #[test]
    fn array_indices_work_in_paths() {
        let r = json!({"items": [{"id": 1}, {"id": 2}]});
        assert!(check("items.1.id = 2", &r));
        assert!(!check("items.5.id = 2", &r));
    }

    // --- compare_values directly -----------------------------------------

    #[test]
    fn compare_values_reports_incomparability() {
        assert_eq!(compare_values(&json!(1), &json!(2)), Some(Ordering::Less));
        assert_eq!(compare_values(&json!("a"), &json!(1)), None);
        assert_eq!(compare_values(&Value::Null, &json!(1)), None);
        assert_eq!(
            compare_values(&Value::Null, &Value::Null),
            Some(Ordering::Equal)
        );
    }
}
