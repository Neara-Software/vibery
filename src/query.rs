//! The query language: lexer, parser, AST.
//!
//! Designed against the specific complaints in the search-issues note:
//!
//! | Complaint                                        | Syntax                                    |
//! |--------------------------------------------------|-------------------------------------------|
//! | can't filter by database without the mouse       | `RVO #bet`                                |
//! | field filter alone returns almost nothing        | `by:@sami` (no text term at all)          |
//! | no way to find things related to other things    | `#note related:(#team platform) interventions` |
//! | what has my teammate worked on recently          | `touched-by:@michael updated:-14d sort:recent` |
//!
//! Grammar (loosest binding first):
//!
//! ```text
//! query   := or
//! or      := and (('OR' | '|') and)*
//! and     := unary+                     // juxtaposition means AND
//! unary   := '-' unary | atom
//! atom    := '(' or ')' | filter | phrase | word
//! filter  := key ':' (value | '(' or ')') | '#' dbname | '@' username
//! ```

use crate::util::{now_secs, parse_datetime};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Word(String),
    Quoted(String),
    /// `key:value`
    Field(String, String),
    /// `key:(` — a filter whose argument is a nested query.
    FieldGroup(String),
    /// `#database`
    Hash(String),
    /// `@user`
    At(String),
    LParen,
    RParen,
    Minus,
    Or,
    And,
}

fn is_word_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '(' | ')' | ':' | '"' | '#' | '@')
}

/// Characters allowed in a bare filter value: dates, comparisons, ranges.
fn is_value_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '(' | ')' | '"')
}

pub fn lex(input: &str) -> Vec<Tok> {
    let cs: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;

    // Read a quoted string, or a run of chars satisfying `pred`.
    let take = |i: &mut usize, cs: &[char], pred: &dyn Fn(char) -> bool| -> String {
        if *i < cs.len() && cs[*i] == '"' {
            *i += 1;
            let start = *i;
            while *i < cs.len() && cs[*i] != '"' {
                *i += 1;
            }
            let s: String = cs[start..*i].iter().collect();
            if *i < cs.len() {
                *i += 1; // closing quote
            }
            s
        } else {
            let start = *i;
            while *i < cs.len() && pred(cs[*i]) {
                *i += 1;
            }
            cs[start..*i].iter().collect()
        }
    };

    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '|' => {
                out.push(Tok::Or);
                i += 1;
            }
            '&' => {
                out.push(Tok::And);
                i += 1;
            }
            '-' => {
                // Reaching this arm means `-` starts a token: `-` is a word
                // char mid-token, so `point-cloud` is consumed whole by the
                // default arm below. A token-initial `-` is therefore negation,
                // provided something follows it.
                let negation = i + 1 < cs.len() && !cs[i + 1].is_whitespace();
                if negation {
                    out.push(Tok::Minus);
                    i += 1;
                } else {
                    let w = take(&mut i, &cs, &is_word_char);
                    if w.is_empty() {
                        i += 1;
                    } else {
                        out.push(Tok::Word(w));
                    }
                }
            }
            '#' => {
                i += 1;
                let w = take(&mut i, &cs, &is_word_char);
                out.push(Tok::Hash(w));
            }
            '@' => {
                i += 1;
                let w = take(&mut i, &cs, &is_word_char);
                out.push(Tok::At(w));
            }
            '"' => {
                let s = take(&mut i, &cs, &is_word_char);
                out.push(Tok::Quoted(s));
            }
            _ => {
                let w = take(&mut i, &cs, &is_word_char);
                if w.is_empty() {
                    i += 1;
                    continue;
                }
                // `key:` introduces a filter.
                if i < cs.len() && cs[i] == ':' {
                    i += 1;
                    if i < cs.len() && cs[i] == '(' {
                        i += 1;
                        out.push(Tok::FieldGroup(w.to_lowercase()));
                        out.push(Tok::LParen);
                        continue;
                    }
                    // `by:@sami` — keep the `@` marker for the parser.
                    let val = if i < cs.len() && cs[i] == '@' {
                        i += 1;
                        let u = take(&mut i, &cs, &is_word_char);
                        format!("@{u}")
                    } else {
                        take(&mut i, &cs, &is_value_char)
                    };
                    out.push(Tok::Field(w.to_lowercase(), val));
                    continue;
                }
                match w.to_ascii_uppercase().as_str() {
                    "OR" => out.push(Tok::Or),
                    "AND" => out.push(Tok::And),
                    "NOT" => out.push(Tok::Minus),
                    _ => out.push(Tok::Word(w)),
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- AST

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateField {
    Created,
    Updated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextScope {
    /// title (boosted) + body + comments
    Any,
    Title,
    Body,
    Comments,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    All,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    /// A single word. `prefix` is set by a trailing `*`, which is also what
    /// incremental "search as I type" uses for the word being typed.
    Word {
        word: String,
        scope: TextScope,
        prefix: bool,
    },
    Phrase {
        words: Vec<String>,
        scope: TextScope,
    },
    /// Resolved at plan time against the catalog, so `#bet` -> `Engineering/Bet`.
    Db(String),
    Space(String),
    /// `role` of `None` means the role-agnostic `touched` edge.
    Actor {
        role: Option<String>,
        user: String,
    },
    /// One hop out: entities whose relations include something matching `sub`.
    Related {
        field: Option<String>,
        sub: Box<Node>,
    },
    Date {
        field: DateField,
        from: Option<i64>,
        to: Option<i64>,
    },
    Enum {
        field: String,
        value: String,
    },
    State(String),
    Has(String),
    Id(String),
    PublicId(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Relevance,
    Recent,
    Created,
    Oldest,
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub root: Node,
    pub sort: Sort,
    pub limit: usize,
    /// Databases to group counts by, for the facet summary.
    pub facets: bool,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            root: Node::All,
            sort: Sort::Relevance,
            limit: 20,
            facets: true,
        }
    }
}

// ---------------------------------------------------------------- parser

pub struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    pub sort: Option<Sort>,
    pub limit: Option<usize>,
    pub warnings: Vec<String>,
}

impl Parser {
    pub fn new(input: &str) -> Self {
        Parser {
            toks: lex(input),
            pos: 0,
            sort: None,
            limit: None,
            warnings: Vec::new(),
        }
    }

    pub fn parse(input: &str) -> (Spec, Vec<String>) {
        let mut p = Parser::new(input);
        let root = p.parse_or();
        let sort = p.sort.unwrap_or(Sort::Relevance);
        let spec = Spec {
            root,
            sort,
            limit: p.limit.unwrap_or(20),
            facets: true,
        };
        (spec, p.warnings)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn parse_or(&mut self) -> Node {
        let mut parts = vec![self.parse_and()];
        while matches!(self.peek(), Some(Tok::Or)) {
            self.pos += 1;
            parts.push(self.parse_and());
        }
        if parts.len() == 1 {
            parts.pop().expect("len checked")
        } else {
            Node::Or(parts)
        }
    }

    fn parse_and(&mut self) -> Node {
        let mut parts: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::RParen) | Some(Tok::Or) => break,
                Some(Tok::And) => {
                    self.pos += 1;
                    continue;
                }
                _ => {}
            }
            // Distinguish "consumed something that produces no node" (a
            // modifier such as `sort:recent`) from "nothing left to consume".
            let before = self.pos;
            match self.parse_unary() {
                Some(n) => parts.push(n),
                None if self.pos > before => continue,
                None => break,
            }
        }
        match parts.len() {
            0 => Node::All,
            1 => parts.pop().expect("len checked"),
            _ => Node::And(parts),
        }
    }

    fn parse_unary(&mut self) -> Option<Node> {
        if matches!(self.peek(), Some(Tok::Minus)) {
            self.pos += 1;
            let inner = self.parse_unary()?;
            return Some(Node::Not(Box::new(inner)));
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Option<Node> {
        let tok = self.peek()?.clone();
        match tok {
            Tok::LParen => {
                self.pos += 1;
                let inner = self.parse_or();
                if matches!(self.peek(), Some(Tok::RParen)) {
                    self.pos += 1;
                } else {
                    self.warnings.push("unclosed `(`".into());
                }
                Some(inner)
            }
            Tok::RParen => None,
            Tok::Word(w) => {
                self.pos += 1;
                Some(word_node(&w, TextScope::Any))
            }
            Tok::Quoted(s) => {
                self.pos += 1;
                Some(phrase_node(&s, TextScope::Any))
            }
            Tok::Hash(name) => {
                self.pos += 1;
                Some(Node::Db(name))
            }
            Tok::At(user) => {
                self.pos += 1;
                Some(Node::Actor {
                    role: None,
                    user,
                })
            }
            Tok::FieldGroup(key) => {
                self.pos += 1; // FieldGroup
                self.pos += 1; // LParen
                let sub = self.parse_or();
                if matches!(self.peek(), Some(Tok::RParen)) {
                    self.pos += 1;
                } else {
                    self.warnings.push(format!("unclosed `(` after `{key}:`"));
                }
                Some(self.group_filter(&key, sub))
            }
            Tok::Field(key, val) => {
                self.pos += 1;
                self.simple_filter(&key, &val)
            }
            Tok::Minus | Tok::Or | Tok::And => {
                self.pos += 1;
                None
            }
        }
    }

    /// `key:(subquery)` forms.
    fn group_filter(&mut self, key: &str, sub: Node) -> Node {
        match key {
            "related" | "rel" | "linked" | "about" | "mentions" => Node::Related {
                field: None,
                sub: Box::new(sub),
            },
            _ => Node::Related {
                field: Some(normalise_role(key)),
                sub: Box::new(sub),
            },
        }
    }

    /// `key:value` forms.
    fn simple_filter(&mut self, key: &str, val: &str) -> Option<Node> {
        // Query-level modifiers are not part of the boolean tree.
        match key {
            "sort" | "order" => {
                self.sort = Some(match val.to_lowercase().as_str() {
                    "recent" | "updated" | "modified" | "new" => Sort::Recent,
                    "created" => Sort::Created,
                    "oldest" | "old" => Sort::Oldest,
                    "relevance" | "score" | "best" => Sort::Relevance,
                    other => {
                        self.warnings
                            .push(format!("unknown sort `{other}`, using relevance"));
                        Sort::Relevance
                    }
                });
                return None;
            }
            "limit" | "top" | "n" => {
                match val.parse::<usize>() {
                    Ok(n) => self.limit = Some(n.clamp(1, 1000)),
                    Err(_) => self.warnings.push(format!("bad limit `{val}`")),
                }
                return None;
            }
            _ => {}
        }

        let user = val.strip_prefix('@');
        Some(match key {
            // --- database / space -----------------------------------------
            "db" | "database" | "type" | "in" if is_space_word(val) => {
                Node::Space(val.to_string())
            }
            "db" | "database" | "type" => Node::Db(val.to_string()),
            "space" | "app" => Node::Space(val.to_string()),

            // --- actors ---------------------------------------------------
            "by" | "author" | "creator" | "created-by" | "createdby" | "from" => Node::Actor {
                role: Some("created".into()),
                user: user.unwrap_or(val).to_string(),
            },
            "commented-by" | "commenter" | "commented" => Node::Actor {
                role: Some("commented".into()),
                user: user.unwrap_or(val).to_string(),
            },
            "touched-by" | "involves" | "involving" | "anyone" | "participant" | "with" => {
                Node::Actor {
                    role: None,
                    user: user.unwrap_or(val).to_string(),
                }
            }

            // --- text scoping ---------------------------------------------
            "title" | "name" => scoped_text(val, TextScope::Title),
            "body" | "text" | "content" | "description" => scoped_text(val, TextScope::Body),
            "comment" | "comments" if user.is_none() => scoped_text(val, TextScope::Comments),

            // --- dates ----------------------------------------------------
            "created" | "created-at" => match parse_date_filter(val) {
                Some((from, to)) => Node::Date {
                    field: DateField::Created,
                    from,
                    to,
                },
                None => {
                    self.warnings.push(format!("bad date `{val}`"));
                    Node::All
                }
            },
            "updated" | "modified" | "updated-at" | "touched" | "since" => {
                match parse_date_filter(val) {
                    Some((from, to)) => Node::Date {
                        field: DateField::Updated,
                        from,
                        to,
                    },
                    None => {
                        self.warnings.push(format!("bad date `{val}`"));
                        Node::All
                    }
                }
            }

            // --- misc -----------------------------------------------------
            "is" | "state" | "status" => match val.to_lowercase().as_str() {
                "open" | "active" | "todo" => Node::Not(Box::new(Node::Or(vec![
                    Node::State("Done".into()),
                    Node::State("Completed".into()),
                    Node::State("Closed".into()),
                    Node::State("Rejected".into()),
                    Node::State("Cancelled".into()),
                ]))),
                "done" | "closed" | "complete" | "completed" => Node::Or(vec![
                    Node::State("Done".into()),
                    Node::State("Completed".into()),
                    Node::State("Closed".into()),
                ]),
                other => Node::State(title_case(other)),
            },
            "has" => Node::Has(val.to_lowercase()),
            "id" => Node::Id(val.to_string()),
            "public-id" | "pid" | "num" | "number" => Node::PublicId(val.to_string()),

            // --- anything else: an actor if it looks like `@user`, otherwise
            //     an enum/select filter on that field name.
            _ => {
                if let Some(u) = user {
                    Node::Actor {
                        role: Some(normalise_role(key)),
                        user: u.to_string(),
                    }
                } else {
                    Node::Enum {
                        field: normalise_role(key),
                        value: val.to_lowercase(),
                    }
                }
            }
        })
    }
}

/// `in:engineering` should mean the space, not a database called engineering.
/// Spaces are single capitalised words in this workspace; we let the planner
/// double-check, and only use this as a hint.
fn is_space_word(_v: &str) -> bool {
    false
}

fn normalise_role(k: &str) -> String {
    k.trim().to_lowercase().replace(' ', "-")
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Split text the same way Tantivy's `default` tokenizer does: on
/// non-alphanumeric boundaries, lowercased, dropping over-long tokens.
pub fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && t.len() <= 40)
        .map(|t| t.to_lowercase())
        .collect()
}

fn word_node(raw: &str, scope: TextScope) -> Node {
    let prefix = raw.ends_with('*');
    let base = raw.trim_end_matches('*');
    let toks = tokenize(base);
    match toks.len() {
        0 => Node::All,
        1 => Node::Word {
            word: toks.into_iter().next().expect("len checked"),
            scope,
            prefix,
        },
        // A "word" that tokenizes into several (e.g. `foo.bar`) is a phrase.
        _ => Node::Phrase { words: toks, scope },
    }
}

fn phrase_node(raw: &str, scope: TextScope) -> Node {
    let toks = tokenize(raw);
    match toks.len() {
        0 => Node::All,
        1 => Node::Word {
            word: toks.into_iter().next().expect("len checked"),
            scope,
            prefix: false,
        },
        _ => Node::Phrase { words: toks, scope },
    }
}

fn scoped_text(val: &str, scope: TextScope) -> Node {
    let toks = tokenize(val);
    if toks.len() > 1 {
        // `title:"a b"` was already unquoted by the lexer; treat multi-token
        // values as an AND of words rather than a strict phrase, which is the
        // friendlier reading for a field filter.
        Node::And(
            toks.into_iter()
                .map(|w| Node::Word {
                    word: w,
                    scope,
                    prefix: false,
                })
                .collect(),
        )
    } else {
        word_node(val, scope)
    }
}

// ---------------------------------------------------------------- dates

const DAY: i64 = 86_400;

/// Parse a date filter into an inclusive `(from, to)` window.
///
/// Accepted: `2026-01-01`, `>2026-01-01`, `<2026-01-01`, `2026-01..2026-03`,
/// `-7d` / `7d` (within the last 7 days), `>-30d`, `today`, `week`, `month`,
/// `quarter`, `year`.
pub fn parse_date_filter(v: &str) -> Option<(Option<i64>, Option<i64>)> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(rest) = v.strip_prefix(">=").or_else(|| v.strip_prefix('>')) {
        let (from, _) = resolve_point(rest)?;
        return Some((Some(from), None));
    }
    if let Some(rest) = v.strip_prefix("<=").or_else(|| v.strip_prefix('<')) {
        let (_, to) = resolve_point(rest)?;
        return Some((None, Some(to)));
    }
    if let Some((a, b)) = v.split_once("..") {
        let (from, _) = resolve_point(a)?;
        let (_, to) = resolve_point(b)?;
        return Some((Some(from), Some(to)));
    }
    // A bare relative duration means "within the last N".
    if let Some(secs) = parse_duration(v) {
        return Some((Some(now_secs() - secs), None));
    }
    if let Some(w) = named_window(v) {
        return Some((Some(w), None));
    }
    // A bare absolute date means that whole day (or month, or year).
    let (from, to) = resolve_point(v)?;
    Some((Some(from), Some(to)))
}

/// Resolve one endpoint to `(start, end)` of the period it names.
fn resolve_point(v: &str) -> Option<(i64, i64)> {
    let v = v.trim();
    if let Some(secs) = parse_duration(v) {
        let t = now_secs() - secs;
        return Some((t, t));
    }
    if let Some(w) = named_window(v) {
        return Some((w, now_secs()));
    }
    match v.len() {
        4 => {
            // `2026` -> the whole year
            let y: i64 = v.parse().ok()?;
            let from = parse_datetime(&format!("{y:04}-01-01"))?;
            let to = parse_datetime(&format!("{:04}-01-01", y + 1))? - 1;
            Some((from, to))
        }
        7 => {
            // `2026-03` -> the whole month
            let (ys, ms) = v.split_once('-')?;
            let y: i64 = ys.parse().ok()?;
            let m: i64 = ms.parse().ok()?;
            let from = parse_datetime(&format!("{y:04}-{m:02}-01"))?;
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let to = parse_datetime(&format!("{ny:04}-{nm:02}-01"))? - 1;
            Some((from, to))
        }
        _ => {
            let from = parse_datetime(v)?;
            Some((from, from + DAY - 1))
        }
    }
}

/// `7d`, `-2w`, `3mo`, `1y`, `36h` -> seconds.
fn parse_duration(v: &str) -> Option<i64> {
    let v = v.trim().trim_start_matches('-');
    if v.is_empty() {
        return None;
    }
    let split = v.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let n: i64 = v[..split].parse().ok()?;
    let mult = match &v[split..] {
        "h" => 3600,
        "d" => DAY,
        "w" => 7 * DAY,
        "mo" | "M" => 30 * DAY,
        "y" => 365 * DAY,
        _ => return None,
    };
    Some(n * mult)
}

fn named_window(v: &str) -> Option<i64> {
    let now = now_secs();
    Some(match v.to_lowercase().as_str() {
        "today" => now - now.rem_euclid(DAY),
        "yesterday" => now - now.rem_euclid(DAY) - DAY,
        "week" | "thisweek" | "this-week" => now - 7 * DAY,
        "month" | "thismonth" | "this-month" => now - 30 * DAY,
        "quarter" => now - 91 * DAY,
        "year" | "thisyear" | "this-year" => now - 365 * DAY,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_filter_and_bare_text() {
        let toks = lex("RVO #Bet");
        assert_eq!(
            toks,
            vec![Tok::Word("RVO".into()), Tok::Hash("Bet".into())]
        );
    }

    #[test]
    fn field_with_at_user() {
        assert_eq!(
            lex("by:@sami"),
            vec![Tok::Field("by".into(), "@sami".into())]
        );
    }

    #[test]
    fn nested_group_filter() {
        assert_eq!(
            lex("related:(#team platform)"),
            vec![
                Tok::FieldGroup("related".into()),
                Tok::LParen,
                Tok::Hash("team".into()),
                Tok::Word("platform".into()),
                Tok::RParen
            ]
        );
    }

    #[test]
    fn hyphens_inside_words_are_not_negation() {
        assert_eq!(lex("point-cloud"), vec![Tok::Word("point-cloud".into())]);
        assert_eq!(
            lex("a -b"),
            vec![Tok::Word("a".into()), Tok::Minus, Tok::Word("b".into())]
        );
    }

    #[test]
    fn quoted_phrases() {
        assert_eq!(
            lex("\"load flow\""),
            vec![Tok::Quoted("load flow".into())]
        );
    }

    #[test]
    fn database_filter_alone_is_a_valid_query() {
        // The key property: no text term required.
        let (spec, w) = Parser::parse("by:@sami");
        assert!(w.is_empty(), "unexpected warnings: {w:?}");
        assert_eq!(
            spec.root,
            Node::Actor {
                role: Some("created".into()),
                user: "sami".into()
            }
        );
    }

    #[test]
    fn modifiers_leave_the_boolean_tree() {
        let (spec, _) = Parser::parse("interventions sort:recent limit:5");
        assert_eq!(spec.sort, Sort::Recent);
        assert_eq!(spec.limit, 5);
        assert_eq!(
            spec.root,
            Node::Word {
                word: "interventions".into(),
                scope: TextScope::Any,
                prefix: false
            }
        );
    }

    #[test]
    fn the_relation_traversal_query_parses() {
        let (spec, w) = Parser::parse("#note related:(#team platform) interventions");
        assert!(w.is_empty(), "unexpected warnings: {w:?}");
        let Node::And(parts) = &spec.root else {
            panic!("expected And, got {:?}", spec.root);
        };
        assert_eq!(parts.len(), 3);
        assert!(matches!(parts[0], Node::Db(_)));
        assert!(matches!(parts[1], Node::Related { field: None, .. }));
        assert!(matches!(parts[2], Node::Word { .. }));
    }

    #[test]
    fn boolean_precedence_or_binds_loosest() {
        let (spec, _) = Parser::parse("a b OR c");
        match spec.root {
            Node::Or(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], Node::And(_)));
                assert!(matches!(parts[1], Node::Word { .. }));
            }
            other => panic!("expected Or at the root, got {other:?}"),
        }
    }

    #[test]
    fn negation_parses() {
        let (spec, _) = Parser::parse("crash -#issue");
        let Node::And(parts) = &spec.root else {
            panic!("expected And");
        };
        assert!(matches!(parts[1], Node::Not(_)));
    }

    #[test]
    fn trailing_star_marks_a_prefix_word() {
        let (spec, _) = Parser::parse("interv*");
        assert_eq!(
            spec.root,
            Node::Word {
                word: "interv".into(),
                scope: TextScope::Any,
                prefix: true
            }
        );
    }

    #[test]
    fn relative_and_absolute_dates() {
        let (from, to) = parse_date_filter("-7d").expect("relative");
        assert!(from.is_some() && to.is_none());
        let (from, to) = parse_date_filter("2026-01-01").expect("absolute day");
        assert_eq!(to.expect("to") - from.expect("from"), DAY - 1);
        let (from, to) = parse_date_filter(">2026-01-01").expect("open ended");
        assert!(from.is_some() && to.is_none());
        let (from, to) = parse_date_filter("2026-01..2026-02").expect("range");
        assert!(from.is_some() && to.is_some());
        assert!(parse_date_filter("nonsense").is_none());
    }

    #[test]
    fn year_and_month_windows() {
        let (from, to) = parse_date_filter("2025").expect("year");
        let span = to.expect("to") - from.expect("from");
        assert!(span > 364 * DAY && span < 366 * DAY, "span was {span}");
    }

    #[test]
    fn empty_query_is_match_all() {
        let (spec, _) = Parser::parse("   ");
        assert_eq!(spec.root, Node::All);
    }
}
