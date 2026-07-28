//! Tokenizer for the logquery query language.
//!
//! The lexer is a single pass over the query string. Every token carries the
//! byte column at which it started (1-based) so that parse errors can point at
//! the offending position in the original query.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords. Matched case-insensitively on bare identifiers.
    Select,
    Where,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    And,
    Or,
    Not,
    Like,

    /// A field path such as `level` or `user.id`. Stored with its dots intact.
    Ident(String),
    /// A string literal, already unescaped.
    Str(String),
    Num(f64),
    Bool(bool),
    Null,

    Star,
    Comma,
    LParen,
    RParen,

    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,

    /// End of input. Always the final token produced.
    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Select => write!(f, "`select`"),
            TokenKind::Where => write!(f, "`where`"),
            TokenKind::Order => write!(f, "`order`"),
            TokenKind::By => write!(f, "`by`"),
            TokenKind::Asc => write!(f, "`asc`"),
            TokenKind::Desc => write!(f, "`desc`"),
            TokenKind::Limit => write!(f, "`limit`"),
            TokenKind::And => write!(f, "`and`"),
            TokenKind::Or => write!(f, "`or`"),
            TokenKind::Not => write!(f, "`not`"),
            TokenKind::Like => write!(f, "`like`"),
            TokenKind::Ident(s) => write!(f, "field `{s}`"),
            TokenKind::Str(s) => write!(f, "string \"{s}\""),
            TokenKind::Num(n) => write!(f, "number {n}"),
            TokenKind::Bool(b) => write!(f, "`{b}`"),
            TokenKind::Null => write!(f, "`null`"),
            TokenKind::Star => write!(f, "`*`"),
            TokenKind::Comma => write!(f, "`,`"),
            TokenKind::LParen => write!(f, "`(`"),
            TokenKind::RParen => write!(f, "`)`"),
            TokenKind::Eq => write!(f, "`=`"),
            TokenKind::Ne => write!(f, "`!=`"),
            TokenKind::Lt => write!(f, "`<`"),
            TokenKind::Le => write!(f, "`<=`"),
            TokenKind::Gt => write!(f, "`>`"),
            TokenKind::Ge => write!(f, "`>=`"),
            TokenKind::Eof => write!(f, "end of query"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    /// 1-based column of the first character of this token.
    pub col: usize,
}

/// A query parse failure, carrying enough context to render a caret diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryError {
    pub message: String,
    /// 1-based column the error refers to.
    pub col: usize,
    /// The query the error came from, for rendering.
    pub query: String,
}

impl QueryError {
    pub fn new(message: impl Into<String>, col: usize, query: &str) -> Self {
        QueryError {
            message: message.into(),
            col,
            query: query.to_string(),
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Render the query on one line with a caret under the failing column.
        // The query is displayed verbatim; tabs are turned into spaces so the
        // caret lines up.
        let shown: String = self
            .query
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .collect();
        let pad = " ".repeat(self.col.saturating_sub(1));
        write!(
            f,
            "query error at column {}: {}\n  {}\n  {}^",
            self.col, self.message, shown, pad
        )
    }
}

impl std::error::Error for QueryError {}

pub fn tokenize(query: &str) -> Result<Vec<Token>, QueryError> {
    Lexer::new(query).run()
}

struct Lexer<'a> {
    src: &'a str,
    chars: Vec<char>,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer {
            src,
            chars: src.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// 1-based column of the current position.
    fn col(&self) -> usize {
        self.pos + 1
    }

    fn err(&self, msg: impl Into<String>, col: usize) -> QueryError {
        QueryError::new(msg, col, self.src)
    }

    fn run(mut self) -> Result<Vec<Token>, QueryError> {
        let mut out = Vec::new();
        loop {
            self.skip_ignorable();
            let col = self.col();
            let Some(c) = self.peek() else {
                out.push(Token {
                    kind: TokenKind::Eof,
                    col,
                });
                return Ok(out);
            };

            let kind = match c {
                '*' => {
                    self.bump();
                    TokenKind::Star
                }
                ',' => {
                    self.bump();
                    TokenKind::Comma
                }
                '(' => {
                    self.bump();
                    TokenKind::LParen
                }
                ')' => {
                    self.bump();
                    TokenKind::RParen
                }
                '=' => {
                    self.bump();
                    // Accept `==` as a synonym for `=`; it is a common slip.
                    if self.peek() == Some('=') {
                        self.bump();
                    }
                    TokenKind::Eq
                }
                '!' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        TokenKind::Ne
                    } else {
                        return Err(self.err("expected `=` after `!`", col + 1));
                    }
                }
                '<' => {
                    self.bump();
                    match self.peek() {
                        Some('=') => {
                            self.bump();
                            TokenKind::Le
                        }
                        Some('>') => {
                            self.bump();
                            TokenKind::Ne
                        }
                        _ => TokenKind::Lt,
                    }
                }
                '>' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        TokenKind::Ge
                    } else {
                        TokenKind::Gt
                    }
                }
                '\'' | '"' => self.lex_string()?,
                c if c.is_ascii_digit() => self.lex_number()?,
                '-' | '+' if matches!(self.peek_at(1), Some(d) if d.is_ascii_digit() || d == '.') => {
                    self.lex_number()?
                }
                '.' if matches!(self.peek_at(1), Some(d) if d.is_ascii_digit()) => {
                    self.lex_number()?
                }
                c if is_ident_start(c) => self.lex_word(),
                other => {
                    return Err(self.err(format!("unexpected character `{other}`"), col));
                }
            };
            out.push(Token { kind, col });
        }
    }

    /// Skip whitespace and `--` comments, which run to the end of the line.
    ///
    /// A comment is only recognised where a token could start, so the `-` of a
    /// negative number is never mistaken for one. This is what makes queries
    /// stored in a file with `-f` readable.
    fn skip_ignorable(&mut self) {
        loop {
            while matches!(self.peek(), Some(c) if c.is_whitespace()) {
                self.bump();
            }
            if self.peek() == Some('-') && self.peek_at(1) == Some('-') {
                while matches!(self.peek(), Some(c) if c != '\n') {
                    self.bump();
                }
                continue;
            }
            return;
        }
    }

    /// Lex a quoted string. Both `'` and `"` delimit; the escape sequences are
    /// the JSON ones plus an escaped copy of the delimiter itself.
    fn lex_string(&mut self) -> Result<TokenKind, QueryError> {
        let open_col = self.col();
        let quote = self.bump().expect("caller checked");
        let mut out = String::new();
        loop {
            let col = self.col();
            match self.bump() {
                None => {
                    return Err(self.err(
                        format!("unterminated string literal, expected closing `{quote}`"),
                        open_col,
                    ))
                }
                Some(c) if c == quote => return Ok(TokenKind::Str(out)),
                Some('\\') => {
                    let esc_col = self.col();
                    match self.bump() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('0') => out.push('\0'),
                        Some('\\') => out.push('\\'),
                        Some('\'') => out.push('\''),
                        Some('"') => out.push('"'),
                        Some('%') => {
                            // Keep the backslash so the `like` matcher can see
                            // that this `%` is a literal, not a wildcard.
                            out.push('\\');
                            out.push('%');
                        }
                        Some(other) => {
                            return Err(
                                self.err(format!("unknown escape `\\{other}`"), esc_col)
                            )
                        }
                        None => {
                            return Err(self.err("unterminated escape sequence", esc_col));
                        }
                    }
                }
                Some(c) => {
                    let _ = col;
                    out.push(c);
                }
            }
        }
    }

    fn lex_number(&mut self) -> Result<TokenKind, QueryError> {
        let start = self.pos;
        let col = self.col();
        if matches!(self.peek(), Some('-') | Some('+')) {
            self.bump();
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            let save = self.pos;
            self.bump();
            if matches!(self.peek(), Some('-') | Some('+')) {
                self.bump();
            }
            if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.bump();
                }
            } else {
                // Not actually an exponent (e.g. `1error`); rewind.
                self.pos = save;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map(TokenKind::Num)
            .map_err(|_| self.err(format!("invalid number literal `{text}`"), col))
    }

    fn lex_word(&mut self) -> TokenKind {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
        let word: String = self.chars[start..self.pos].iter().collect();
        match word.to_ascii_lowercase().as_str() {
            "select" => TokenKind::Select,
            "where" => TokenKind::Where,
            "order" => TokenKind::Order,
            "by" => TokenKind::By,
            "asc" => TokenKind::Asc,
            "desc" => TokenKind::Desc,
            "limit" => TokenKind::Limit,
            "and" => TokenKind::And,
            "or" => TokenKind::Or,
            "not" => TokenKind::Not,
            "like" => TokenKind::Like,
            "true" => TokenKind::Bool(true),
            "false" => TokenKind::Bool(false),
            "null" => TokenKind::Null,
            _ => TokenKind::Ident(word),
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '@' || c == '$'
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == '.' || c == '-' || c == '/'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(q: &str) -> Vec<TokenKind> {
        tokenize(q).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn lexes_a_full_query() {
        assert_eq!(
            kinds("select level, msg where level = \"error\" limit 20"),
            vec![
                TokenKind::Select,
                TokenKind::Ident("level".into()),
                TokenKind::Comma,
                TokenKind::Ident("msg".into()),
                TokenKind::Where,
                TokenKind::Ident("level".into()),
                TokenKind::Eq,
                TokenKind::Str("error".into()),
                TokenKind::Limit,
                TokenKind::Num(20.0),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            kinds("SELECT * WHERE a AND b OR NOT c LIKE d"),
            vec![
                TokenKind::Select,
                TokenKind::Star,
                TokenKind::Where,
                TokenKind::Ident("a".into()),
                TokenKind::And,
                TokenKind::Ident("b".into()),
                TokenKind::Or,
                TokenKind::Not,
                TokenKind::Ident("c".into()),
                TokenKind::Like,
                TokenKind::Ident("d".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_all_comparison_operators() {
        assert_eq!(
            kinds("= == != <> < <= > >="),
            vec![
                TokenKind::Eq,
                TokenKind::Eq,
                TokenKind::Ne,
                TokenKind::Ne,
                TokenKind::Lt,
                TokenKind::Le,
                TokenKind::Gt,
                TokenKind::Ge,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_dotted_identifiers() {
        assert_eq!(
            kinds("user.id http.status-code a/b"),
            vec![
                TokenKind::Ident("user.id".into()),
                TokenKind::Ident("http.status-code".into()),
                TokenKind::Ident("a/b".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_numbers() {
        assert_eq!(
            kinds("0 12 3.5 -2 +7 1e3 2.5e-2 .5"),
            vec![
                TokenKind::Num(0.0),
                TokenKind::Num(12.0),
                TokenKind::Num(3.5),
                TokenKind::Num(-2.0),
                TokenKind::Num(7.0),
                TokenKind::Num(1000.0),
                TokenKind::Num(0.025),
                TokenKind::Num(0.5),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn number_followed_by_letters_is_two_tokens() {
        assert_eq!(
            kinds("1e"),
            vec![TokenKind::Num(1.0), TokenKind::Ident("e".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn lexes_literals() {
        assert_eq!(
            kinds("true false null TRUE Null"),
            vec![
                TokenKind::Bool(true),
                TokenKind::Bool(false),
                TokenKind::Null,
                TokenKind::Bool(true),
                TokenKind::Null,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_strings_with_either_quote() {
        assert_eq!(
            kinds("'it''s' \"a b\""),
            vec![
                TokenKind::Str("it".into()),
                TokenKind::Str("s".into()),
                TokenKind::Str("a b".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn unescapes_string_escapes() {
        assert_eq!(
            kinds(r#""a\nb\tc\\d\"e""#),
            vec![TokenKind::Str("a\nb\tc\\d\"e".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn escaped_percent_keeps_its_backslash() {
        assert_eq!(
            kinds(r#""100\%""#),
            vec![TokenKind::Str("100\\%".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn unterminated_string_points_at_the_opening_quote() {
        let err = tokenize("where msg = \"oops").unwrap_err();
        assert_eq!(err.col, 13);
        assert!(err.message.contains("unterminated string"), "{}", err.message);
    }

    #[test]
    fn unknown_escape_is_an_error() {
        let err = tokenize(r#""a\qb""#).unwrap_err();
        assert!(err.message.contains("unknown escape"), "{}", err.message);
        assert_eq!(err.col, 4);
    }

    #[test]
    fn unexpected_character_reports_its_column() {
        let err = tokenize("select a # b").unwrap_err();
        assert_eq!(err.col, 10);
        assert!(err.message.contains('#'), "{}", err.message);
    }

    #[test]
    fn bang_without_equals_is_an_error() {
        let err = tokenize("a ! b").unwrap_err();
        assert!(err.message.contains("expected `=`"), "{}", err.message);
    }

    #[test]
    fn a_double_dash_comment_runs_to_the_end_of_the_line() {
        assert_eq!(
            kinds("select *  -- everything\nlimit 5 -- but not much"),
            vec![
                TokenKind::Select,
                TokenKind::Star,
                TokenKind::Limit,
                TokenKind::Num(5.0),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn a_comment_may_be_the_whole_query_text() {
        assert_eq!(kinds("-- nothing here"), vec![TokenKind::Eof]);
        assert_eq!(kinds(""), vec![TokenKind::Eof]);
    }

    #[test]
    fn consecutive_comment_lines_are_all_skipped() {
        assert_eq!(
            kinds("-- one\n-- two\nselect *"),
            vec![TokenKind::Select, TokenKind::Star, TokenKind::Eof]
        );
    }

    #[test]
    fn a_double_dash_inside_a_string_is_not_a_comment() {
        assert_eq!(
            kinds(r#""a -- b""#),
            vec![TokenKind::Str("a -- b".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn token_columns_are_one_based() {
        let toks = tokenize("  select *").unwrap();
        assert_eq!(toks[0].col, 3);
        assert_eq!(toks[1].col, 10);
        assert_eq!(toks[2].col, 11); // Eof, just past the end.
    }

    #[test]
    fn error_display_renders_a_caret() {
        let err = tokenize("select a # b").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("column 10"), "{text}");
        let caret_line = text.lines().next_back().unwrap();
        assert_eq!(caret_line, format!("  {}^", " ".repeat(9)));
    }
}
