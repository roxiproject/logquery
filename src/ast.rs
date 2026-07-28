//! Abstract syntax tree for the query language.

use std::fmt;

/// A parsed query. Every clause except `select` is optional.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub select: Selection,
    pub filter: Option<Expr>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
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
    pub path: Path,
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
        }
    }

    /// The accepted argument counts, as `(minimum, maximum)`. `None` for the
    /// maximum means "any number".
    pub fn arity(self) -> (usize, Option<usize>) {
        match self {
            Func::Now => (0, Some(0)),
            Func::Lower | Func::Upper | Func::Len | Func::Num | Func::DurationMs => (1, Some(1)),
            Func::Substr => (2, Some(3)),
            Func::Coalesce => (1, None),
        }
    }
}

/// Something that produces a value: a field, a literal, or a call.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueExpr {
    Field(Path),
    Lit(Literal),
    Call { func: Func, args: Vec<ValueExpr> },
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
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Str(s) => write!(f, "\"{s}\""),
            Literal::Num(n) => write!(f, "{n}"),
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
