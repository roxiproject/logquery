//! Log line parsing: JSON Lines and logfmt, with per-line auto-detection.
//!
//! A record is a `serde_json::Map` of field name to value. logfmt lines are
//! lifted into the same representation so that everything downstream —
//! evaluation, ordering, output — works on one type.

use serde_json::{Map, Number, Value};

pub type Record = Map<String, Value>;

/// Which parser produced a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Logfmt,
}

/// Parse one log line, choosing the parser by inspecting the line.
///
/// Returns `None` for blank lines and for lines neither parser can make sense
/// of; the caller counts those as skipped.
pub fn parse_line(line: &str) -> Option<(Record, Format)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // A line starting with `{` is JSON or nothing. Anything else is tried as
    // logfmt. This keeps detection to a single cheap check and avoids the
    // ambiguity of a line that is valid under both readings.
    if trimmed.starts_with('{') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Object(map)) => Some((map, Format::Json)),
            _ => None,
        }
    } else {
        parse_logfmt(trimmed).map(|m| (m, Format::Logfmt))
    }
}

/// Turn a plain-text line into a record using a compiled pattern.
///
/// Each named capture group becomes a field. Captured text is typed the way a
/// bare logfmt value is, so a pattern that captures a status code yields a
/// number and `status >= 500` works without a cast. A line the pattern does
/// not match yields `None`, and the caller counts it as skipped.
pub fn parse_with_pattern(line: &str, pattern: &crate::pattern::Pattern) -> Option<Record> {
    let caps = pattern.captures(line)?;
    let mut out = Map::new();
    for (name, text) in caps {
        out.insert(name, type_bare_value(&text));
    }
    Some(out)
}

/// Parse a logfmt line: whitespace-separated `key=value` pairs.
///
/// * Values may be bare (`level=error`) or double-quoted (`msg="a b"`).
/// * Inside quotes, `\"`, `\\`, `\n`, `\t` and `\r` are recognised; any other
///   backslash pair is kept verbatim.
/// * A bare key with no `=` is recorded with a value of `true`, which is how
///   logfmt encodes flags.
/// * Bare values that look like numbers, booleans or `null` are typed as such;
///   quoted values are always strings. This makes `duration_ms > 100` work on
///   logfmt input without special-casing.
///
/// Returns `None` if a quoted value is left unterminated, or if the line
/// contains no `key=value` pair at all — a line of bare words is prose, not
/// logfmt, and is better skipped than turned into a record of flags.
pub fn parse_logfmt(line: &str) -> Option<Record> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut out = Map::new();
    let mut saw_pair = false;

    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }

        // Key: everything up to `=` or whitespace.
        let key_start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        if i == key_start {
            // A stray `=` with no key in front of it. Skip the character so we
            // make progress rather than looping forever.
            i += 1;
            continue;
        }
        let key: String = chars[key_start..i].iter().collect();

        if i >= chars.len() || chars[i] != '=' {
            out.insert(key, Value::Bool(true));
            continue;
        }
        i += 1; // consume `=`
        saw_pair = true;

        if i < chars.len() && chars[i] == '"' {
            i += 1;
            let mut value = String::new();
            let mut closed = false;
            while i < chars.len() {
                match chars[i] {
                    '"' => {
                        i += 1;
                        closed = true;
                        break;
                    }
                    '\\' if i + 1 < chars.len() => {
                        let esc = chars[i + 1];
                        match esc {
                            'n' => value.push('\n'),
                            't' => value.push('\t'),
                            'r' => value.push('\r'),
                            '"' => value.push('"'),
                            '\\' => value.push('\\'),
                            other => {
                                value.push('\\');
                                value.push(other);
                            }
                        }
                        i += 2;
                    }
                    c => {
                        value.push(c);
                        i += 1;
                    }
                }
            }
            if !closed {
                return None;
            }
            out.insert(key, Value::String(value));
        } else {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            let raw: String = chars[start..i].iter().collect();
            out.insert(key, type_bare_value(&raw));
        }
    }

    if saw_pair {
        Some(out)
    } else {
        None
    }
}

/// Give a bare logfmt value its most specific type.
fn type_bare_value(raw: &str) -> Value {
    match raw {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "" => return Value::Null,
        _ => {}
    }
    // Only treat it as a number if the whole token parses; `1.2.3` and `12ms`
    // stay strings.
    if let Ok(i) = raw.parse::<i64>() {
        return Value::Number(i.into());
    }
    if let Ok(f) = raw.parse::<f64>() {
        if let Some(num) = Number::from_f64(f) {
            return Value::Number(num);
        }
    }
    Value::String(raw.to_string())
}

/// Follow a dotted path into a record.
///
/// Object keys are matched at every level. Array indices are addressed with a
/// numeric segment (`items.0.id`). A segment that does not exist yields `None`,
/// which the evaluator treats as "missing" — distinct from a present `null`.
pub fn lookup<'a>(record: &'a Record, segments: &[String]) -> Option<&'a Value> {
    let (first, rest) = segments.split_first()?;
    let mut current = record.get(first)?;
    for seg in rest {
        current = match current {
            Value::Object(map) => map.get(seg)?,
            Value::Array(items) => {
                let idx: usize = seg.parse().ok()?;
                items.get(idx)?
            }
            _ => return None,
        };
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(v: Value) -> Record {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn detects_json_lines() {
        let (r, f) = parse_line(r#"{"level":"error","code":500}"#).unwrap();
        assert_eq!(f, Format::Json);
        assert_eq!(r["level"], json!("error"));
        assert_eq!(r["code"], json!(500));
    }

    #[test]
    fn detects_logfmt() {
        let (r, f) = parse_line("level=error code=500").unwrap();
        assert_eq!(f, Format::Logfmt);
        assert_eq!(r["level"], json!("error"));
        assert_eq!(r["code"], json!(500));
    }

    #[test]
    fn blank_lines_are_skipped() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   \t ").is_none());
    }

    #[test]
    fn malformed_json_is_rejected_not_reparsed_as_logfmt() {
        assert!(parse_line(r#"{"level":"error""#).is_none());
    }

    #[test]
    fn json_scalars_and_arrays_are_not_records() {
        assert!(parse_line("[1,2,3]").is_none());
        assert!(parse_line("42").is_none());
    }

    #[test]
    fn logfmt_handles_quoted_values_with_spaces() {
        let r = parse_logfmt(r#"level=info msg="connection reset by peer" code=104"#).unwrap();
        assert_eq!(r["msg"], json!("connection reset by peer"));
        assert_eq!(r["code"], json!(104));
    }

    #[test]
    fn logfmt_handles_escapes_inside_quotes() {
        let r = parse_logfmt(r#"msg="he said \"hi\"\nbye" path="C:\\tmp""#).unwrap();
        assert_eq!(r["msg"], json!("he said \"hi\"\nbye"));
        assert_eq!(r["path"], json!("C:\\tmp"));
    }

    #[test]
    fn logfmt_keeps_unknown_escapes_verbatim() {
        let r = parse_logfmt(r#"re="\d+""#).unwrap();
        assert_eq!(r["re"], json!("\\d+"));
    }

    #[test]
    fn logfmt_quoted_numbers_stay_strings() {
        let r = parse_logfmt(r#"a=42 b="42""#).unwrap();
        assert_eq!(r["a"], json!(42));
        assert_eq!(r["b"], json!("42"));
    }

    #[test]
    fn logfmt_types_bare_values() {
        let r = parse_logfmt("i=7 f=1.5 neg=-3 t=true f2=false n=null s=12ms v=1.2.3").unwrap();
        assert_eq!(r["i"], json!(7));
        assert_eq!(r["f"], json!(1.5));
        assert_eq!(r["neg"], json!(-3));
        assert_eq!(r["t"], json!(true));
        assert_eq!(r["f2"], json!(false));
        assert_eq!(r["n"], Value::Null);
        assert_eq!(r["s"], json!("12ms"));
        assert_eq!(r["v"], json!("1.2.3"));
    }

    #[test]
    fn plain_prose_is_not_logfmt() {
        assert!(parse_line("2026-07-27 12:00:00 starting up").is_none());
        assert!(parse_logfmt("just some words").is_none());
    }

    #[test]
    fn logfmt_bare_key_is_a_flag() {
        let r = parse_logfmt("cached level=info").unwrap();
        assert_eq!(r["cached"], json!(true));
        assert_eq!(r["level"], json!("info"));
    }

    #[test]
    fn logfmt_empty_value_is_null() {
        let r = parse_logfmt("err= level=info").unwrap();
        assert_eq!(r["err"], Value::Null);
    }

    #[test]
    fn logfmt_empty_quoted_value_is_an_empty_string() {
        let r = parse_logfmt(r#"err="""#).unwrap();
        assert_eq!(r["err"], json!(""));
    }

    #[test]
    fn logfmt_unterminated_quote_is_rejected() {
        assert!(parse_logfmt(r#"msg="never ends"#).is_none());
    }

    #[test]
    fn logfmt_tolerates_extra_whitespace() {
        let r = parse_logfmt("  a=1\t\tb=2   ").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r["b"], json!(2));
    }

    #[test]
    fn logfmt_later_key_wins() {
        let r = parse_logfmt("a=1 a=2").unwrap();
        assert_eq!(r["a"], json!(2));
    }

    #[test]
    fn logfmt_stray_equals_does_not_hang() {
        let r = parse_logfmt("= a=1").unwrap();
        assert_eq!(r["a"], json!(1));
    }

    #[test]
    fn lookup_reads_a_top_level_field() {
        let r = rec(json!({"level": "warn"}));
        assert_eq!(lookup(&r, &["level".into()]), Some(&json!("warn")));
    }

    #[test]
    fn lookup_walks_nested_objects() {
        let r = rec(json!({"user": {"id": 7, "name": {"first": "ada"}}}));
        assert_eq!(lookup(&r, &["user".into(), "id".into()]), Some(&json!(7)));
        assert_eq!(
            lookup(&r, &["user".into(), "name".into(), "first".into()]),
            Some(&json!("ada"))
        );
    }

    #[test]
    fn lookup_indexes_arrays() {
        let r = rec(json!({"items": [{"id": 1}, {"id": 2}]}));
        assert_eq!(
            lookup(&r, &["items".into(), "1".into(), "id".into()]),
            Some(&json!(2))
        );
        assert_eq!(lookup(&r, &["items".into(), "9".into()]), None);
        assert_eq!(lookup(&r, &["items".into(), "x".into()]), None);
    }

    #[test]
    fn lookup_of_a_missing_path_is_none() {
        let r = rec(json!({"user": {"id": 7}}));
        assert_eq!(lookup(&r, &["nope".into()]), None);
        assert_eq!(lookup(&r, &["user".into(), "nope".into()]), None);
        // Descending into a scalar is missing, not an error.
        assert_eq!(lookup(&r, &["user".into(), "id".into(), "x".into()]), None);
    }

    #[test]
    fn lookup_distinguishes_present_null_from_missing() {
        let r = rec(json!({"err": null}));
        assert_eq!(lookup(&r, &["err".into()]), Some(&Value::Null));
        assert_eq!(lookup(&r, &["other".into()]), None);
    }

    #[test]
    fn a_dotted_json_key_is_reachable_when_nesting_is_absent() {
        // Some emitters write flat keys containing dots. The first segment is
        // tried against the map, so `a.b` only resolves via nesting; this test
        // pins the documented behaviour.
        let r = rec(json!({"a": {"b": 1}}));
        assert_eq!(lookup(&r, &["a".into(), "b".into()]), Some(&json!(1)));
    }

    #[test]
    fn a_pattern_extracts_named_fields_from_plain_text() {
        let p = crate::pattern::Pattern::compile(
            r#"^(?<ip>[0-9.]+) \S+ \S+ \[(?<when>[^\]]+)\] "(?<method>[A-Z]+) (?<path>\S+)[^"]*" (?<status>\d+) (?<bytes>\d+)"#,
        )
        .unwrap();
        let line = r#"10.0.0.7 - - [27/Jul/2026:10:00:00 +0000] "GET /v1/users HTTP/1.1" 503 1274"#;
        let r = parse_with_pattern(line, &p).unwrap();
        assert_eq!(r["ip"], json!("10.0.0.7"));
        assert_eq!(r["method"], json!("GET"));
        assert_eq!(r["path"], json!("/v1/users"));
        // Numbers are typed, so `status >= 500` needs no cast.
        assert_eq!(r["status"], json!(503));
        assert_eq!(r["bytes"], json!(1274));
    }

    #[test]
    fn a_line_the_pattern_does_not_match_is_no_record() {
        let p = crate::pattern::Pattern::compile(r"^(?<level>[A-Z]+) (?<msg>.+)").unwrap();
        assert!(parse_with_pattern("ERROR upstream timeout", &p).is_some());
        assert!(parse_with_pattern("not shaped like that", &p).is_none());
    }

    #[test]
    fn a_group_that_did_not_take_part_is_absent() {
        let p = crate::pattern::Pattern::compile(r"^(?<a>x)|(?<b>y)$").unwrap();
        let r = parse_with_pattern("y", &p).unwrap();
        assert!(!r.contains_key("a"));
        assert_eq!(r["b"], json!("y"));
    }
}
