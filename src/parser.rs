//! Recursive-descent parser for the query language.
//!
//! Grammar (lowest precedence first):
//!
//! ```text
//! query      := "select" selection [ "where" expr ]
//!               [ "order" "by" field [ "asc" | "desc" ] ]
//!               [ "limit" number ]
//! selection  := "*" | field { "," field }
//! expr       := or_expr
//! or_expr    := and_expr { "or" and_expr }
//! and_expr   := not_expr { "and" not_expr }
//! not_expr   := "not" not_expr | predicate
//! predicate  := primary [ ( cmp_op primary ) | ( ["not"] "like" primary ) ]
//! primary    := "(" expr ")" | field | literal
//! ```
//!
//! `not` binds tighter than `and`, which binds tighter than `or`. Comparisons
//! bind tighter than all three, so `not a = 1 and b = 2` parses as
//! `(not (a = 1)) and (b = 2)`.

use crate::ast::{CmpOp, Expr, Literal, OrderBy, Path, Query, Selection};
use crate::ast::Operand;
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

/// Parse just an expression. Used by tests and by anything that wants a bare
/// filter without the surrounding `select`.
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
        let select = self.parse_selection()?;

        let filter = if self.eat(&TokenKind::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let order_by = if self.eat(&TokenKind::Order) {
            self.expect(TokenKind::By, "`by` after `order`")?;
            let path = self.parse_field("a field name after `order by`")?;
            let descending = if self.eat(&TokenKind::Desc) {
                true
            } else {
                self.eat(&TokenKind::Asc);
                false
            };
            Some(OrderBy { path, descending })
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

        Ok(Query {
            select,
            filter,
            order_by,
            limit,
        })
    }

    fn parse_selection(&mut self) -> Result<Selection, QueryError> {
        if self.eat(&TokenKind::Star) {
            return Ok(Selection::All);
        }
        let mut fields = vec![self.parse_field("`*` or a field name after `select`")?];
        while self.eat(&TokenKind::Comma) {
            fields.push(self.parse_field("a field name after `,`")?);
        }
        Ok(Selection::Fields(fields))
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

        let left = self.parse_operand()?;

        if let Some(op) = self.peek_cmp() {
            self.bump();
            let right = self.parse_operand()?;
            return Ok(Expr::Compare { op, left, right });
        }

        if self.eat(&TokenKind::Like) {
            let pattern = self.parse_operand()?;
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
            let pattern = self.parse_operand()?;
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

    fn parse_operand(&mut self) -> Result<Operand, QueryError> {
        let t = self.peek().clone();
        let operand = match t.kind {
            TokenKind::Ident(name) => Operand::Field(Path::new(name)),
            TokenKind::Str(s) => Operand::Lit(Literal::Str(s)),
            TokenKind::Num(n) => Operand::Lit(Literal::Num(n)),
            TokenKind::Bool(b) => Operand::Lit(Literal::Bool(b)),
            TokenKind::Null => Operand::Lit(Literal::Null),
            _ => return Err(self.err_here("a field name or a literal value")),
        };
        self.bump();
        Ok(operand)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str) -> Operand {
        Operand::Field(Path::new(name))
    }
    fn s(v: &str) -> Operand {
        Operand::Lit(Literal::Str(v.into()))
    }
    fn n(v: f64) -> Operand {
        Operand::Lit(Literal::Num(v))
    }
    fn cmp(op: CmpOp, l: Operand, r: Operand) -> Expr {
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

    #[test]
    fn parses_a_field_list() {
        let q = parse("select level, msg, user.id").unwrap();
        assert_eq!(
            q.select,
            Selection::Fields(vec![
                Path::new("level"),
                Path::new("msg"),
                Path::new("user.id")
            ])
        );
    }

    #[test]
    fn parses_every_clause() {
        let q = parse("select a where b = 1 order by c desc limit 5").unwrap();
        assert_eq!(q.select, Selection::Fields(vec![Path::new("a")]));
        assert_eq!(q.filter, Some(cmp(CmpOp::Eq, field("b"), n(1.0))));
        assert_eq!(
            q.order_by,
            Some(OrderBy {
                path: Path::new("c"),
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
            cmp(CmpOp::Eq, field("ok"), Operand::Lit(Literal::Bool(true)))
        );
        assert_eq!(
            parse_expr("err != null").unwrap(),
            cmp(CmpOp::Ne, field("err"), Operand::Lit(Literal::Null))
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
        assert!(err.message.contains("field name after `,`"), "{}", err.message);
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
            err.message.contains("field name or a literal"),
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
