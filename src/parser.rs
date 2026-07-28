//! Recursive-descent parser for the query language.
//!
//! Grammar (lowest precedence first):
//!
//! ```text
//! query      := "select" selection [ "where" expr ]
//!               [ "group" "by" item { "," item } ] [ "having" expr ]
//!               [ "order" "by" value [ "asc" | "desc" ] ]
//!               [ "limit" number ]
//! selection  := "*" | item { "," item }
//! item       := value [ "as" name ]
//! expr       := or_expr
//! or_expr    := and_expr { "or" and_expr }
//! and_expr   := not_expr { "and" not_expr }
//! not_expr   := "not" not_expr | predicate
//! predicate  := primary [ ( cmp_op value ) | ( ["not"] "like" value ) ]
//! primary    := "(" expr ")" | value
//! value      := atom { ( "+" | "-" ) atom }
//! atom       := "(" value ")" | call | agg | field | literal | duration
//! call       := name "(" [ value { "," value } ] ")"
//! agg        := name "(" ( "*" | value ) ")"
//! ```
//!
//! `not` binds tighter than `and`, which binds tighter than `or`. Comparisons
//! bind tighter than all three, so `not a = 1 and b = 2` parses as
//! `(not (a = 1)) and (b = 2)`.

use crate::ast::{
    Agg, ArithOp, CmpOp, Expr, Func, Literal, OrderBy, Path, Query, SelectItem, Selection, ValueExpr,
};
use crate::lexer::{tokenize, QueryError, Token, TokenKind};

pub fn parse(query: &str) -> Result<Query, QueryError> {
    let tokens = tokenize(query)?;
    let mut p = Parser {
        src: query,
        tokens,
        pos: 0,
    };
    let q = p.parse_query()?;
    p.expect_eof()?;
    Ok(q)
}

/// Parse a bare filter expression, without the surrounding `select`. The
/// binary always goes through [`parse`]; this entry point exists so the
/// expression grammar and evaluator can be exercised in isolation.
#[cfg(test)]
pub fn parse_expr(src: &str) -> Result<Expr, QueryError> {
    let tokens = tokenize(src)?;
    let mut p = Parser {
        src,
        tokens,
        pos: 0,
    };
    let e = p.parse_expr()?;
    p.expect_eof()?;
    Ok(e)
}

/// Render an arity for an error message: "1 argument", "2 or 3 arguments",
/// "at least 1 argument".
fn describe_arity(min: usize, max: Option<usize>) -> String {
    let plural = |n: usize| if n == 1 { "argument" } else { "arguments" };
    match max {
        Some(m) if m == min => format!("{min} {}", plural(min)),
        Some(m) => format!("{min} to {m} arguments"),
        None => format!("at least {min} {}", plural(min)),
    }
}

/// True when any leaf of the expression is an aggregate.
fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Or(a, b) | Expr::And(a, b) => expr_has_aggregate(a) || expr_has_aggregate(b),
        Expr::Not(a) => expr_has_aggregate(a),
        Expr::Compare { left, right, .. } => left.has_aggregate() || right.has_aggregate(),
        Expr::Like { left, pattern, .. } => left.has_aggregate() || pattern.has_aggregate(),
        Expr::Truthy(v) => v.has_aggregate(),
    }
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        // The lexer always appends Eof, so this never runs off the end as long
        // as we stop advancing at Eof (which `bump` guarantees).
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if t.kind != TokenKind::Eof {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if &self.peek().kind == kind {
            self.bump();
            true
        } else {
            false
        }
    }

    fn err_here(&self, expected: &str) -> QueryError {
        let t = self.peek();
        QueryError::new(
            format!("expected {expected}, found {}", t.kind),
            t.col,
            self.src,
        )
    }

    fn expect(&mut self, kind: TokenKind, expected: &str) -> Result<Token, QueryError> {
        if self.peek().kind == kind {
            Ok(self.bump())
        } else {
            Err(self.err_here(expected))
        }
    }

    fn expect_eof(&self) -> Result<(), QueryError> {
        if self.peek().kind == TokenKind::Eof {
            Ok(())
        } else {
            Err(self.err_here("end of query"))
        }
    }

    fn parse_query(&mut self) -> Result<Query, QueryError> {
        self.expect(TokenKind::Select, "`select` at the start of the query")?;
        let select_col = self.peek().col;
        let (select, item_cols) = self.parse_selection()?;

        let filter_col = self.peek().col;
        let filter = if self.eat(&TokenKind::Where) {
            let e = self.parse_expr()?;
            if expr_has_aggregate(&e) {
                return Err(QueryError::new(
                    "an aggregate cannot appear in `where`; `where` runs before the groups are \
                     formed, so use `having` instead",
                    filter_col,
                    self.src,
                ));
            }
            Some(e)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat(&TokenKind::Group) {
            self.expect(TokenKind::By, "`by` after `group`")?;
            group_by.push(self.parse_select_item("a field name or a function after `group by`")?);
            while self.eat(&TokenKind::Comma) {
                group_by.push(self.parse_select_item("a field name or a function after `,`")?);
            }
        }

        let having = if self.eat(&TokenKind::Having) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let order_by = if self.eat(&TokenKind::Order) {
            self.expect(TokenKind::By, "`by` after `order`")?;
            let value = self.parse_value("a field name or a function after `order by`")?;
            let descending = if self.eat(&TokenKind::Desc) {
                true
            } else {
                self.eat(&TokenKind::Asc);
                false
            };
            Some(OrderBy { value, descending })
        } else {
            None
        };

        let limit = if self.eat(&TokenKind::Limit) {
            let t = self.bump();
            match t.kind {
                TokenKind::Num(n) if n >= 0.0 && n.fract() == 0.0 => Some(n as usize),
                TokenKind::Num(_) => {
                    return Err(QueryError::new(
                        "`limit` requires a non-negative whole number",
                        t.col,
                        self.src,
                    ))
                }
                other => {
                    return Err(QueryError::new(
                        format!("expected a number after `limit`, found {other}"),
                        t.col,
                        self.src,
                    ))
                }
            }
        } else {
            None
        };

        let query = Query {
            select,
            filter,
            group_by,
            having,
            order_by,
            limit,
        };
        self.check_grouping(&query, select_col, &item_cols)?;
        Ok(query)
    }

    /// A grouped query may only select its group keys and aggregates of them.
    /// Anything else would have to pick one record's value out of the group
    /// arbitrarily, which is never what the query meant.
    fn check_grouping(
        &self,
        query: &Query,
        select_col: usize,
        item_cols: &[usize],
    ) -> Result<(), QueryError> {
        if !query.is_grouped() {
            return Ok(());
        }
        let Selection::Items(items) = &query.select else {
            return Err(QueryError::new(
                "`select *` cannot be grouped; name the group keys and the aggregates you want",
                select_col,
                self.src,
            ));
        };
        for (i, item) in items.iter().enumerate() {
            if item.value.has_aggregate() || query.group_by.iter().any(|k| k.value == item.value) {
                continue;
            }
            return Err(QueryError::new(
                format!(
                    "`{}` is neither a group key nor an aggregate; add it to `group by` or wrap \
                     it in an aggregate",
                    item.value
                ),
                item_cols.get(i).copied().unwrap_or(select_col),
                self.src,
            ));
        }
        Ok(())
    }

    /// The select list, with the column at which each item started so that a
    /// grouping error can point at the offending entry.
    fn parse_selection(&mut self) -> Result<(Selection, Vec<usize>), QueryError> {
        if self.eat(&TokenKind::Star) {
            return Ok((Selection::All, Vec::new()));
        }
        let mut cols = vec![self.peek().col];
        let mut items =
            vec![self.parse_select_item("`*`, a field name or a function after `select`")?];
        while self.eat(&TokenKind::Comma) {
            cols.push(self.peek().col);
            items.push(self.parse_select_item("a field name or a function after `,`")?);
        }
        Ok((Selection::Items(items), cols))
    }

    /// One select entry, with an optional `as` alias. Without an alias the
    /// column is headed with the expression exactly as it was written.
    fn parse_select_item(&mut self, expected: &str) -> Result<SelectItem, QueryError> {
        let value = self.parse_value(expected)?;
        let label = if self.eat(&TokenKind::As) {
            match &self.peek().kind {
                TokenKind::Ident(name) => {
                    let name = name.clone();
                    self.bump();
                    name
                }
                TokenKind::Str(name) => {
                    let name = name.clone();
                    self.bump();
                    name
                }
                _ => return Err(self.err_here("a column name after `as`")),
            }
        } else {
            value.to_string()
        };
        Ok(SelectItem { value, label })
    }

    fn parse_field(&mut self, expected: &str) -> Result<Path, QueryError> {
        match &self.peek().kind {
            TokenKind::Ident(name) => {
                let path = Path::new(name.clone());
                self.bump();
                Ok(path)
            }
            _ => Err(self.err_here(expected)),
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, QueryError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_and()?;
        while self.eat(&TokenKind::Or) {
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_not()?;
        while self.eat(&TokenKind::And) {
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, QueryError> {
        if self.eat(&TokenKind::Not) {
            Ok(Expr::Not(Box::new(self.parse_not()?)))
        } else {
            self.parse_predicate()
        }
    }

    fn parse_predicate(&mut self) -> Result<Expr, QueryError> {
        // A parenthesised group is a complete expression, not an operand, so
        // handle it before falling through to operand parsing.
        if self.peek().kind == TokenKind::LParen {
            self.bump();
            let inner = self.parse_expr()?;
            self.expect(TokenKind::RParen, "`)` to close the group")?;
            return Ok(inner);
        }

        let left = self.parse_value("a field name, a function or a literal value")?;

        if let Some(op) = self.peek_cmp() {
            self.bump();
            let right = self.parse_value("a field name, a function or a literal value")?;
            return Ok(Expr::Compare { op, left, right });
        }

        if self.eat(&TokenKind::Like) {
            let pattern = self.parse_value("a pattern after `like`")?;
            return Ok(Expr::Like {
                left,
                pattern,
                negated: false,
            });
        }

        // `field not like "x"` — the postfix form of negated like.
        if self.peek().kind == TokenKind::Not
            && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::Like)
        {
            self.bump();
            self.bump();
            let pattern = self.parse_value("a pattern after `not like`")?;
            return Ok(Expr::Like {
                left,
                pattern,
                negated: true,
            });
        }

        Ok(Expr::Truthy(left))
    }

    fn peek_cmp(&self) -> Option<CmpOp> {
        match self.peek().kind {
            TokenKind::Eq => Some(CmpOp::Eq),
            TokenKind::Ne => Some(CmpOp::Ne),
            TokenKind::Lt => Some(CmpOp::Lt),
            TokenKind::Le => Some(CmpOp::Le),
            TokenKind::Gt => Some(CmpOp::Gt),
            TokenKind::Ge => Some(CmpOp::Ge),
            _ => None,
        }
    }

    /// A value, including any `+`/`-` chain applied to it. The operators are
    /// left-associative, so `a - b - c` is `(a - b) - c`.
    fn parse_value(&mut self, expected: &str) -> Result<ValueExpr, QueryError> {
        let mut left = self.parse_atom(expected)?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => ArithOp::Add,
                TokenKind::Minus => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.bump();
            let right = self.parse_atom("a value after the operator")?;
            left = ValueExpr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    /// A single value: a call, a field, a literal, or a parenthesised sum.
    fn parse_atom(&mut self, expected: &str) -> Result<ValueExpr, QueryError> {
        let t = self.peek().clone();
        // Parentheses around a value group its arithmetic. A parenthesised
        // condition is handled by `parse_predicate` before this point.
        if self.eat(&TokenKind::LParen) {
            let inner = self.parse_value(expected)?;
            self.expect(TokenKind::RParen, "`)` to close the group")?;
            return Ok(inner);
        }
        // `name(` is a call; a bare name is a field.
        if let TokenKind::Ident(name) = &t.kind {
            if self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&TokenKind::LParen) {
                return self.parse_call(name.clone(), t.col);
            }
        }
        let value = match t.kind {
            TokenKind::Ident(name) => ValueExpr::Field(Path::new(name)),
            TokenKind::Str(s) => ValueExpr::Lit(Literal::Str(s)),
            TokenKind::Num(n) => ValueExpr::Lit(Literal::Num(n)),
            TokenKind::Duration(secs) => ValueExpr::Lit(Literal::Duration(secs)),
            TokenKind::Bool(b) => ValueExpr::Lit(Literal::Bool(b)),
            TokenKind::Null => ValueExpr::Lit(Literal::Null),
            _ => return Err(self.err_here(expected)),
        };
        self.bump();
        Ok(value)
    }

    fn parse_call(&mut self, name: String, col: usize) -> Result<ValueExpr, QueryError> {
        if let Some(agg) = Agg::parse(&name) {
            return self.parse_agg(agg, col);
        }
        let Some(func) = Func::parse(&name) else {
            return Err(QueryError::new(
                format!("unknown function `{name}`"),
                col,
                self.src,
            ));
        };
        self.bump(); // name
        self.bump(); // `(`
        let mut args = Vec::new();
        if !self.eat(&TokenKind::RParen) {
            loop {
                args.push(self.parse_value("an argument")?);
                if self.eat(&TokenKind::Comma) {
                    continue;
                }
                self.expect(TokenKind::RParen, "`,` or `)` to close the argument list")?;
                break;
            }
        }
        let (min, max) = func.arity();
        if args.len() < min || max.map(|m| args.len() > m).unwrap_or(false) {
            return Err(QueryError::new(
                format!(
                    "`{}` takes {}, but {} {} given",
                    func.name(),
                    describe_arity(min, max),
                    args.len(),
                    if args.len() == 1 { "was" } else { "were" }
                ),
                col,
                self.src,
            ));
        }
        Ok(ValueExpr::Call { func, args })
    }

    /// An aggregate call. Every aggregate takes exactly one argument, and
    /// `count` also accepts `*` to mean "every record in the group".
    fn parse_agg(&mut self, agg: Agg, col: usize) -> Result<ValueExpr, QueryError> {
        self.bump(); // name
        self.bump(); // `(`
        let arg = if self.eat(&TokenKind::Star) {
            if !agg.accepts_star() {
                return Err(QueryError::new(
                    format!("`{}(*)` is not meaningful; give it a value to fold", agg.name()),
                    col,
                    self.src,
                ));
            }
            None
        } else {
            let value = self.parse_value("a value or `*` inside the aggregate")?;
            if value.has_aggregate() {
                return Err(QueryError::new(
                    "aggregates do not nest".to_string(),
                    col,
                    self.src,
                ));
            }
            Some(Box::new(value))
        };
        self.expect(TokenKind::RParen, "`)` to close the aggregate")?;
        Ok(ValueExpr::Agg { agg, arg })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Func;

    fn field(name: &str) -> ValueExpr {
        ValueExpr::Field(Path::new(name))
    }
    fn s(v: &str) -> ValueExpr {
        ValueExpr::Lit(Literal::Str(v.into()))
    }
    fn n(v: f64) -> ValueExpr {
        ValueExpr::Lit(Literal::Num(v))
    }
    fn cmp(op: CmpOp, l: ValueExpr, r: ValueExpr) -> Expr {
        Expr::Compare {
            op,
            left: l,
            right: r,
        }
    }

    #[test]
    fn parses_select_star_only() {
        let q = parse("select *").unwrap();
        assert_eq!(q.select, Selection::All);
        assert!(q.filter.is_none());
        assert!(q.order_by.is_none());
        assert!(q.limit.is_none());
    }

    fn labels(q: &Query) -> Vec<String> {
        match &q.select {
            Selection::All => Vec::new(),
            Selection::Items(items) => items.iter().map(|i| i.label.clone()).collect(),
        }
    }

    fn values(q: &Query) -> Vec<ValueExpr> {
        match &q.select {
            Selection::All => Vec::new(),
            Selection::Items(items) => items.iter().map(|i| i.value.clone()).collect(),
        }
    }

    #[test]
    fn parses_a_field_list() {
        let q = parse("select level, msg, user.id").unwrap();
        assert_eq!(
            values(&q),
            vec![field("level"), field("msg"), field("user.id")]
        );
        assert_eq!(labels(&q), vec!["level", "msg", "user.id"]);
    }

    #[test]
    fn parses_function_calls_in_the_selection() {
        let q = parse("select lower(level), substr(msg, 0, 20)").unwrap();
        assert_eq!(
            values(&q),
            vec![
                ValueExpr::Call {
                    func: Func::Lower,
                    args: vec![field("level")]
                },
                ValueExpr::Call {
                    func: Func::Substr,
                    args: vec![field("msg"), n(0.0), n(20.0)]
                },
            ]
        );
        assert_eq!(labels(&q), vec!["lower(level)", "substr(msg, 0, 20)"]);
    }

    #[test]
    fn parses_a_call_with_no_arguments() {
        let q = parse("select now()").unwrap();
        assert_eq!(
            values(&q),
            vec![ValueExpr::Call {
                func: Func::Now,
                args: vec![]
            }]
        );
        assert_eq!(labels(&q), vec!["now()"]);
    }

    #[test]
    fn calls_nest() {
        let q = parse("select upper(coalesce(user.name, \"anon\"))").unwrap();
        assert_eq!(labels(&q), vec!["upper(coalesce(user.name, \"anon\"))"]);
    }

    #[test]
    fn an_alias_renames_the_column() {
        let q = parse("select lower(level) as lvl, msg as \"the message\"").unwrap();
        assert_eq!(labels(&q), vec!["lvl", "the message"]);
    }

    #[test]
    fn functions_are_usable_in_where() {
        let e = parse_expr("lower(level) = \"error\"").unwrap();
        assert_eq!(
            e,
            cmp(
                CmpOp::Eq,
                ValueExpr::Call {
                    func: Func::Lower,
                    args: vec![field("level")]
                },
                s("error")
            )
        );
    }

    #[test]
    fn an_unknown_function_is_an_error() {
        let err = parse("select sqrt(x)").unwrap_err();
        assert!(err.message.contains("unknown function `sqrt`"), "{}", err.message);
        assert_eq!(err.col, 8);
    }

    #[test]
    fn the_wrong_number_of_arguments_is_an_error() {
        let err = parse("select lower(a, b)").unwrap_err();
        assert!(err.message.contains("takes 1 argument"), "{}", err.message);
        let err = parse("select substr(a)").unwrap_err();
        assert!(err.message.contains("2 to 3 arguments"), "{}", err.message);
        let err = parse("select now(1)").unwrap_err();
        assert!(err.message.contains("takes 0 arguments"), "{}", err.message);
        let err = parse("select coalesce()").unwrap_err();
        assert!(err.message.contains("at least 1 argument"), "{}", err.message);
    }

    #[test]
    fn an_unclosed_argument_list_is_an_error() {
        let err = parse("select lower(a").unwrap_err();
        assert!(err.message.contains("`)`"), "{}", err.message);
    }

    #[test]
    fn an_alias_without_a_name_is_an_error() {
        let err = parse("select a as").unwrap_err();
        assert!(err.message.contains("column name after `as`"), "{}", err.message);
    }

    #[test]
    fn parses_addition_and_subtraction() {
        let e = parse_expr("ts > now() - 15m").unwrap();
        assert_eq!(
            e,
            cmp(
                CmpOp::Gt,
                field("ts"),
                ValueExpr::Arith {
                    op: ArithOp::Sub,
                    left: Box::new(ValueExpr::Call {
                        func: Func::Now,
                        args: vec![]
                    }),
                    right: Box::new(ValueExpr::Lit(Literal::Duration(900.0))),
                }
            )
        );
    }

    #[test]
    fn arithmetic_is_left_associative() {
        let q = parse("select a - b - c").unwrap();
        assert_eq!(labels(&q), vec!["a - b - c"]);
        match &values(&q)[0] {
            ValueExpr::Arith { left, .. } => {
                assert!(matches!(**left, ValueExpr::Arith { .. }), "left should nest")
            }
            other => panic!("expected a sum, got {other:?}"),
        }
    }

    #[test]
    fn parentheses_regroup_a_sum_and_survive_the_label() {
        let q = parse("select a - (b - c)").unwrap();
        assert_eq!(labels(&q), vec!["a - (b - c)"]);
        match &values(&q)[0] {
            ValueExpr::Arith { right, .. } => {
                assert!(matches!(**right, ValueExpr::Arith { .. }), "right should nest")
            }
            other => panic!("expected a sum, got {other:?}"),
        }
    }

    #[test]
    fn an_unclosed_value_group_is_an_error() {
        let err = parse("select (a - b").unwrap_err();
        assert!(err.message.contains("`)`"), "{}", err.message);
    }

    #[test]
    fn a_dangling_arithmetic_operator_is_an_error() {
        let err = parse("select a +").unwrap_err();
        assert!(err.message.contains("value after the operator"), "{}", err.message);
    }

    #[test]
    fn order_by_takes_an_expression() {
        let q = parse("select level, count(*) group by level order by count(*) desc").unwrap();
        let order = q.order_by.unwrap();
        assert_eq!(
            order.value,
            ValueExpr::Agg {
                agg: Agg::Count,
                arg: None
            }
        );
        assert!(order.descending);

        let q = parse("select msg order by lower(level)").unwrap();
        assert_eq!(q.order_by.unwrap().value.to_string(), "lower(level)");
    }

    #[test]
    fn parses_group_by_and_having() {
        let q = parse("select level, count(*) group by level having count(*) > 10").unwrap();
        assert!(q.is_grouped());
        assert_eq!(q.group_by.len(), 1);
        assert_eq!(q.group_by[0].value, field("level"));
        assert_eq!(labels(&q), vec!["level", "count(*)"]);
        assert_eq!(
            values(&q)[1],
            ValueExpr::Agg {
                agg: Agg::Count,
                arg: None
            }
        );
        assert!(q.having.is_some());
    }

    #[test]
    fn an_aggregate_without_group_by_is_one_group() {
        let q = parse("select count(*), max(duration_ms)").unwrap();
        assert!(q.is_grouped());
        assert!(q.group_by.is_empty());
    }

    #[test]
    fn a_plain_query_is_not_grouped() {
        assert!(!parse("select level, msg where level = \"error\"").unwrap().is_grouped());
        assert!(!parse("select *").unwrap().is_grouped());
    }

    #[test]
    fn aggregates_take_one_value_and_count_also_takes_star() {
        let q = parse("select sum(duration_ms), count(user.id)").unwrap();
        assert_eq!(
            values(&q),
            vec![
                ValueExpr::Agg {
                    agg: Agg::Sum,
                    arg: Some(Box::new(field("duration_ms")))
                },
                ValueExpr::Agg {
                    agg: Agg::Count,
                    arg: Some(Box::new(field("user.id")))
                },
            ]
        );
        assert_eq!(labels(&q), vec!["sum(duration_ms)", "count(user.id)"]);
    }

    #[test]
    fn only_count_accepts_a_star() {
        let err = parse("select sum(*)").unwrap_err();
        assert!(err.message.contains("not meaningful"), "{}", err.message);
    }

    #[test]
    fn aggregates_do_not_nest() {
        let err = parse("select sum(count(x))").unwrap_err();
        assert!(err.message.contains("do not nest"), "{}", err.message);
    }

    #[test]
    fn an_aggregate_in_where_is_an_error() {
        let err = parse("select level, count(*) where count(*) > 1 group by level").unwrap_err();
        assert!(err.message.contains("`having`"), "{}", err.message);
    }

    #[test]
    fn a_selected_field_must_be_a_group_key_or_an_aggregate() {
        let err = parse("select level, msg, count(*) group by level").unwrap_err();
        assert!(err.message.contains("neither a group key"), "{}", err.message);
        assert_eq!(err.col, 15);
        // Repeating the key is fine, and so is an aggregate of anything.
        assert!(parse("select level, count(msg) group by level").is_ok());
    }

    #[test]
    fn select_star_cannot_be_grouped() {
        let err = parse("select * group by level").unwrap_err();
        assert!(err.message.contains("cannot be grouped"), "{}", err.message);
    }

    #[test]
    fn group_by_accepts_an_expression_with_an_alias() {
        let q = parse("select lower(level) as lvl, count(*) group by lower(level)").unwrap();
        assert_eq!(labels(&q), vec!["lvl", "count(*)"]);
        assert_eq!(q.group_by[0].label, "lower(level)");
    }

    #[test]
    fn group_by_without_by_is_an_error() {
        let err = parse("select count(*) group level").unwrap_err();
        assert!(err.message.contains("`by` after `group`"), "{}", err.message);
    }

    #[test]
    fn parses_every_clause() {
        let q = parse("select a where b = 1 order by c desc limit 5").unwrap();
        assert_eq!(values(&q), vec![field("a")]);
        assert_eq!(q.filter, Some(cmp(CmpOp::Eq, field("b"), n(1.0))));
        assert_eq!(
            q.order_by,
            Some(OrderBy {
                value: field("c"),
                descending: true
            })
        );
        assert_eq!(q.limit, Some(5));
    }

    #[test]
    fn order_by_defaults_to_ascending() {
        let q = parse("select * order by ts").unwrap();
        assert!(!q.order_by.unwrap().descending);
        let q = parse("select * order by ts asc").unwrap();
        assert!(!q.order_by.unwrap().descending);
    }

    #[test]
    fn and_binds_tighter_than_or() {
        let e = parse_expr("a = 1 or b = 2 and c = 3").unwrap();
        assert_eq!(
            e,
            Expr::Or(
                Box::new(cmp(CmpOp::Eq, field("a"), n(1.0))),
                Box::new(Expr::And(
                    Box::new(cmp(CmpOp::Eq, field("b"), n(2.0))),
                    Box::new(cmp(CmpOp::Eq, field("c"), n(3.0))),
                ))
            )
        );
    }

    #[test]
    fn parens_override_precedence() {
        let e = parse_expr("(a = 1 or b = 2) and c = 3").unwrap();
        assert_eq!(
            e,
            Expr::And(
                Box::new(Expr::Or(
                    Box::new(cmp(CmpOp::Eq, field("a"), n(1.0))),
                    Box::new(cmp(CmpOp::Eq, field("b"), n(2.0))),
                )),
                Box::new(cmp(CmpOp::Eq, field("c"), n(3.0)))
            )
        );
    }

    #[test]
    fn not_binds_tighter_than_and() {
        let e = parse_expr("not a = 1 and b = 2").unwrap();
        assert_eq!(
            e,
            Expr::And(
                Box::new(Expr::Not(Box::new(cmp(CmpOp::Eq, field("a"), n(1.0))))),
                Box::new(cmp(CmpOp::Eq, field("b"), n(2.0)))
            )
        );
    }

    #[test]
    fn not_nests() {
        let e = parse_expr("not not a").unwrap();
        assert_eq!(
            e,
            Expr::Not(Box::new(Expr::Not(Box::new(Expr::Truthy(field("a"))))))
        );
    }

    #[test]
    fn or_is_left_associative() {
        let e = parse_expr("a or b or c").unwrap();
        assert_eq!(
            e,
            Expr::Or(
                Box::new(Expr::Or(
                    Box::new(Expr::Truthy(field("a"))),
                    Box::new(Expr::Truthy(field("b")))
                )),
                Box::new(Expr::Truthy(field("c")))
            )
        );
    }

    #[test]
    fn parses_like_and_not_like() {
        assert_eq!(
            parse_expr("msg like \"%timeout%\"").unwrap(),
            Expr::Like {
                left: field("msg"),
                pattern: s("%timeout%"),
                negated: false
            }
        );
        assert_eq!(
            parse_expr("msg not like \"%health%\"").unwrap(),
            Expr::Like {
                left: field("msg"),
                pattern: s("%health%"),
                negated: true
            }
        );
    }

    #[test]
    fn parses_all_comparison_operators() {
        for (src, op) in [
            ("a = 1", CmpOp::Eq),
            ("a != 1", CmpOp::Ne),
            ("a < 1", CmpOp::Lt),
            ("a <= 1", CmpOp::Le),
            ("a > 1", CmpOp::Gt),
            ("a >= 1", CmpOp::Ge),
        ] {
            assert_eq!(parse_expr(src).unwrap(), cmp(op, field("a"), n(1.0)), "{src}");
        }
    }

    #[test]
    fn literals_may_appear_on_either_side() {
        assert_eq!(
            parse_expr("100 < duration_ms").unwrap(),
            cmp(CmpOp::Lt, n(100.0), field("duration_ms"))
        );
    }

    #[test]
    fn parses_bool_and_null_literals() {
        assert_eq!(
            parse_expr("ok = true").unwrap(),
            cmp(CmpOp::Eq, field("ok"), ValueExpr::Lit(Literal::Bool(true)))
        );
        assert_eq!(
            parse_expr("err != null").unwrap(),
            cmp(CmpOp::Ne, field("err"), ValueExpr::Lit(Literal::Null))
        );
    }

    #[test]
    fn missing_select_is_an_error() {
        let err = parse("level = \"error\"").unwrap_err();
        assert!(err.message.contains("`select`"), "{}", err.message);
        assert_eq!(err.col, 1);
    }

    #[test]
    fn trailing_comma_in_selection_is_an_error() {
        let err = parse("select a, b,").unwrap_err();
        assert!(err.message.contains("field name or a function after `,`"), "{}", err.message);
        assert_eq!(err.col, 13);
    }

    #[test]
    fn unclosed_paren_is_an_error() {
        let err = parse("select * where (a = 1").unwrap_err();
        assert!(err.message.contains("`)`"), "{}", err.message);
        assert_eq!(err.col, 22);
    }

    #[test]
    fn dangling_operator_is_an_error() {
        let err = parse("select * where a =").unwrap_err();
        assert!(
            err.message.contains("field name, a function or a literal"),
            "{}",
            err.message
        );
        assert_eq!(err.col, 19);
    }

    #[test]
    fn order_without_by_is_an_error() {
        let err = parse("select * order ts").unwrap_err();
        assert!(err.message.contains("`by` after `order`"), "{}", err.message);
    }

    #[test]
    fn non_numeric_limit_is_an_error() {
        let err = parse("select * limit \"ten\"").unwrap_err();
        assert!(err.message.contains("number after `limit`"), "{}", err.message);
        assert_eq!(err.col, 16);
    }

    #[test]
    fn fractional_limit_is_an_error() {
        let err = parse("select * limit 2.5").unwrap_err();
        assert!(err.message.contains("whole number"), "{}", err.message);
    }

    #[test]
    fn negative_limit_is_an_error() {
        assert!(parse("select * limit -3").is_err());
    }

    #[test]
    fn junk_after_the_query_is_an_error() {
        let err = parse("select * limit 5 nonsense").unwrap_err();
        assert!(err.message.contains("end of query"), "{}", err.message);
        assert_eq!(err.col, 18);
    }

    #[test]
    fn clauses_must_appear_in_order() {
        // `where` after `limit` is rejected rather than silently reordered.
        assert!(parse("select * limit 5 where a = 1").is_err());
    }
}
