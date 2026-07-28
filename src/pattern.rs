//! A small backtracking regular-expression engine with named captures.
//!
//! This exists so that `--pattern` can turn plain-text logs into records
//! without pulling in a regular-expression dependency. The supported syntax is
//! the subset that log patterns actually use:
//!
//! ```text
//! (?<name>…) (?P<name>…)  named capture group
//! (?:…)                   group, no capture
//! (…)                     group, numbered capture
//! |                       alternation
//! .                       any character except newline
//! [a-z0-9_] [^ ]          character class, ranges and negation
//! \d \D \w \W \s \S       digit / word / space classes and complements
//! * + ? {m} {m,} {m,n}    greedy repetition
//! *? +? ?? {m,n}?         lazy repetition
//! ^ $                     start and end of the line
//! \. \\ \n \t \r \x41     escapes
//! ```
//!
//! Matching is a straightforward recursive backtracker in continuation-passing
//! style. That is exponential on pathological patterns, which a hand-written
//! log pattern is not; in exchange the whole engine is a few hundred lines and
//! the dependency list stays at two crates.

use std::fmt;

/// A pattern that failed to compile, carrying the column so the CLI can render
/// the same caret diagram query errors use.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternError {
    pub message: String,
    /// 1-based column within the pattern.
    pub col: usize,
    pub pattern: String,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pad = " ".repeat(self.col.saturating_sub(1));
        write!(
            f,
            "pattern error at column {}: {}\n  {}\n  {}^",
            self.col, self.message, self.pattern, pad
        )
    }
}

impl std::error::Error for PatternError {}

#[derive(Debug, Clone, PartialEq)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// Matches the empty string.
    Empty,
    Char(char),
    /// `.` — any character except a newline.
    Any,
    Class {
        items: Vec<ClassItem>,
        negated: bool,
    },
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
    Group {
        /// `None` for `(?:…)`.
        index: Option<usize>,
        node: Box<Node>,
    },
    Start,
    End,
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    root: Node,
    /// Group index to capture name. Unnamed groups hold `None`.
    names: Vec<Option<String>>,
}

impl Pattern {
    /// Compile a pattern, or report where it went wrong.
    pub fn compile(src: &str) -> Result<Pattern, PatternError> {
        let mut p = Parser {
            src,
            chars: src.chars().collect(),
            pos: 0,
            names: Vec::new(),
        };
        let root = p.parse_alt()?;
        if p.pos < p.chars.len() {
            // Only an unbalanced `)` can get here.
            return Err(p.err("unmatched `)`", p.pos + 1));
        }
        if !p.names.iter().any(|n| n.is_some()) {
            return Err(PatternError {
                message: "pattern has no named capture groups; write `(?<name>…)` for each field \
                          you want to extract"
                    .to_string(),
                col: 1,
                pattern: src.to_string(),
            });
        }
        Ok(Pattern {
            root,
            names: p.names,
        })
    }

    /// The capture names, in the order they appear in the pattern.
    pub fn capture_names(&self) -> Vec<&str> {
        self.names.iter().filter_map(|n| n.as_deref()).collect()
    }

    /// Match `text` and return the named captures.
    ///
    /// The pattern is searched for anywhere in the line unless it is anchored
    /// with `^`. A group that took part in no alternative is absent from the
    /// result rather than empty, so a query can tell "did not match" from
    /// "matched nothing".
    pub fn captures(&self, text: &str) -> Option<Vec<(String, String)>> {
        let chars: Vec<char> = text.chars().collect();
        let mut caps: Vec<Option<(usize, usize)>> = vec![None; self.names.len()];
        for start in 0..=chars.len() {
            caps.iter_mut().for_each(|c| *c = None);
            if match_node(&self.root, &chars, start, &mut caps, &mut |_, _| true) {
                let mut out = Vec::new();
                for (i, name) in self.names.iter().enumerate() {
                    let Some(name) = name else { continue };
                    if let Some((s, e)) = caps[i] {
                        out.push((name.clone(), chars[s..e].iter().collect()));
                    }
                }
                return Some(out);
            }
            // A pattern anchored at the start cannot match further along.
            if starts_anchored(&self.root) {
                break;
            }
        }
        None
    }
}

/// True when every alternative of the pattern begins with `^`.
fn starts_anchored(node: &Node) -> bool {
    match node {
        Node::Start => true,
        Node::Concat(parts) => parts.first().map(starts_anchored).unwrap_or(false),
        Node::Group { node, .. } => starts_anchored(node),
        Node::Alt(alts) => alts.iter().all(starts_anchored),
        _ => false,
    }
}

// --- matching -------------------------------------------------------------

type Caps = Vec<Option<(usize, usize)>>;

/// Match `node` at `pos`, calling `k` with the position after the match.
///
/// `k` returns whether the rest of the pattern matched from there; returning
/// false makes this function try its next alternative, which is what gives the
/// engine its backtracking.
fn match_node(
    node: &Node,
    text: &[char],
    pos: usize,
    caps: &mut Caps,
    k: &mut dyn FnMut(usize, &mut Caps) -> bool,
) -> bool {
    match node {
        Node::Empty => k(pos, caps),
        Node::Start => pos == 0 && k(pos, caps),
        Node::End => pos == text.len() && k(pos, caps),
        Node::Char(c) => pos < text.len() && text[pos] == *c && k(pos + 1, caps),
        Node::Any => pos < text.len() && text[pos] != '\n' && k(pos + 1, caps),
        Node::Class { items, negated } => {
            pos < text.len()
                && class_matches(items, *negated, text[pos])
                && k(pos + 1, caps)
        }
        Node::Concat(parts) => match_seq(parts, text, pos, caps, k),
        Node::Alt(alts) => alts.iter().any(|a| match_node(a, text, pos, caps, k)),
        Node::Group { index, node } => {
            let idx = *index;
            match_node(node, text, pos, caps, &mut |end, caps| {
                match idx {
                    Some(i) => {
                        let saved = caps[i];
                        caps[i] = Some((pos, end));
                        if k(end, caps) {
                            true
                        } else {
                            caps[i] = saved;
                            false
                        }
                    }
                    None => k(end, caps),
                }
            })
        }
        Node::Repeat {
            node,
            min,
            max,
            greedy,
        } => match_repeat(node, *min, *max, *greedy, text, pos, caps, 0, k),
    }
}

fn match_seq(
    parts: &[Node],
    text: &[char],
    pos: usize,
    caps: &mut Caps,
    k: &mut dyn FnMut(usize, &mut Caps) -> bool,
) -> bool {
    match parts.split_first() {
        None => k(pos, caps),
        Some((head, rest)) => match_node(head, text, pos, caps, &mut |p, c| {
            match_seq(rest, text, p, c, k)
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn match_repeat(
    node: &Node,
    min: u32,
    max: Option<u32>,
    greedy: bool,
    text: &[char],
    pos: usize,
    caps: &mut Caps,
    done: u32,
    k: &mut dyn FnMut(usize, &mut Caps) -> bool,
) -> bool {
    // Lazy repetition takes the exit first, as long as the minimum is met.
    if !greedy && done >= min && k(pos, caps) {
        return true;
    }
    let can_take_more = max.map(|m| done < m).unwrap_or(true);
    // One more repetition. The `p == pos` guard stops a nullable body
    // (`(a?)*`) from looping for ever.
    if can_take_more
        && match_node(node, text, pos, caps, &mut |p, c| {
            if p == pos && done >= min {
                return false;
            }
            match_repeat(node, min, max, greedy, text, p, c, done + 1, k)
        })
    {
        return true;
    }
    // Greedy repetition takes the exit last.
    greedy && done >= min && k(pos, caps)
}

fn class_matches(items: &[ClassItem], negated: bool, c: char) -> bool {
    let hit = items.iter().any(|item| match item {
        ClassItem::Char(x) => *x == c,
        ClassItem::Range(a, b) => c >= *a && c <= *b,
        ClassItem::Digit(want) => c.is_ascii_digit() == *want,
        ClassItem::Word(want) => (c.is_alphanumeric() || c == '_') == *want,
        ClassItem::Space(want) => c.is_whitespace() == *want,
    });
    hit != negated
}

// --- parsing --------------------------------------------------------------

struct Parser<'a> {
    src: &'a str,
    chars: Vec<char>,
    pos: usize,
    names: Vec<Option<String>>,
}

impl Parser<'_> {
    fn err(&self, message: impl Into<String>, col: usize) -> PatternError {
        PatternError {
            message: message.into(),
            col,
            pattern: self.src.to_string(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn parse_alt(&mut self) -> Result<Node, PatternError> {
        let mut alts = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.bump();
            alts.push(self.parse_concat()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().expect("just checked")
        } else {
            Node::Alt(alts)
        })
    }

    fn parse_concat(&mut self) -> Result<Node, PatternError> {
        let mut parts = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.parse_atom()?;
            parts.push(self.parse_quantifier(atom)?);
        }
        Ok(match parts.len() {
            0 => Node::Empty,
            1 => parts.pop().expect("just checked"),
            _ => Node::Concat(parts),
        })
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node, PatternError> {
        let col = self.pos + 1;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.bump();
                (0, None)
            }
            Some('+') => {
                self.bump();
                (1, None)
            }
            Some('?') => {
                self.bump();
                (0, Some(1))
            }
            Some('{') if self.looks_like_counted_repeat() => {
                self.bump();
                let min = self.parse_count(col)?;
                let max = if self.peek() == Some(',') {
                    self.bump();
                    if self.peek() == Some('}') {
                        None
                    } else {
                        Some(self.parse_count(col)?)
                    }
                } else {
                    Some(min)
                };
                if self.bump() != Some('}') {
                    return Err(self.err("expected `}` to close the repeat count", self.pos + 1));
                }
                if let Some(m) = max {
                    if m < min {
                        return Err(self.err("repeat count is empty: the maximum is below the minimum", col));
                    }
                }
                (min, max)
            }
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Start | Node::End) {
            return Err(self.err("an anchor cannot be repeated", col));
        }
        let greedy = if self.peek() == Some('?') {
            self.bump();
            false
        } else {
            true
        };
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    /// `{` only starts a repeat when it is followed by digits; otherwise it is
    /// a literal brace, which log patterns use often enough to matter.
    fn looks_like_counted_repeat(&self) -> bool {
        matches!(self.chars.get(self.pos + 1), Some(c) if c.is_ascii_digit())
    }

    fn parse_count(&mut self, col: usize) -> Result<u32, PatternError> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.bump();
        }
        if start == self.pos {
            return Err(self.err("expected a number in the repeat count", self.pos + 1));
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| self.err("repeat count is too large", col))
    }

    fn parse_atom(&mut self) -> Result<Node, PatternError> {
        let col = self.pos + 1;
        match self.bump() {
            Some('(') => self.parse_group(col),
            Some('[') => self.parse_class(col),
            Some('.') => Ok(Node::Any),
            Some('^') => Ok(Node::Start),
            Some('$') => Ok(Node::End),
            Some('\\') => self.parse_escape(col),
            Some(c @ ('*' | '+' | '?')) => {
                Err(self.err(format!("nothing for `{c}` to repeat"), col))
            }
            Some(c) => Ok(Node::Char(c)),
            None => Ok(Node::Empty),
        }
    }

    fn parse_group(&mut self, open_col: usize) -> Result<Node, PatternError> {
        let mut index = None;
        if self.peek() == Some('?') {
            self.bump();
            match self.peek() {
                Some(':') => {
                    self.bump();
                }
                Some('<') | Some('\'') => {
                    let name = self.parse_group_name()?;
                    index = Some(self.names.len());
                    self.names.push(Some(name));
                }
                Some('P') => {
                    self.bump();
                    if self.peek() != Some('<') {
                        return Err(self.err(
                            "expected `<name>` after `(?P`",
                            self.pos + 1,
                        ));
                    }
                    let name = self.parse_group_name()?;
                    index = Some(self.names.len());
                    self.names.push(Some(name));
                }
                _ => {
                    return Err(self.err(
                        "unsupported group flag; use `(?<name>…)` or `(?:…)`",
                        self.pos + 1,
                    ))
                }
            }
        } else {
            index = Some(self.names.len());
            self.names.push(None);
        }
        let inner = self.parse_alt()?;
        if self.bump() != Some(')') {
            return Err(self.err("unclosed group, expected `)`", open_col));
        }
        Ok(Node::Group {
            index,
            node: Box::new(inner),
        })
    }

    fn parse_group_name(&mut self) -> Result<String, PatternError> {
        let open = self.bump().expect("caller peeked");
        let close = if open == '<' { '>' } else { '\'' };
        let col = self.pos + 1;
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c != close) {
            self.bump();
        }
        if self.bump() != Some(close) {
            return Err(self.err(format!("unclosed capture name, expected `{close}`"), col));
        }
        let name: String = self.chars[start..self.pos - 1].iter().collect();
        if name.is_empty() {
            return Err(self.err("capture name is empty", col));
        }
        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return Err(self.err(
                format!("capture name `{name}` may only contain letters, digits, `_` and `.`"),
                col,
            ));
        }
        if self.names.iter().flatten().any(|n| n == &name) {
            return Err(self.err(format!("duplicate capture name `{name}`"), col));
        }
        Ok(name)
    }

    fn parse_class(&mut self, open_col: usize) -> Result<Node, PatternError> {
        let negated = if self.peek() == Some('^') {
            self.bump();
            true
        } else {
            false
        };
        let mut items = Vec::new();
        // A `]` in the first position is a literal, as in POSIX.
        if self.peek() == Some(']') {
            self.bump();
            items.push(ClassItem::Char(']'));
        }
        loop {
            let col = self.pos + 1;
            match self.bump() {
                None => return Err(self.err("unclosed character class, expected `]`", open_col)),
                Some(']') => break,
                Some('\\') => {
                    let c = self.bump().ok_or_else(|| {
                        self.err("unfinished escape in character class", col)
                    })?;
                    match class_escape(c) {
                        Some(item) => items.push(item),
                        None => match simple_escape(c) {
                            Some(ch) => items.push(ClassItem::Char(ch)),
                            None => {
                                return Err(
                                    self.err(format!("unknown escape `\\{c}` in class"), col)
                                )
                            }
                        },
                    }
                }
                Some(lo) => {
                    // A `-` between two characters is a range.
                    if self.peek() == Some('-')
                        && !matches!(self.chars.get(self.pos + 1), Some(']') | None)
                    {
                        self.bump();
                        let hi_col = self.pos + 1;
                        let hi = match self.bump() {
                            Some('\\') => {
                                let c = self.bump().ok_or_else(|| {
                                    self.err("unfinished escape in character class", hi_col)
                                })?;
                                simple_escape(c).ok_or_else(|| {
                                    self.err(format!("`\\{c}` cannot end a range"), hi_col)
                                })?
                            }
                            Some(c) => c,
                            None => {
                                return Err(
                                    self.err("unclosed character class, expected `]`", open_col)
                                )
                            }
                        };
                        if hi < lo {
                            return Err(self.err(
                                format!("range `{lo}-{hi}` runs backwards"),
                                col,
                            ));
                        }
                        items.push(ClassItem::Range(lo, hi));
                    } else {
                        items.push(ClassItem::Char(lo));
                    }
                }
            }
        }
        if items.is_empty() {
            return Err(self.err("empty character class", open_col));
        }
        Ok(Node::Class { items, negated })
    }

    fn parse_escape(&mut self, col: usize) -> Result<Node, PatternError> {
        let Some(c) = self.bump() else {
            return Err(self.err("pattern ends with a backslash", col));
        };
        if let Some(item) = class_escape(c) {
            return Ok(Node::Class {
                items: vec![item],
                negated: false,
            });
        }
        if c == 'x' {
            let start = self.pos;
            for _ in 0..2 {
                match self.peek() {
                    Some(h) if h.is_ascii_hexdigit() => {
                        self.bump();
                    }
                    _ => return Err(self.err("`\\x` needs two hex digits", col)),
                }
            }
            let hex: String = self.chars[start..self.pos].iter().collect();
            let value = u32::from_str_radix(&hex, 16).map_err(|_| {
                self.err(format!("`\\x{hex}` is not a valid escape"), col)
            })?;
            let ch = char::from_u32(value)
                .ok_or_else(|| self.err(format!("`\\x{hex}` is not a character"), col))?;
            return Ok(Node::Char(ch));
        }
        match simple_escape(c) {
            Some(ch) => Ok(Node::Char(ch)),
            None => Err(self.err(format!("unknown escape `\\{c}`"), col)),
        }
    }
}

fn class_escape(c: char) -> Option<ClassItem> {
    match c {
        'd' => Some(ClassItem::Digit(true)),
        'D' => Some(ClassItem::Digit(false)),
        'w' => Some(ClassItem::Word(true)),
        'W' => Some(ClassItem::Word(false)),
        's' => Some(ClassItem::Space(true)),
        'S' => Some(ClassItem::Space(false)),
        _ => None,
    }
}

fn simple_escape(c: char) -> Option<char> {
    match c {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        '0' => Some('\0'),
        c if !c.is_alphanumeric() => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(pattern: &str, text: &str) -> Option<Vec<(String, String)>> {
        Pattern::compile(pattern).unwrap().captures(text)
    }

    fn named(pattern: &str, text: &str, name: &str) -> Option<String> {
        caps(pattern, text)?
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    #[test]
    fn extracts_a_common_log_line() {
        let p = r#"^(?<ip>[\d.]+) \S+ \S+ \[(?<ts>[^\]]+)\] "(?<method>\w+) (?<path>\S+)[^"]*" (?<status>\d{3}) (?<bytes>\d+)$"#;
        let line = r#"10.0.0.7 - alice [27/Jul/2026:09:14:15 +0000] "GET /api/orders HTTP/1.1" 500 1734"#;
        let got = caps(p, line).unwrap();
        assert_eq!(
            got,
            vec![
                ("ip".to_string(), "10.0.0.7".to_string()),
                ("ts".to_string(), "27/Jul/2026:09:14:15 +0000".to_string()),
                ("method".to_string(), "GET".to_string()),
                ("path".to_string(), "/api/orders".to_string()),
                ("status".to_string(), "500".to_string()),
                ("bytes".to_string(), "1734".to_string()),
            ]
        );
    }

    #[test]
    fn both_named_group_spellings_work() {
        assert_eq!(named(r"(?<a>\d+)", "x42", "a").as_deref(), Some("42"));
        assert_eq!(named(r"(?P<a>\d+)", "x42", "a").as_deref(), Some("42"));
        assert_eq!(named(r"(?'a'\d+)", "x42", "a").as_deref(), Some("42"));
    }

    #[test]
    fn search_is_unanchored_but_honours_a_caret() {
        assert!(caps(r"(?<w>\w+)", "   hello").is_some());
        assert_eq!(named(r"(?<w>\w+)", "   hello", "w").as_deref(), Some("hello"));
        assert!(caps(r"^(?<w>\w+)$", "   hello").is_none());
    }

    #[test]
    fn alternation_picks_the_first_matching_branch() {
        let p = r"(?<level>ERROR|WARN|INFO)";
        assert_eq!(named(p, "a WARN b", "level").as_deref(), Some("WARN"));
        assert_eq!(named(p, "a INFO b", "level").as_deref(), Some("INFO"));
        assert!(caps(p, "a DEBUG b").is_none());
    }

    #[test]
    fn quantifiers_cover_every_form() {
        assert_eq!(named(r"a(?<x>b*)c", "ac", "x").as_deref(), Some(""));
        assert_eq!(named(r"a(?<x>b*)c", "abbbc", "x").as_deref(), Some("bbb"));
        assert!(caps(r"^a(?<x>b+)c$", "ac").is_none());
        assert_eq!(named(r"^(?<x>a?)b$", "b", "x").as_deref(), Some(""));
        assert_eq!(named(r"^(?<x>\d{3})$", "123", "x").as_deref(), Some("123"));
        assert!(caps(r"^(?<x>\d{3})$", "12").is_none());
        assert_eq!(named(r"^(?<x>\d{2,4})$", "12345", "x"), None);
        assert_eq!(named(r"(?<x>\d{2,})", "12345", "x").as_deref(), Some("12345"));
    }

    #[test]
    fn lazy_quantifiers_stop_early() {
        assert_eq!(named(r"<(?<x>.+?)>", "<a><b>", "x").as_deref(), Some("a"));
        assert_eq!(named(r"<(?<x>.+)>", "<a><b>", "x").as_deref(), Some("a><b"));
    }

    #[test]
    fn character_classes_and_negation() {
        assert_eq!(named(r"(?<x>[a-f0-9]+)", "zz beef 12", "x").as_deref(), Some("beef"));
        assert_eq!(named(r"(?<x>[^ ]+)", " abc def", "x").as_deref(), Some("abc"));
        assert_eq!(named(r"(?<x>[\]]+)", "a]]b", "x").as_deref(), Some("]]"));
        assert_eq!(named(r"(?<x>[-a]+)", "?-a-", "x").as_deref(), Some("-a-"));
    }

    #[test]
    fn shorthand_classes_match_the_right_sets() {
        assert_eq!(named(r"(?<x>\d+)", "ab123", "x").as_deref(), Some("123"));
        assert_eq!(named(r"(?<x>\D+)", "ab123", "x").as_deref(), Some("ab"));
        assert_eq!(named(r"(?<x>\s+)", "a  b", "x").as_deref(), Some("  "));
        assert_eq!(named(r"(?<x>\S+)", "  ab", "x").as_deref(), Some("ab"));
        assert_eq!(named(r"(?<x>\w+)", "  a_1", "x").as_deref(), Some("a_1"));
        assert_eq!(named(r"(?<x>\W+)", "a__ ..b", "x").as_deref(), Some(" .."));
    }

    #[test]
    fn escapes_are_literal() {
        assert_eq!(named(r"(?<x>a\.b)", "axb a.b", "x").as_deref(), Some("a.b"));
        assert_eq!(named(r"(?<x>\x41+)", "zAAz", "x").as_deref(), Some("AA"));
        assert_eq!(named(r"(?<x>a\tb)", "a\tb", "x").as_deref(), Some("a\tb"));
        assert_eq!(named(r"(?<x>\(\))", "()", "x").as_deref(), Some("()"));
    }

    #[test]
    fn non_capturing_groups_are_not_reported() {
        let p = Pattern::compile(r"(?:x)(?<a>y)").unwrap();
        assert_eq!(p.capture_names(), vec!["a"]);
        assert_eq!(p.captures("xy").unwrap().len(), 1);
    }

    #[test]
    fn unnamed_groups_group_without_capturing_output() {
        let got = caps(r"(?<a>(ab)+)", "ababab").unwrap();
        assert_eq!(got, vec![("a".to_string(), "ababab".to_string())]);
    }

    #[test]
    fn a_group_that_did_not_participate_is_absent() {
        let got = caps(r"^(?:(?<a>x)|(?<b>y))$", "y").unwrap();
        assert_eq!(got, vec![("b".to_string(), "y".to_string())]);
    }

    #[test]
    fn backtracking_finds_a_late_match() {
        assert_eq!(named(r"^(?<x>a*)ab$", "aaab", "x").as_deref(), Some("aa"));
    }

    #[test]
    fn nullable_repeat_terminates() {
        assert!(caps(r"^(?<x>(a?)*)$", "aaa").is_some());
        assert!(caps(r"^(?<x>(a?)*)b$", "aaa").is_none());
    }

    #[test]
    fn multibyte_text_is_matched_by_character() {
        assert_eq!(named(r"(?<x>\S+)", " héllo ", "x").as_deref(), Some("héllo"));
        assert_eq!(named(r"(?<x>.{2})", "日本語", "x").as_deref(), Some("日本"));
    }

    #[test]
    fn capture_names_are_reported_in_order() {
        let p = Pattern::compile(r"(?<a>x) (?<b>y) (?<c>z)").unwrap();
        assert_eq!(p.capture_names(), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_pattern_without_names_is_rejected() {
        let e = Pattern::compile(r"\d+").unwrap_err();
        assert!(e.message.contains("no named capture"), "{}", e.message);
    }

    #[test]
    fn compile_errors_point_at_the_column() {
        let e = Pattern::compile(r"(?<a>\d+").unwrap_err();
        assert!(e.message.contains("unclosed group"), "{}", e.message);
        assert_eq!(e.col, 1);

        let e = Pattern::compile(r"(?<a>[0-9)").unwrap_err();
        assert!(e.message.contains("character class"), "{}", e.message);
        assert_eq!(e.col, 6);

        let e = Pattern::compile(r"(?<a>x)*+").unwrap_err();
        assert!(e.message.contains("repeat"), "{}", e.message);

        let e = Pattern::compile(r"(?<>x)").unwrap_err();
        assert!(e.message.contains("empty"), "{}", e.message);

        let e = Pattern::compile(r"(?<a>x)(?<a>y)").unwrap_err();
        assert!(e.message.contains("duplicate"), "{}", e.message);

        let e = Pattern::compile(r"(?<a>[z-a])").unwrap_err();
        assert!(e.message.contains("backwards"), "{}", e.message);

        let e = Pattern::compile(r"(?<a>\q)").unwrap_err();
        assert!(e.message.contains("unknown escape"), "{}", e.message);

        let e = Pattern::compile(r"(?#a)").unwrap_err();
        assert!(e.message.contains("unsupported group flag"), "{}", e.message);

        let e = Pattern::compile(r"(?<a>x))").unwrap_err();
        assert!(e.message.contains("unmatched"), "{}", e.message);
    }

    #[test]
    fn error_display_renders_a_caret() {
        let e = Pattern::compile(r"(?<a>[0-9)").unwrap_err();
        let text = e.to_string();
        assert!(text.starts_with("pattern error at column 6"), "{text}");
        assert_eq!(text.lines().next_back().unwrap(), format!("  {}^", " ".repeat(5)));
    }

    #[test]
    fn a_literal_brace_is_not_a_repeat() {
        assert_eq!(named(r"(?<x>\w+\{)", "ab{", "x").as_deref(), Some("ab{"));
        assert_eq!(named(r"(?<x>a{b)", "a{b", "x").as_deref(), Some("a{b"));
    }

    #[test]
    fn no_match_returns_none() {
        assert!(caps(r"^(?<a>\d+)$", "not a number").is_none());
    }
}
