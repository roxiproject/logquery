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

use crate::ast::{ArithOp, CmpOp, Expr, Func, Literal, ValueExpr};
use crate::record::{lookup, Record};
use crate::timeutil;

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

fn resolve<'a>(expr: &'a ValueExpr, record: &'a Record) -> Resolved<'a> {
    match expr {
        ValueExpr::Field(path) => match lookup(record, &path.segments) {
            Some(v) => Resolved::Val(v),
            None => Resolved::Missing,
        },
        ValueExpr::Lit(lit) => Resolved::Owned(literal_value(lit)),
        ValueExpr::Call { func, args } => call(*func, args, record),
        ValueExpr::Arith { op, left, right } => arith(*op, left, right, record),
        // An aggregate cannot be computed from one record. The engine folds
        // each group first and stores the result under the aggregate's own
        // text, so by the time `select` and `having` are evaluated the value
        // is an ordinary field of the grouped row.
        ValueExpr::Agg { .. } => match record.get(&expr.to_string()) {
            Some(v) => Resolved::Owned(v.clone()),
            None => Resolved::Missing,
        },
    }
}

/// Add or subtract two values. Both sides are cast to numbers the way `num`
/// casts them, so `ts(t) > now() - 15m` works whether the log wrote its
/// timestamp as a number or as text. A side that is not numeric is missing,
/// and missing propagates.
fn arith<'a>(op: ArithOp, left: &ValueExpr, right: &ValueExpr, record: &Record) -> Resolved<'a> {
    let (Some(a), Some(b)) = (
        resolve(left, record).value().and_then(to_number),
        resolve(right, record).value().and_then(to_number),
    ) else {
        return Resolved::Missing;
    };
    Resolved::Owned(number(match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
    }))
}

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::String(s.clone()),
        Literal::Num(n) => number(*n),
        // A duration is a number of seconds; the unit only existed to spell it.
        Literal::Duration(secs) => number(*secs),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

/// Wrap an `f64` as a JSON number, or `null` when it is not finite. Nothing
/// downstream has to worry about a `NaN` sneaking into a record.
fn number(n: f64) -> Value {
    serde_json::Number::from_f64(n)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Evaluate a value expression against a record.
///
/// `None` means the value is *missing* — a path that did not resolve, or a
/// function whose input was not of a usable shape. Missing propagates: a
/// function of a missing value is missing, and so a comparison against it is
/// false rather than an error.
pub fn eval_value(expr: &ValueExpr, record: &Record) -> Option<Value> {
    resolve(expr, record).value().cloned()
}

/// Apply a scalar function.
fn call<'a>(func: Func, args: &'a [ValueExpr], record: &'a Record) -> Resolved<'a> {
    // `coalesce` and `now` do not want their arguments reduced up front.
    match func {
        Func::Now => return Resolved::Owned(number(timeutil::now_epoch())),
        Func::Coalesce => {
            for a in args {
                let r = resolve(a, record);
                if !matches!(r.value(), None | Some(Value::Null)) {
                    return r;
                }
            }
            return Resolved::Missing;
        }
        Func::Concat => {
            // Joins whatever is there. A missing or null argument contributes
            // nothing, so `concat(level, ": ", msg)` still reads well when one
            // side is absent. All arguments missing is itself missing.
            let mut out = String::new();
            let mut any = false;
            for a in args {
                if let Some(t) = resolve(a, record).value().and_then(as_text) {
                    out.push_str(&t);
                    any = true;
                }
            }
            return if any {
                Resolved::Owned(Value::String(out))
            } else {
                Resolved::Missing
            };
        }
        _ => {}
    }

    let Some(first) = resolve(&args[0], record).value().cloned() else {
        return Resolved::Missing;
    };

    let out = match func {
        Func::Lower => as_text(&first).map(|t| Value::String(t.to_lowercase())),
        Func::Upper => as_text(&first).map(|t| Value::String(t.to_uppercase())),
        Func::Len => match &first {
            Value::String(s) => Some(number(s.chars().count() as f64)),
            Value::Array(a) => Some(number(a.len() as f64)),
            Value::Object(o) => Some(number(o.len() as f64)),
            _ => None,
        },
        Func::Num => to_number(&first).map(number),
        Func::DurationMs => to_duration_ms(&first).map(number),
        Func::Trim => as_text(&first).map(|t| Value::String(t.trim().to_string())),
        Func::Abs => to_number(&first).map(|n| number(n.abs())),
        Func::Ts => to_epoch(&first).map(number),
        Func::FormatTime => {
            let Some(secs) = to_epoch(&first) else {
                return Resolved::Missing;
            };
            // The optional second argument names the zone. Anything that is
            // not a recognised zone is a value the query cannot honour, so the
            // result is missing rather than silently rendered as UTC.
            let offset = match args.get(1) {
                None => 0,
                Some(arg) => {
                    let Some(spec) = resolve(arg, record).value().and_then(as_text) else {
                        return Resolved::Missing;
                    };
                    match timeutil::parse_tz(&spec) {
                        Some(o) => o,
                        None => return Resolved::Missing,
                    }
                }
            };
            Some(Value::String(timeutil::format_epoch(secs, offset)))
        }
        Func::Floor => to_number(&first).map(|n| number(n.floor())),
        Func::Ceil => to_number(&first).map(|n| number(n.ceil())),
        Func::Round => {
            let Some(n) = to_number(&first) else {
                return Resolved::Missing;
            };
            // The optional second argument is the number of decimal places.
            let places = match args.get(1) {
                None => 0.0,
                Some(arg) => match resolve(arg, record).value().and_then(to_number) {
                    Some(p) => p.trunc().clamp(0.0, 15.0),
                    None => return Resolved::Missing,
                },
            };
            let scale = 10f64.powf(places);
            Some(number((n * scale).round() / scale))
        }
        Func::Contains | Func::StartsWith | Func::EndsWith => {
            let (Some(hay), Some(needle)) = (as_text(&first), text_arg(args, 1, record)) else {
                return Resolved::Missing;
            };
            Some(Value::Bool(match func {
                Func::Contains => hay.contains(&needle),
                Func::StartsWith => hay.starts_with(&needle),
                _ => hay.ends_with(&needle),
            }))
        }
        Func::Replace => {
            let (Some(text), Some(from), Some(to)) = (
                as_text(&first),
                text_arg(args, 1, record),
                text_arg(args, 2, record),
            ) else {
                return Resolved::Missing;
            };
            // Replacing the empty string would splice the replacement between
            // every character, which is never what a log query means.
            if from.is_empty() {
                Some(Value::String(text))
            } else {
                Some(Value::String(text.replace(&from, &to)))
            }
        }
        Func::Substr => {
            let Some(text) = as_text(&first) else {
                return Resolved::Missing;
            };
            let chars: Vec<char> = text.chars().collect();
            let start = resolve(&args[1], record)
                .value()
                .and_then(to_number)
                .map(|n| n.trunc());
            let Some(start) = start else {
                return Resolved::Missing;
            };
            // A negative start counts back from the end, as in most scripting
            // languages; indices are 0-based.
            let begin = if start < 0.0 {
                (chars.len() as f64 + start).max(0.0) as usize
            } else {
                (start as usize).min(chars.len())
            };
            let end = match args.get(2) {
                None => chars.len(),
                Some(arg) => {
                    let Some(len) = resolve(arg, record).value().and_then(to_number) else {
                        return Resolved::Missing;
                    };
                    if len <= 0.0 {
                        begin
                    } else {
                        (begin + len.trunc() as usize).min(chars.len())
                    }
                }
            };
            Some(Value::String(chars[begin..end].iter().collect()))
        }
        Func::Now | Func::Coalesce | Func::Concat => unreachable!("handled above"),
    };

    match out {
        Some(v) => Resolved::Owned(v),
        None => Resolved::Missing,
    }
}

/// Timestamp cast to epoch seconds. A number is already epoch seconds, except
/// that a value large enough to be milliseconds is scaled down — no log is
/// from the year 33658, so a 13-digit timestamp is milliseconds. Text is read
/// as RFC 3339.
fn to_epoch(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().map(rescale_epoch),
        Value::String(s) => {
            let t = s.trim();
            timeutil::parse_rfc3339(t).or_else(|| parse_number(t).map(rescale_epoch))
        }
        _ => None,
    }
}

/// Milliseconds since 1970 above this are seconds no calendar cares about.
const EPOCH_MS_THRESHOLD: f64 = 100_000_000_000.0;

fn rescale_epoch(n: f64) -> f64 {
    if n.abs() >= EPOCH_MS_THRESHOLD {
        n / 1000.0
    } else {
        n
    }
}

/// Reduce argument `i` to text, or `None` when it is absent or has no textual
/// form.
fn text_arg(args: &[ValueExpr], i: usize, record: &Record) -> Option<String> {
    resolve(args.get(i)?, record).value().and_then(as_text)
}

/// Numeric cast. Numbers pass through, booleans become `1`/`0`, and a string is
/// read as a number with an optional unit suffix, so `"120ms"` is `120` and
/// `"1.5"` is `1.5`. Anything else is missing.
fn to_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim();
            if let Some(n) = parse_number(t) {
                return Some(n);
            }
            // Strip a trailing unit: `120ms`, `4.5s`, `12KB`.
            let head: String = t
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
                .collect();
            parse_number(&head)
        }
        _ => None,
    }
}

/// Duration cast to milliseconds. A bare number is already milliseconds, and a
/// string carrying a unit is converted: `"1.5s"` is `1500`, `"2m"` is `120000`.
fn to_duration_ms(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if let Some(n) = parse_number(t) {
                return Some(n);
            }
            timeutil::parse_duration(t).map(|secs| secs * 1000.0)
        }
        _ => None,
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

    // --- scalar functions -------------------------------------------------

    fn value(expr: &str, record: &Value) -> Option<Value> {
        let e = parse_expr(expr).unwrap();
        match e {
            Expr::Truthy(v) => eval_value(&v, &rec(record.clone())),
            other => panic!("expected a bare value, got {other:?}"),
        }
    }

    #[test]
    fn lower_and_upper_change_case() {
        let r = json!({"level": "Error", "code": 500});
        assert_eq!(value("lower(level)", &r), Some(json!("error")));
        assert_eq!(value("upper(level)", &r), Some(json!("ERROR")));
        // Numbers are rendered as text first.
        assert_eq!(value("upper(code)", &r), Some(json!("500")));
    }

    #[test]
    fn len_counts_characters_and_members() {
        let r = json!({"s": "héllo", "arr": [1, 2, 3], "obj": {"a": 1}, "n": 5});
        assert_eq!(value("len(s)", &r), Some(json!(5.0)));
        assert_eq!(value("len(arr)", &r), Some(json!(3.0)));
        assert_eq!(value("len(obj)", &r), Some(json!(1.0)));
        assert_eq!(value("len(n)", &r), None);
        assert_eq!(value("len(nope)", &r), None);
    }

    #[test]
    fn substr_slices_by_character() {
        let r = json!({"s": "connection timeout", "u": "日本語ログ"});
        assert_eq!(value("substr(s, 0, 10)", &r), Some(json!("connection")));
        assert_eq!(value("substr(s, 11)", &r), Some(json!("timeout")));
        assert_eq!(value("substr(u, 0, 3)", &r), Some(json!("日本語")));
        // Past the end is clamped, not an error.
        assert_eq!(value("substr(s, 5, 999)", &r), Some(json!("ction timeout")));
        assert_eq!(value("substr(s, 99, 3)", &r), Some(json!("")));
        assert_eq!(value("substr(s, 0, 0)", &r), Some(json!("")));
    }

    #[test]
    fn substr_counts_a_negative_start_from_the_end() {
        let r = json!({"s": "abcdef"});
        assert_eq!(value("substr(s, -2)", &r), Some(json!("ef")));
        assert_eq!(value("substr(s, -99)", &r), Some(json!("abcdef")));
    }

    #[test]
    fn coalesce_takes_the_first_present_value() {
        let r = json!({"a": null, "b": "", "c": "x"});
        assert_eq!(value(r#"coalesce(missing, a, b, c)"#, &r), Some(json!("")));
        assert_eq!(value(r#"coalesce(missing, a, c)"#, &r), Some(json!("x")));
        assert_eq!(value(r#"coalesce(missing, a)"#, &r), None);
        assert_eq!(value(r#"coalesce(missing, "fallback")"#, &r), Some(json!("fallback")));
    }

    #[test]
    fn now_returns_epoch_seconds() {
        let v = value("now()", &json!({})).unwrap();
        assert!(v.as_f64().unwrap() > 1_767_225_600.0);
    }

    #[test]
    fn num_casts_to_a_number() {
        let r = json!({"n": 42, "s": "500", "u": "120ms", "b": true, "junk": "error", "arr": []});
        assert_eq!(value("num(n)", &r), Some(json!(42.0)));
        assert_eq!(value("num(s)", &r), Some(json!(500.0)));
        assert_eq!(value("num(u)", &r), Some(json!(120.0)));
        assert_eq!(value("num(b)", &r), Some(json!(1.0)));
        assert_eq!(value("num(junk)", &r), None);
        assert_eq!(value("num(arr)", &r), None);
        assert_eq!(value("num(missing)", &r), None);
    }

    #[test]
    fn duration_ms_normalises_units() {
        let r = json!({"a": "1.5s", "b": "250ms", "c": "2m", "d": 300, "e": "300", "f": "soon"});
        assert_eq!(value("duration_ms(a)", &r), Some(json!(1500.0)));
        assert_eq!(value("duration_ms(b)", &r), Some(json!(250.0)));
        assert_eq!(value("duration_ms(c)", &r), Some(json!(120000.0)));
        assert_eq!(value("duration_ms(d)", &r), Some(json!(300.0)));
        assert_eq!(value("duration_ms(e)", &r), Some(json!(300.0)));
        assert_eq!(value("duration_ms(f)", &r), None);
    }

    #[test]
    fn trim_strips_surrounding_whitespace() {
        let r = json!({"s": "  padded \t", "inner": "a b"});
        assert_eq!(value("trim(s)", &r), Some(json!("padded")));
        assert_eq!(value("trim(inner)", &r), Some(json!("a b")));
    }

    #[test]
    fn substring_predicates_return_booleans() {
        let r = json!({"msg": "upstream timeout on /v1/users"});
        assert_eq!(value(r#"contains(msg, "timeout")"#, &r), Some(json!(true)));
        assert_eq!(value(r#"contains(msg, "Timeout")"#, &r), Some(json!(false)));
        assert_eq!(value(r#"starts_with(msg, "upstream")"#, &r), Some(json!(true)));
        assert_eq!(value(r#"ends_with(msg, "/v1/users")"#, &r), Some(json!(true)));
        assert_eq!(value(r#"ends_with(msg, "upstream")"#, &r), Some(json!(false)));
        assert_eq!(value(r#"contains(gone, "x")"#, &r), None);
    }

    #[test]
    fn substring_predicates_filter_in_where() {
        let r = json!({"msg": "GET /health 200"});
        assert!(check(r#"contains(msg, "/health")"#, &r));
        assert!(check(r#"starts_with(msg, "GET") and ends_with(msg, "200")"#, &r));
        assert!(!check(r#"contains(msg, "POST")"#, &r));
    }

    #[test]
    fn replace_rewrites_every_occurrence() {
        let r = json!({"path": "/v1/users/12/orders/34"});
        assert_eq!(
            value(r#"replace(path, "/", ":")"#, &r),
            Some(json!(":v1:users:12:orders:34"))
        );
        // An empty needle would match everywhere; the text is returned as-is.
        assert_eq!(value(r#"replace(path, "", "x")"#, &r), Some(json!("/v1/users/12/orders/34")));
        assert_eq!(value(r#"replace(gone, "a", "b")"#, &r), None);
    }

    #[test]
    fn concat_joins_the_arguments_it_has() {
        let r = json!({"level": "error", "code": 500, "nil": null});
        assert_eq!(
            value(r#"concat(level, ": ", code)"#, &r),
            Some(json!("error: 500"))
        );
        // Absent and null pieces contribute nothing rather than voiding the row.
        assert_eq!(value(r#"concat(gone, level, nil)"#, &r), Some(json!("error")));
        assert_eq!(value("concat(gone, nil)", &r), None);
    }

    #[test]
    fn arithmetic_functions_round_and_truncate() {
        let r = json!({"n": -2.5, "d": 1234.5678, "s": "  42.7 "});
        assert_eq!(value("abs(n)", &r), Some(json!(2.5)));
        assert_eq!(value("floor(d)", &r), Some(json!(1234.0)));
        assert_eq!(value("ceil(d)", &r), Some(json!(1235.0)));
        assert_eq!(value("round(d)", &r), Some(json!(1235.0)));
        assert_eq!(value("round(d, 2)", &r), Some(json!(1234.57)));
        // Text that reads as a number is cast first, as `num` would.
        assert_eq!(value("round(s)", &r), Some(json!(43.0)));
        assert_eq!(value("abs(gone)", &r), None);
    }

    #[test]
    fn rounding_places_are_clamped_to_a_usable_range() {
        let r = json!({"d": 1.23456});
        // A negative place count would otherwise scale the value away.
        assert_eq!(value("round(d, -3)", &r), Some(json!(1.0)));
        assert_eq!(value("round(d, 99)", &r), Some(json!(1.23456)));
    }

    #[test]
    fn ts_reads_timestamps_onto_one_scale() {
        let r = json!({
            "text": "2026-07-27T10:00:00Z",
            "offset": "2026-07-27T12:00:00+02:00",
            "secs": 1785146400,
            "millis": 1785146400000i64,
            "junk": "yesterday"
        });
        assert_eq!(value("ts(text)", &r), Some(json!(1785146400.0)));
        // The same instant written in another zone lands on the same number.
        assert_eq!(value("ts(offset)", &r), value("ts(text)", &r));
        assert_eq!(value("ts(secs)", &r), Some(json!(1785146400.0)));
        assert_eq!(value("ts(millis)", &r), Some(json!(1785146400.0)));
        assert_eq!(value("ts(junk)", &r), None);
        assert_eq!(value("ts(gone)", &r), None);
    }

    #[test]
    fn format_time_renders_in_the_named_zone() {
        let r = json!({"ts": "2026-07-27T10:00:00Z"});
        assert_eq!(
            value("format_time(ts)", &r),
            Some(json!("2026-07-27T10:00:00Z"))
        );
        assert_eq!(
            value(r#"format_time(ts, "+02:00")"#, &r),
            Some(json!("2026-07-27T12:00:00+02:00"))
        );
        assert_eq!(
            value(r#"format_time(ts, "utc")"#, &r),
            Some(json!("2026-07-27T10:00:00Z"))
        );
        // A zone nobody can resolve is missing, not a quiet fallback to UTC.
        assert_eq!(value(r#"format_time(ts, "Mars/Olympus")"#, &r), None);
    }

    #[test]
    fn timestamps_compare_across_representations() {
        let r = json!({"ts": "2026-07-27T10:00:00Z", "seen": 1785146460});
        assert!(check("ts(seen) > ts(ts)", &r));
        assert!(check("ts(ts) < now()", &r));
    }

    #[test]
    fn sums_and_differences_evaluate() {
        let r = json!({"a": 10, "b": 4, "s": "2.5"});
        assert_eq!(value("a + b", &r), Some(json!(14.0)));
        assert_eq!(value("a - b", &r), Some(json!(6.0)));
        assert_eq!(value("a - b - b", &r), Some(json!(2.0)));
        assert_eq!(value("a - (b - b)", &r), Some(json!(10.0)));
        // A numeric string is cast, as `num` casts it.
        assert_eq!(value("a + s", &r), Some(json!(12.5)));
    }

    #[test]
    fn a_duration_literal_is_its_length_in_seconds() {
        let r = json!({});
        assert_eq!(value("0 + 15m", &r), Some(json!(900.0)));
        assert_eq!(value("0 + 1h", &r), Some(json!(3600.0)));
        assert_eq!(value("0 + 250ms", &r), Some(json!(0.25)));
    }

    #[test]
    fn a_recency_window_filters_on_the_clock() {
        let now = crate::timeutil::now_epoch();
        let r = json!({"recent": now - 60.0, "old": now - 7200.0});
        assert!(check("recent > now() - 15m", &r));
        assert!(!check("old > now() - 15m", &r));
    }

    #[test]
    fn arithmetic_on_a_missing_or_non_numeric_side_is_missing() {
        let r = json!({"a": 1, "text": "error", "nil": null});
        assert_eq!(value("a + gone", &r), None);
        assert_eq!(value("gone + a", &r), None);
        assert_eq!(value("a + text", &r), None);
        assert_eq!(value("a + nil", &r), None);
    }

    #[test]
    fn a_function_of_a_missing_value_is_missing() {
        let r = json!({"nil": null});
        for expr in ["lower(gone)", "upper(gone)", "len(gone)", "substr(gone, 1)", "num(gone)"] {
            assert_eq!(value(expr, &r), None, "{expr}");
        }
        // A present null is not text either.
        assert_eq!(value("lower(nil)", &r), None);
    }

    #[test]
    fn functions_compose_and_compare() {
        let r = json!({"level": "ERROR", "msg": "Upstream Timeout", "dur": "1.5s"});
        assert!(check(r#"lower(level) = "error""#, &r));
        assert!(check(r#"lower(substr(msg, 0, 8)) = "upstream""#, &r));
        assert!(check("duration_ms(dur) > 1000", &r));
        assert!(check("len(level) = 5", &r));
        // Missing propagates into the comparison, which is then false.
        assert!(!check("len(gone) = 5", &r));
        assert!(!check("len(gone) != 5", &r));
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
