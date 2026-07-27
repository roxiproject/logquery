//! Abstract syntax tree for the query language.

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
    /// `select a, b.c` — emit these paths, in this order.
    Fields(Vec<Path>),
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
        left: Operand,
        right: Operand,
    },
    /// `field like "pat%"` — `negated` is set by `not like`.
    Like {
        left: Operand,
        pattern: Operand,
        negated: bool,
    },
    /// A bare field or literal used as a condition, e.g. `where ok`.
    /// Truthiness rules live in `eval`.
    Truthy(Operand),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Field(Path),
    Lit(Literal),
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
}
