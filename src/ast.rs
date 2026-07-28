//! Abstract syntax tree for the query language.

use std::fmt;

/// A parsed query. Every clause except `select` is optional.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub select: Selection,
    pub filter: Option<Expr>,
    /// `group by level, bucket(ts, 5m)` — the values that define a group. The
    /// labels are the column headers the group keys are printed under.
    pub group_by: Vec<SelectItem>,
    /// `having count(*) > 10` — a filter applied to the grouped rows.
    pub having: Option<Expr>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
}

impl Query {
    /// True when the query produces one row per group rather than one row per
    /// record. Writing an aggregate without `group by` is a single group over
    /// the whole input, as in SQL.
    pub fn is_grouped(&self) -> bool {
        !self.group_by.is_empty() || self.mentions_an_aggregate()
    }

    /// True when an aggregate appears anywhere the query could use one.
    fn mentions_an_aggregate(&self) -> bool {
        let selected = match &self.select {
            Selection::All => false,
            Selection::Items(items) => items.iter().any(|i| i.value.has_aggregate()),
        };
        selected
            || self.having.is_some()
            || self
                .order_by
                .as_ref()
                .is_some_and(|o| o.value.has_aggregate())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    /// `select *` — emit every field of the record.
    All,
    /// `select a, lower(b.c) as name` — emit these values, in this order.
    Items(Vec<SelectItem>),
}

/// One entry of a select list: a value to compute and the column to print it
/// under.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    pub value: ValueExpr,
    /// The column header: the alias when one was written, otherwise the
    /// expression as it appeared in the query.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    /// The value to sort on. It need not appear in the select list.
    pub value: ValueExpr,
    pub descending: bool,
}

/// A dotted field path, pre-split into segments. `user.id` is `["user", "id"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    /// The path exactly as written, used as the output column header.
    pub raw: String,
    pub segments: Vec<String>,
}

impl Path {
    pub fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let segments = raw.split('.').map(|s| s.to_string()).collect();
        Path { raw, segments }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Num(f64),
    /// A duration literal such as `15m`, already converted to seconds.
    Duration(f64),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Compare {
        op: CmpOp,
        left: ValueExpr,
        right: ValueExpr,
    },
    /// `field like "pat%"` — `negated` is set by `not like`.
    Like {
        left: ValueExpr,
        pattern: ValueExpr,
        negated: bool,
    },
    /// A bare value used as a condition, e.g. `where ok`. Truthiness rules
    /// live in `eval`.
    Truthy(ValueExpr),
}

/// The scalar functions, resolved at parse time so an unknown name is a query
/// error rather than a silent miss at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Lower,
    Upper,
    Len,
    Substr,
    Coalesce,
    Now,
    Num,
    DurationMs,
    Trim,
    Contains,
    StartsWith,
    EndsWith,
    Replace,
    Concat,
    Abs,
    Round,
    Floor,
    Ceil,
    Ts,
    FormatTime,
}

impl Func {
    pub fn parse(name: &str) -> Option<Func> {
        match name.to_ascii_lowercase().as_str() {
            "lower" => Some(Func::Lower),
            "upper" => Some(Func::Upper),
            "len" | "length" => Some(Func::Len),
            "substr" | "substring" => Some(Func::Substr),
            "coalesce" => Some(Func::Coalesce),
            "now" => Some(Func::Now),
            "num" => Some(Func::Num),
            "duration_ms" => Some(Func::DurationMs),
            "trim" => Some(Func::Trim),
            "contains" => Some(Func::Contains),
            "starts_with" => Some(Func::StartsWith),
            "ends_with" => Some(Func::EndsWith),
            "replace" => Some(Func::Replace),
            "concat" => Some(Func::Concat),
            "abs" => Some(Func::Abs),
            "round" => Some(Func::Round),
            "floor" => Some(Func::Floor),
            "ceil" => Some(Func::Ceil),
            "ts" => Some(Func::Ts),
            "format_time" => Some(Func::FormatTime),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Func::Lower => "lower",
            Func::Upper => "upper",
            Func::Len => "len",
            Func::Substr => "substr",
            Func::Coalesce => "coalesce",
            Func::Now => "now",
            Func::Num => "num",
            Func::DurationMs => "duration_ms",
            Func::Trim => "trim",
            Func::Contains => "contains",
            Func::StartsWith => "starts_with",
            Func::EndsWith => "ends_with",
            Func::Replace => "replace",
            Func::Concat => "concat",
            Func::Abs => "abs",
            Func::Round => "round",
            Func::Floor => "floor",
            Func::Ceil => "ceil",
            Func::Ts => "ts",
            Func::FormatTime => "format_time",
        }
    }

    /// The accepted argument counts, as `(minimum, maximum)`. `None` for the
    /// maximum means "any number".
    pub fn arity(self) -> (usize, Option<usize>) {
        match self {
            Func::Now => (0, Some(0)),
            Func::Lower | Func::Upper | Func::Len | Func::Num | Func::DurationMs
            | Func::Trim
            | Func::Abs
            | Func::Floor
            | Func::Ceil
            | Func::Ts => (1, Some(1)),
            Func::FormatTime => (1, Some(2)),
            Func::Round => (1, Some(2)),
            Func::Substr => (2, Some(3)),
            Func::Contains | Func::StartsWith | Func::EndsWith => (2, Some(2)),
            Func::Replace => (3, Some(3)),
            Func::Coalesce | Func::Concat => (1, None),
        }
    }
}

/// Addition and subtraction over values. Multiplication and division are
/// deliberately absent: `*` already means "every field" in a select list, and
/// nothing a log query asks for needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
}

impl ArithOp {
    pub fn symbol(self) -> char {
        match self {
            ArithOp::Add => '+',
            ArithOp::Sub => '-',
        }
    }
}

/// The aggregate functions. These fold a whole group down to one value, so
/// unlike a scalar function they can only appear in `select` and `having`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
    First,
    Last,
}

impl Agg {
    pub fn parse(name: &str) -> Option<Agg> {
        match name.to_ascii_lowercase().as_str() {
            "count" => Some(Agg::Count),
            "count_distinct" => Some(Agg::CountDistinct),
            "sum" => Some(Agg::Sum),
            "avg" | "mean" => Some(Agg::Avg),
            "min" => Some(Agg::Min),
            "max" => Some(Agg::Max),
            "first" => Some(Agg::First),
            "last" => Some(Agg::Last),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agg::Count => "count",
            Agg::CountDistinct => "count_distinct",
            Agg::Sum => "sum",
            Agg::Avg => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
            Agg::First => "first",
            Agg::Last => "last",
        }
    }

    /// True when the aggregate accepts `count(*)`, meaning "every record in the
    /// group" rather than "every record where this value is present".
    pub fn accepts_star(self) -> bool {
        matches!(self, Agg::Count)
    }
}

/// Something that produces a value: a field, a literal, a call, or a sum.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueExpr {
    Field(Path),
    Lit(Literal),
    Call {
        func: Func,
        args: Vec<ValueExpr>,
    },
    Arith {
        op: ArithOp,
        left: Box<ValueExpr>,
        right: Box<ValueExpr>,
    },
    /// An aggregate over a group. `arg` is `None` for `count(*)`.
    Agg {
        agg: Agg,
        arg: Option<Box<ValueExpr>>,
    },
}

impl ValueExpr {
    /// True when evaluating this needs a whole group rather than one record.
    pub fn has_aggregate(&self) -> bool {
        match self {
            ValueExpr::Field(_) | ValueExpr::Lit(_) => false,
            ValueExpr::Agg { .. } => true,
            ValueExpr::Call { args, .. } => args.iter().any(|a| a.has_aggregate()),
            ValueExpr::Arith { left, right, .. } => left.has_aggregate() || right.has_aggregate(),
        }
    }
}

impl fmt::Display for ValueExpr {
    /// Render the expression back to query text. This is what a select item
    /// without an alias is labelled with, so `select lower(level)` prints a
    /// column headed `lower(level)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueExpr::Field(p) => write!(f, "{}", p.raw),
            ValueExpr::Lit(l) => write!(f, "{l}"),
            ValueExpr::Call { func, args } => {
                write!(f, "{}(", func.name())?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{a}")?;
                }
                write!(f, ")")
            }
            ValueExpr::Agg { agg, arg } => match arg {
                None => write!(f, "{}(*)", agg.name()),
                Some(a) => write!(f, "{}({a})", agg.name()),
            },
            ValueExpr::Arith { op, left, right } => {
                write!(f, "{left} {} ", op.symbol())?;
                // `a - (b - c)` differs from `a - b - c`, so a nested sum on
                // the right keeps the parentheses that were written.
                if matches!(**right, ValueExpr::Arith { .. }) {
                    write!(f, "({right})")
                } else {
                    write!(f, "{right}")
                }
            }
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Str(s) => write!(f, "\"{s}\""),
            Literal::Num(n) => write!(f, "{n}"),
            Literal::Duration(secs) => write!(f, "{secs}s"),
            Literal::Bool(b) => write!(f, "{b}"),
            Literal::Null => write!(f, "null"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_splits_on_dots() {
        let p = Path::new("user.id");
        assert_eq!(p.raw, "user.id");
        assert_eq!(p.segments, vec!["user", "id"]);
    }

    #[test]
    fn path_without_dots_is_one_segment() {
        assert_eq!(Path::new("level").segments, vec!["level"]);
    }

    #[test]
    fn function_names_resolve_case_insensitively() {
        assert_eq!(Func::parse("LOWER"), Some(Func::Lower));
        assert_eq!(Func::parse("length"), Some(Func::Len));
        assert_eq!(Func::parse("substring"), Some(Func::Substr));
        assert_eq!(Func::parse("nope"), None);
    }

    #[test]
    fn value_expressions_render_as_query_text() {
        let e = ValueExpr::Call {
            func: Func::Coalesce,
            args: vec![
                ValueExpr::Field(Path::new("user.id")),
                ValueExpr::Lit(Literal::Str("anon".into())),
            ],
        };
        assert_eq!(e.to_string(), "coalesce(user.id, \"anon\")");
        assert_eq!(
            ValueExpr::Call {
                func: Func::Now,
                args: vec![]
            }
            .to_string(),
            "now()"
        );
    }
}
