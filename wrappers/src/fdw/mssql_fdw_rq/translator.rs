//! Pure PostgreSQL-SQL → T-SQL translator for full-query pushdown (TZ §4.3).
//!
//! Input is a deparsed PostgreSQL statement (PG dialect) as delivered by the
//! framework's `FullQuery`. Output is a single valid T-SQL statement. The
//! translator is stateless: `translate(sql, ctx)` takes the relations mapping
//! (and the set of boolean columns) and nothing else, which keeps it fully
//! covered by plain unit tests. Anything outside the supported construct set
//! yields a structured [`TranslateError::UnsupportedConstruct`] — never
//! silently wrong SQL.

use std::collections::{HashMap, HashSet};
use std::fmt;

use super::types;

/// Remote name mapping for one foreign relation, built from the foreign table
/// options `schema` (default `dbo`) and `table`.
#[derive(Debug, Clone)]
pub struct RelationMapping {
    pub local_schema: String,
    pub local_table: String,
    pub remote_schema: String,
    pub remote_table: String,
}

/// Everything the translator needs besides the SQL itself.
#[derive(Debug, Clone, Default)]
pub struct TranslateContext {
    /// foreign relations referenced by the statement
    pub relations: Vec<RelationMapping>,
    /// local column names (lowercase) whose PostgreSQL type is `boolean`;
    /// bare references to them in predicate position become `= 1` / `= 0`
    pub bool_columns: Vec<String>,
    /// local column names (lowercase) that are NOT NULL — only those may be
    /// sorted without a NULL tiebreaker (PostgreSQL and T-SQL disagree on
    /// implicit NULL ordering: PG puts NULLs last for ASC / first for DESC,
    /// while T-SQL always treats NULL as the smallest value)
    pub not_null_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    /// A construct that has no faithful T-SQL equivalent in v1.
    UnsupportedConstruct {
        sql_fragment: String,
        reason: String,
    },
    /// An identifier that cannot be safely bracket-quoted.
    InvalidIdentifier(String),
    /// Lexical structure is broken.
    Unterminated(String),
}

impl fmt::Display for TranslateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedConstruct {
                sql_fragment,
                reason,
            } => write!(f, "unsupported construct '{sql_fragment}': {reason}"),
            Self::InvalidIdentifier(name) => {
                write!(f, "identifier '{name}' cannot be quoted safely for T-SQL")
            }
            Self::Unterminated(what) => write!(f, "unterminated {what}"),
        }
    }
}

impl std::error::Error for TranslateError {}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// bare identifier or keyword, original case
    Word(String),
    /// `"quoted identifier"`, value already unescaped
    QIdent(String),
    /// `'literal'` / `E'literal'`, value already unescaped
    Str(String),
    Num(String),
    /// `$n`, stores just the number
    Param(String),
    Op(String),
}

const MULTI_CHAR_OPS: [&str; 8] = ["::", "||", "<=", ">=", "<>", "!=", "<<", ">>"];
// characters that have a different meaning (or no meaning) in T-SQL and are
// therefore rejected instead of mistranslated
const FORBIDDEN_CHARS: [char; 5] = ['~', '^', '@', '#', '?'];

fn lex(src: &str) -> Result<Vec<Tok>, TranslateError> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // comments never appear in deparsed SQL, but tolerate them anyway
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }

        if c == '"' {
            i += 1;
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(TranslateError::Unterminated("quoted identifier".into())),
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        s.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(Tok::QIdent(s));
            continue;
        }

        if c == '\'' || (matches!(c, 'E' | 'e') && chars.get(i + 1) == Some(&'\'')) {
            let is_escape_string = c != '\'';
            i += if is_escape_string { 2 } else { 1 };
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(TranslateError::Unterminated("string literal".into())),
                    // E'' only: a backslash escapes the next character, so
                    // `\'` does not close the literal. Keep the pair raw and
                    // decode below so trailing-backslash forms match PG.
                    Some('\\') if is_escape_string => {
                        let Some(&next) = chars.get(i + 1) else {
                            return Err(TranslateError::Unterminated("string literal".into()));
                        };
                        s.push('\\');
                        s.push(next);
                        i += 2;
                    }
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            let s = if is_escape_string {
                decode_escape_string(&s)?
            } else {
                s
            };
            out.push(Tok::Str(s));
            continue;
        }

        if c == '$' {
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                s.push(chars[i]);
                i += 1;
            }
            if s.is_empty() {
                return Err(TranslateError::UnsupportedConstruct {
                    sql_fragment: "$".to_string(),
                    reason: "dollar-quoting is not supported".to_string(),
                });
            }
            out.push(Tok::Param(s));
            continue;
        }

        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()))
        {
            let mut s = String::new();
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                s.push(chars[i]);
                i += 1;
            }
            if matches!(chars.get(i), Some('e' | 'E')) {
                let save = i;
                let mut exp = String::new();
                exp.push(chars[i]);
                i += 1;
                if matches!(chars.get(i), Some('+' | '-')) {
                    exp.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() && chars[i].is_ascii_digit() {
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        exp.push(chars[i]);
                        i += 1;
                    }
                    s.push_str(&exp);
                } else {
                    i = save;
                }
            }
            out.push(Tok::Num(s));
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let mut s = String::new();
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                s.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Word(s));
            continue;
        }

        if FORBIDDEN_CHARS.contains(&c) {
            let reason = match c {
                '~' => "POSIX regex operators are not supported".to_string(),
                '^' => "power operator has no T-SQL equivalent (^ is XOR in T-SQL)".to_string(),
                _ => "character is not part of the supported PG-SQL subset".to_string(),
            };
            return Err(TranslateError::UnsupportedConstruct {
                sql_fragment: c.to_string(),
                reason,
            });
        }

        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if MULTI_CHAR_OPS.contains(&two.as_str()) {
            out.push(Tok::Op(two));
            i += 2;
            continue;
        }

        // '[' and ']' lex fine but are only valid inside ANY/ALL (ARRAY[…]);
        // the transform loop rejects them anywhere else
        if "()=<>+-*/%.,;&|![]".contains(c) {
            out.push(Tok::Op(c.to_string()));
            i += 1;
            continue;
        }

        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: c.to_string(),
            reason: "character is not part of the supported PG-SQL subset".to_string(),
        });
    }

    Ok(out)
}

/// Decode a PostgreSQL `E'…'` escape-string body (quote doubling and raw
/// backslash pairs are already in `s`). Escape meanings follow PostgreSQL:
/// `\b`, `\f`, `\n`, `\r`, `\t`, `\v`, and any other `\x` yields `x`
/// (this covers `\\`, `\'`, `\"`).
fn decode_escape_string(s: &str) -> Result<String, TranslateError> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            None => {
                return Err(TranslateError::Unterminated("escape sequence".into()));
            }
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\u{000B}'),
            Some(other) => out.push(other),
        }
    }
    Ok(out)
}

/// Match a LIMIT/OFFSET value token: the deparser prints constants as
/// `'3'::bigint` (string + cast) and parameters as `$1::bigint`.
/// Returns the value and how many extra tokens the cast tail occupies.
fn limit_value_at(toks: &[Tok], at: usize) -> Option<(LimitValue, usize)> {
    let (value, next) = match toks.get(at)? {
        Tok::Num(n) => (LimitValue::Num(n.clone()), at + 1),
        Tok::Str(s) => (LimitValue::Num(s.clone()), at + 1),
        Tok::Param(p) => (LimitValue::Param(p.clone()), at + 1),
        _ => return None,
    };
    // optional ::type cast tail
    let mut tail = 0usize;
    if matches!(toks.get(next), Some(Tok::Op(o)) if o == "::") {
        tail = 1 + type_token_len(&toks[next + 1..]);
    }
    Some((value, tail))
}

#[derive(Debug, Clone, PartialEq)]
enum LimitValue {
    Num(String),
    Param(String),
}

impl LimitValue {
    fn render(&self) -> String {
        match self {
            Self::Num(n) => n.clone(),
            Self::Param(p) => format!("@P{p}"),
        }
    }
}

#[derive(Debug, Default)]
struct Clauses {
    limit: Option<LimitValue>,
    offset: Option<LimitValue>,
    /// tokens consumed by the OFFSET clause (incl. optional ROWS/FETCH tail)
    offset_len: usize,
    /// tokens consumed by the LIMIT clause (2 for `LIMIT n`, 5 for the
    /// deparser's `FETCH FIRST n ROWS ONLY` form)
    limit_len: usize,
    has_order_by: bool,
    has_setop: bool,
}

fn analyze(toks: &[Tok]) -> Result<Clauses, TranslateError> {
    let mut clauses = Clauses::default();
    let mut depth = 0usize;

    let mut i = 0usize;
    while i < toks.len() {
        match &toks[i] {
            Tok::Op(o) if o == "(" => depth += 1,
            Tok::Op(o) if o == ")" => depth = depth.saturating_sub(1),
            Tok::Word(w) if w.eq_ignore_ascii_case("limit") => {
                // LIMIT at any depth: depth 0 records the value, subqueries are
                // rejected outright (T-SQL has no LIMIT anywhere)
                if depth > 0 {
                    return Err(TranslateError::UnsupportedConstruct {
                        sql_fragment: "( ... LIMIT ...)".to_string(),
                        reason: "LIMIT inside a subquery is not supported in v1".to_string(),
                    });
                }
                match limit_value_at(toks, i + 1) {
                    Some((value, cast_tail)) => {
                        clauses.limit = Some(value);
                        clauses.limit_len = 2 + cast_tail;
                        i += 1 + cast_tail;
                    }
                    None if matches!(toks.get(i + 1), Some(Tok::Word(a)) if a.eq_ignore_ascii_case("all")) =>
                    {
                        return Err(TranslateError::UnsupportedConstruct {
                            sql_fragment: "LIMIT ALL".to_string(),
                            reason: "LIMIT ALL is not supported".to_string(),
                        });
                    }
                    _ => {
                        return Err(TranslateError::UnsupportedConstruct {
                            sql_fragment: "LIMIT".to_string(),
                            reason: "LIMIT without a plain constant or parameter".to_string(),
                        });
                    }
                }
            }
            Tok::Word(w) if depth == 0 => {
                let lw = w.to_lowercase();
                match lw.as_str() {
                    "offset" => {
                        // OFFSET n [ROW|ROWS] [FETCH {FIRST|NEXT} n [ROW|ROWS] ONLY]
                        let Some((value, cast_tail)) = limit_value_at(toks, i + 1) else {
                            return Err(TranslateError::UnsupportedConstruct {
                                sql_fragment: "OFFSET".to_string(),
                                reason: "OFFSET without a plain constant or parameter".to_string(),
                            });
                        };
                        clauses.offset = Some(value);
                        let mut len = 2 + cast_tail; // OFFSET + value (+ ::cast)
                        if matches!(toks.get(i + len), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("row") || w.eq_ignore_ascii_case("rows"))
                        {
                            len += 1;
                        }
                        // FETCH FIRST|NEXT n ROW|ROWS ONLY
                        if matches!(toks.get(i + len), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("fetch"))
                            && matches!(toks.get(i + len + 1), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("first") || w.eq_ignore_ascii_case("next"))
                            && let Some((limit, fetch_cast_tail)) =
                                limit_value_at(toks, i + len + 2)
                        {
                            clauses.limit = Some(limit);
                            len += 3 + fetch_cast_tail; // FETCH FIRST/NEXT n (+ ::cast)
                            if matches!(toks.get(i + len), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("row") || w.eq_ignore_ascii_case("rows"))
                            {
                                len += 1;
                            }
                            if matches!(toks.get(i + len), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("only"))
                            {
                                len += 1;
                            }
                        }
                        clauses.offset_len = len;
                        i += len - 1; // outer loop adds 1 more
                    }
                    "order" => {
                        if let Some(Tok::Word(b)) = toks.get(i + 1) {
                            if b.eq_ignore_ascii_case("by") {
                                clauses.has_order_by = true;
                                i += 1;
                            }
                        }
                    }
                    // the deparser prints `LIMIT n` as FETCH FIRST n ROWS ONLY
                    // when there is no OFFSET
                    "fetch" if depth == 0 => {
                        if matches!(toks.get(i + 1), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("first") || w.eq_ignore_ascii_case("next"))
                            && let Some((limit, cast_tail)) = limit_value_at(toks, i + 2)
                        {
                            clauses.limit = Some(limit);
                            let mut len = 3 + cast_tail;
                            if matches!(toks.get(i + len), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("row") || w.eq_ignore_ascii_case("rows"))
                            {
                                len += 1;
                            }
                            if matches!(toks.get(i + len), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("only"))
                            {
                                len += 1;
                            }
                            clauses.limit_len = len;
                            i += len - 1;
                        }
                    }
                    "union" | "intersect" | "except" => clauses.has_setop = true,
                    "distinct" => {
                        if let Some(Tok::Word(b)) = toks.get(i + 1) {
                            if b.eq_ignore_ascii_case("on") {
                                return Err(TranslateError::UnsupportedConstruct {
                                    sql_fragment: "DISTINCT ON".to_string(),
                                    reason: "DISTINCT ON has no T-SQL equivalent".to_string(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }

    Ok(clauses)
}

// ---------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------

/// words that end a predicate expression (used by the boolean-column rule)
const PREDICATE_CONTINUATION: [&str; 16] = [
    "and",
    "or",
    ")",
    ",",
    "then",
    "end",
    "else",
    "group",
    "order",
    "limit",
    "offset",
    "having",
    "union",
    "intersect",
    "except",
    ";",
];
/// words after which a predicate expression starts
const PREDICATE_START: [&str; 9] = [
    "where", "and", "or", "on", "(", "not", "then", "when", "case",
];
/// reserved words that may not appear as the subject of a cast / ILIKE
/// capture, nor as the "function name" in front of a parenthesized subject
const NON_SUBJECT_WORDS: [&str; 34] = [
    "and",
    "or",
    "not",
    "as",
    "on",
    "when",
    "then",
    "else",
    "case",
    "end",
    "select",
    "from",
    "where",
    "group",
    "order",
    "by",
    "having",
    "limit",
    "offset",
    "union",
    "intersect",
    "except",
    "all",
    "distinct",
    "asc",
    "desc",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "in",
];

/// function calls with verified T-SQL equivalents (same name and semantics);
/// anything else followed by `(` is rejected instead of being sent to MSSQL
const KNOWN_FUNCTIONS: [&str; 30] = [
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "abs",
    "round",
    "floor",
    "ceiling",
    "sqrt",
    "power",
    "square",
    "sign",
    "lower",
    "upper",
    "left",
    "right",
    "replace",
    "concat",
    "coalesce",
    "nullif",
    "iif", // window functions (identical names and OVER syntax in T-SQL)
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
];

/// keywords that may legitimately precede `(` and are not function calls
const KEYWORD_CALL_WORDS: [&str; 9] = [
    "in", "exists", "values", "any", "all", "cast", "isnull", "using", "over",
];

pub fn translate(sql: &str, ctx: &TranslateContext) -> Result<String, TranslateError> {
    let toks = lex(sql)?;
    let clauses = analyze(&toks)?;

    // relation lookups (local names are matched case-insensitively: PG folds
    // unquoted identifiers, and we only ever create lowercase foreign tables)
    let maps = relation_maps(&ctx.relations);

    // decide LIMIT strategy
    let use_top = clauses.limit.is_some()
        && clauses.offset.is_none()
        && !clauses.has_order_by
        && !clauses.has_setop;
    let use_fetch = clauses.offset.is_some() || (clauses.limit.is_some() && clauses.has_order_by);
    if clauses.limit.is_some() && clauses.has_setop && !use_top && !clauses.has_order_by {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: "LIMIT with set operations".to_string(),
            reason: "TOP over UNION/INTERSECT/EXCEPT is not supported; add ORDER BY".to_string(),
        });
    }

    let mut out: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut first_select_seen = false;
    let mut top_emitted = false;
    // CASE ... END tracking: casts/ILIKE may not cross an open CASE
    let mut case_depth = 0usize;
    // current top-level clause: the deparser prints top-level AND-chains in
    // WHERE/HAVING/ON as comma-separated lists, which must become AND
    let mut in_condition_clause = false;

    // top-level ORDER BY tracking: PostgreSQL's implicit NULL ordering
    // (ASC → NULLS LAST, DESC → NULLS FIRST) differs from T-SQL's (NULL is
    // always smallest), so nullable sort keys need a CASE tiebreaker
    let mut in_order = false;
    let mut order_item_closed = true;
    // Some(depth) while inside the parentheses of `OVER ( ... )`
    let mut over_paren_depth: Option<usize> = None;
    let mut next_opens_over = false;
    let mut over_order_seen = false;

    let mut i = 0usize;
    while i < toks.len() {
        match &toks[i] {
            Tok::Op(o) => {
                match o.as_str() {
                    // '[' and ']' lex, but only the ANY/ALL (ARRAY[…]) form
                    // consumes them; anything else is an array subscript
                    "[" | "]" => {
                        return Err(TranslateError::UnsupportedConstruct {
                            sql_fragment: o.clone(),
                            reason: "array subscripts are not supported in v1".to_string(),
                        });
                    }
                    "(" => {
                        depth += 1;
                        if next_opens_over {
                            over_paren_depth = Some(depth);
                            next_opens_over = false;
                        }
                    }
                    ")" => {
                        depth = depth.saturating_sub(1);
                        if over_paren_depth == Some(depth + 1) {
                            over_paren_depth = None;
                            if over_order_seen {
                                over_order_seen = false;
                                // last sort key inside OVER(): must be a
                                // plain NOT NULL column (or NULL ordering
                                // would silently differ in T-SQL). PG17's
                                // deparser appends an explicit window frame
                                // (`ROWS … PRECEDING/…`), which trails the
                                // sort key — lift it off first and put it
                                // back afterwards.
                                const FRAME_WORDS: [&str; 9] = [
                                    "ROWS",
                                    "RANGE",
                                    "GROUPS",
                                    "PRECEDING",
                                    "FOLLOWING",
                                    "UNBOUNDED",
                                    "CURRENT",
                                    "ROW",
                                    "BETWEEN",
                                ];
                                let mut frame: Vec<String> = Vec::new();
                                while out.last().is_some_and(|p| {
                                    FRAME_WORDS.contains(&p.to_uppercase().as_str())
                                }) {
                                    frame.insert(0, out.pop().unwrap());
                                }
                                let last_desc = pop_if_word(&mut out, "desc");
                                if !last_desc {
                                    pop_if_word(&mut out, "asc");
                                }
                                if let Ok(start) = capture_subject(&out, case_depth) {
                                    let expr = out[start..].join(" ");
                                    let bare_not_null = out.len() - start == 1
                                        && !expr.contains(' ')
                                        && ctx.not_null_columns.contains(&expr.to_lowercase());
                                    if !bare_not_null {
                                        return Err(TranslateError::UnsupportedConstruct {
                                            sql_fragment: format!("ORDER BY {expr} inside OVER(…)"),
                                            reason: "nullable NULL ordering inside a window \
                                                     cannot be translated to T-SQL faithfully"
                                                .to_string(),
                                        });
                                    }
                                    if last_desc {
                                        out.push("DESC".to_string());
                                    }
                                }
                                // PG17's deparser materializes the default
                                // frame (`ROWS/RANGE UNBOUNDED PRECEDING`),
                                // which T-SQL forbids on ranking functions
                                // and which equals T-SQL's implicit default
                                // anyway — drop it; keep explicit frames.
                                let is_default_frame = frame.len() == 3
                                    && matches!(frame[0].as_str(), "ROWS" | "RANGE")
                                    && frame[1].eq_ignore_ascii_case("UNBOUNDED")
                                    && frame[2].eq_ignore_ascii_case("PRECEDING");
                                if !is_default_frame {
                                    out.extend(frame);
                                }
                            }
                        }
                    }
                    ";" => {
                        i += 1;
                        continue;
                    }
                    "," if in_condition_clause && depth == 0 => {
                        // deparser's `,` at the top of a WHERE/HAVING/ON list
                        out.push("AND".to_string());
                        i += 1;
                        continue;
                    }
                    "," if in_order && depth == 0 => {
                        // close the previous ORDER BY item, start the next
                        if !order_item_closed {
                            close_order_item(&mut out, ctx, case_depth, None)?;
                            order_item_closed = true;
                        }
                        out.push(",".to_string());
                        i += 1;
                        continue;
                    }
                    "||" => {
                        out.push("+".to_string());
                        i += 1;
                        continue;
                    }
                    "::" => {
                        let mssql_type = parse_cast_type(&toks[i + 1..])?;
                        let end = type_token_len(&toks[i + 1..]);
                        let start = capture_subject(&out, case_depth)?;
                        let expr = out[start..].join(" ");
                        out.truncate(start);
                        out.push(format!("CAST({expr} AS {mssql_type})"));
                        i += 1 + end;
                        continue;
                    }
                    _ => {}
                }
                out.push(o.clone());
            }
            Tok::Num(n) => out.push(n.clone()),
            Tok::Str(s) => out.push(tsql_string_literal(s)),
            Tok::Param(p) => out.push(format!("@P{p}")),
            Tok::QIdent(name) => {
                // quoted names take part in relation matching too
                if let Some((remote, consumed)) = match_relation(&toks, i, &maps)? {
                    out.push(remote);
                    i += consumed;
                    continue;
                }
                out.push(bracket_ident(name)?);
            }
            Tok::Word(w) => {
                // --- relation renaming ------------------------------------
                if let Some((remote, consumed)) = match_relation(&toks, i, &maps)? {
                    out.push(remote);
                    i += consumed;
                    continue;
                }

                let lw = w.to_lowercase();
                match lw.as_str() {
                    "limit" if depth == 0 => {
                        // value token already validated by analyze(); TOP was
                        // injected at the SELECT, otherwise the OFFSET/FETCH
                        // clause belongs at this position (right after ORDER BY)
                        if in_order && !order_item_closed {
                            close_order_item(&mut out, ctx, case_depth, None)?;
                            order_item_closed = true;
                        }
                        in_order = false;
                        if use_fetch && clauses.offset.is_none() {
                            if !clauses.has_order_by {
                                out.push("ORDER BY (SELECT NULL)".to_string());
                            }
                            out.push("OFFSET 0 ROWS".to_string());
                            out.push(format!(
                                "FETCH NEXT {} ROWS ONLY",
                                clauses.limit.as_ref().unwrap().render()
                            ));
                        }
                        i += clauses.limit_len.max(2);
                        continue;
                    }
                    // deparser's `FETCH FIRST n ROWS ONLY` LIMIT form (no
                    // OFFSET): the canonical clause was emitted already
                    "fetch" if depth == 0 && clauses.limit_len > 0 && clauses.offset.is_none() => {
                        if in_order && !order_item_closed {
                            close_order_item(&mut out, ctx, case_depth, None)?;
                            order_item_closed = true;
                        }
                        in_order = false;
                        if use_fetch && !top_emitted {
                            if !clauses.has_order_by {
                                out.push("ORDER BY (SELECT NULL)".to_string());
                            }
                            out.push("OFFSET 0 ROWS".to_string());
                            out.push(format!(
                                "FETCH NEXT {} ROWS ONLY",
                                clauses.limit.as_ref().unwrap().render()
                            ));
                        }
                        i += clauses.limit_len;
                        continue;
                    }
                    "offset" if depth == 0 => {
                        // T-SQL OFFSET/FETCH must follow an ORDER BY; the
                        // whole PG tail (ROWS / FETCH … ONLY) is re-emitted
                        // in canonical form, so consume everything analyze
                        // measured for this clause
                        if in_order && !order_item_closed {
                            close_order_item(&mut out, ctx, case_depth, None)?;
                            order_item_closed = true;
                        }
                        in_order = false;
                        if !clauses.has_order_by {
                            out.push("ORDER BY (SELECT NULL)".to_string());
                        }
                        out.push(format!(
                            "OFFSET {} ROWS",
                            clauses.offset.as_ref().unwrap().render()
                        ));
                        if let Some(limit) = &clauses.limit {
                            out.push(format!("FETCH NEXT {} ROWS ONLY", limit.render()));
                        }
                        i += clauses.offset_len;
                        continue;
                    }
                    "select" if depth == 0 && !first_select_seen => {
                        first_select_seen = true;
                        out.push("SELECT".to_string());
                        i += 1;
                        // pull DISTINCT into place, then TOP right after it
                        if let Some(Tok::Word(d)) = toks.get(i) {
                            if d.eq_ignore_ascii_case("distinct") {
                                out.push("DISTINCT".to_string());
                                i += 1;
                            }
                        }
                        if use_top && !top_emitted {
                            top_emitted = true;
                            out.push(format!(
                                "TOP ({})",
                                clauses.limit.as_ref().unwrap().render()
                            ));
                        }
                        continue;
                    }
                    "case" => case_depth += 1,
                    "end" => case_depth = case_depth.saturating_sub(1),
                    // condition clauses: their top-level `,` lists mean AND
                    "where" | "having" | "on" if depth == 0 => in_condition_clause = true,
                    // top-level ORDER BY opens sort-key tracking; inside
                    // OVER() it only arms the closing-parenthesis check
                    "order" if matches!(toks.get(i + 1), Some(Tok::Word(b)) if b.eq_ignore_ascii_case("by")) =>
                    {
                        if depth == 0 {
                            in_condition_clause = false;
                            in_order = true;
                            order_item_closed = false;
                        } else if over_paren_depth.is_some()
                            && depth >= over_paren_depth.unwrap_or(usize::MAX)
                        {
                            over_order_seen = true;
                        }
                        out.push("ORDER BY".to_string());
                        i += 2;
                        continue;
                    }
                    // structural keywords end the condition clause and the
                    // ORDER BY list
                    "select" | "from" | "group" | "limit" | "offset" | "union" | "intersect"
                    | "except" | "join" | "inner" | "left" | "right" | "full" | "cross"
                    | "outer" | "returning"
                        if depth == 0 =>
                    {
                        in_condition_clause = false;
                        if in_order && !order_item_closed {
                            close_order_item(&mut out, ctx, case_depth, None)?;
                            order_item_closed = true;
                        }
                        in_order = false;
                    }
                    // `OVER ( ... )` needs separate NULL-ordering handling
                    "over" if matches!(toks.get(i + 1), Some(Tok::Op(o)) if o == "(") => {
                        next_opens_over = true;
                        out.push("OVER".to_string());
                        i += 1;
                        continue;
                    }
                    // direction terminates an ORDER BY item; inside OVER()
                    // a nullable sort key cannot be corrected safely
                    "asc" | "desc" if in_order && depth == 0 && over_paren_depth.is_none() => {
                        let desc = lw == "desc";
                        if matches!(toks.get(i + 1), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("nulls"))
                        {
                            // the NULLS branch below owns the item rewrite
                            out.push(if desc { "DESC" } else { "ASC" }.to_string());
                        } else {
                            close_order_item(&mut out, ctx, case_depth, Some(desc))?;
                            order_item_closed = true;
                        }
                        i += 1;
                        continue;
                    }
                    "asc" | "desc"
                        if over_paren_depth.is_some()
                            && depth >= over_paren_depth.unwrap_or(usize::MAX) =>
                    {
                        if let Ok(start) = capture_subject(&out, case_depth) {
                            let expr = out[start..].join(" ");
                            // a composite key (`price + id`) would have its
                            // nullability judged by the last operand only;
                            // refuse it instead of trusting the wrong check
                            if !is_whole_order_item(&out, start) {
                                return Err(TranslateError::UnsupportedConstruct {
                                    sql_fragment: "ORDER BY … inside OVER(…)".to_string(),
                                    reason: format!(
                                        "composite ORDER BY keys inside a window cannot \
                                         be NULL-corrected in T-SQL (expr={expr:?})"
                                    ),
                                });
                            }
                            let bare_not_null = out.len() - start == 1
                                && !expr.contains(' ')
                                && ctx.not_null_columns.contains(&expr.to_lowercase());
                            if !bare_not_null {
                                return Err(TranslateError::UnsupportedConstruct {
                                    sql_fragment: "ORDER BY … NULLS … inside OVER(…)".to_string(),
                                    reason: format!(
                                        "nullable NULL ordering inside a window cannot be \
                                         translated to T-SQL faithfully (expr={expr:?}, \
                                         not_null_columns={:?})",
                                        ctx.not_null_columns
                                    ),
                                });
                            }
                        }
                        out.push(lw.to_uppercase());
                        i += 1;
                        continue;
                    }
                    // PG emits `FROM ONLY tbl` to skip child tables; T-SQL
                    // has no inheritance, so ONLY is simply dropped
                    "only" => {
                        i += 1;
                        continue;
                    }
                    // ORDER BY … NULLS FIRST/LAST: T-SQL has no NULLS
                    // syntax. Its implicit rule is "NULL is smallest" (ASC →
                    // NULLS FIRST, DESC → NULLS LAST); matching forms just
                    // drop the word, the opposite ones add a CASE tiebreaker.
                    "nulls" if matches!(toks.get(i + 1), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("first") || w.eq_ignore_ascii_case("last")) =>
                    {
                        let pg_last = toks.get(i + 1).is_some_and(
                            |w| matches!(w, Tok::Word(x) if x.eq_ignore_ascii_case("last")),
                        );
                        let dir_desc = pop_if_word(&mut out, "desc");
                        if !dir_desc {
                            pop_if_word(&mut out, "asc");
                        }
                        let tsql_default_last = dir_desc; // NULL smallest
                        if pg_last != tsql_default_last {
                            // opposite of T-SQL default → prepend CASE tiebreaker
                            let start = capture_subject(&out, case_depth)?;
                            let expr = out[start..].join(" ");
                            if !is_whole_order_item(&out, start) {
                                return Err(TranslateError::UnsupportedConstruct {
                                    sql_fragment: format!(
                                        "ORDER BY {} NULLS …",
                                        order_item_fragment(&out, start)
                                    ),
                                    reason: "composite ORDER BY keys cannot carry \
                                             PostgreSQL NULL ordering in T-SQL; \
                                             sort by a column or parenthesize the \
                                             expression"
                                        .to_string(),
                                });
                            }
                            out.truncate(start);
                            out.push(format!(
                                "CASE WHEN {expr} IS NULL THEN 1 ELSE 0 END{}",
                                if dir_desc { " DESC" } else { "" }
                            ));
                            out.push(",".to_string());
                            out.push(expr);
                            if dir_desc {
                                out.push("DESC".to_string());
                            }
                        } else {
                            // matches the T-SQL default: keep the plain term
                            if dir_desc {
                                out.push("DESC".to_string());
                            }
                        }
                        order_item_closed = true;
                        i += 2;
                        continue;
                    }
                    // SQL typed literals: DATE '...' / TIMESTAMP '...' /
                    // NUMERIC '...' become CASTs; unmappable types error out
                    _ if matches!(toks.get(i + 1), Some(Tok::Str(_)))
                        && types::is_pg_type_name(&lw) =>
                    {
                        match types::pg_type_to_mssql(&lw) {
                            Some(mssql_type) => {
                                let Tok::Str(s) = &toks[i + 1] else {
                                    unreachable!();
                                };
                                out.push(format!(
                                    "CAST({} AS {mssql_type})",
                                    tsql_string_literal(s)
                                ));
                                i += 2;
                                continue;
                            }
                            None => {
                                return Err(TranslateError::UnsupportedConstruct {
                                    sql_fragment: format!("{lw} '…'"),
                                    reason: "typed literal has no T-SQL mapping".to_string(),
                                });
                            }
                        }
                    }
                    "any" | "all" if matches!(toks.get(i + 1), Some(Tok::Op(o)) if o == "(") => {
                        let is_any = lw == "any";
                        translate_any_all(&toks, i, &mut out, &mut i, case_depth, is_any)?;
                        continue;
                    }
                    "ilike" => {
                        translate_ilike(&toks, i, &mut out, &mut i, case_depth)?;
                        continue;
                    }
                    "is" => {
                        translate_is(&toks, i, &mut out, &mut i, case_depth)?;
                        continue;
                    }
                    "true" => {
                        out.push("1".to_string());
                        i += 1;
                        continue;
                    }
                    "false" => {
                        out.push("0".to_string());
                        i += 1;
                        continue;
                    }
                    _ => {}
                }

                // --- bare boolean column in predicate position -------------
                if ctx.bool_columns.contains(&lw)
                    && last_piece_is(&out, &PREDICATE_START)
                    && next_is_continuation(&toks, i + 1)
                {
                    let negated = pop_if_word(&mut out, "not");
                    out.push(format!("{w} = {}", if negated { 0 } else { 1 }));
                    i += 1;
                    continue;
                }

                // --- unknown function calls are rejected -------------------
                if matches!(toks.get(i + 1), Some(Tok::Op(o)) if o == "(")
                    && !KNOWN_FUNCTIONS.contains(&lw.as_str())
                    && !KEYWORD_CALL_WORDS.contains(&lw.as_str())
                    && !NON_SUBJECT_WORDS.contains(&lw.as_str())
                {
                    return Err(TranslateError::UnsupportedConstruct {
                        sql_fragment: format!("{w}(…)"),
                        reason: "function has no verified T-SQL equivalent".to_string(),
                    });
                }

                out.push(w.clone());
            }
        }
        i += 1;
    }

    // an ORDER BY list can end with the statement itself
    if in_order && !order_item_closed {
        close_order_item(&mut out, ctx, case_depth, None)?;
    }

    Ok(join_pieces(&out))
}

/// True when the subject captured at `start` spans a complete top-level or
/// window ORDER BY item: the piece before it must open the item (`ORDER BY`)
/// or separate items (`,`). Otherwise the item is a composite expression and
/// only its last operand was captured — rewriting it into a NULL-tiebreaker
/// pair would silently reorder rows, so callers reject it (fail-closed).
fn is_whole_order_item(out: &[String], start: usize) -> bool {
    start == 0
        || matches!(
            out.get(start - 1).map(String::as_str),
            Some("ORDER BY" | ",")
        )
}

/// Render the full ORDER BY item that `start` was captured from, walking back
/// to the item's opening piece — used for error diagnostics so the reported
/// fragment shows `amount + fee`, not just the captured `fee`.
fn order_item_fragment(out: &[String], start: usize) -> String {
    let mut item_start = start;
    while item_start > 0 && !matches!(out[item_start - 1].as_str(), "ORDER BY" | ",") {
        item_start -= 1;
    }
    out[item_start..].join(" ")
}

/// Finish one top-level ORDER BY item: re-emit it with a NULL tiebreaker
/// that reproduces PostgreSQL's implicit NULL ordering (ASC → NULLS LAST,
/// DESC → NULLS FIRST) unless the item is a plain NOT NULL column, which
/// needs no correction in T-SQL.
fn close_order_item(
    out: &mut Vec<String>,
    ctx: &TranslateContext,
    case_depth: usize,
    desc: Option<bool>,
) -> Result<(), TranslateError> {
    let start = capture_subject(out, case_depth)?;
    let expr = out[start..].join(" ");
    if !is_whole_order_item(out, start) {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: format!("ORDER BY {}", order_item_fragment(out, start)),
            reason: "composite ORDER BY keys cannot carry PostgreSQL NULL \
                     ordering in T-SQL; sort by a column or parenthesize the \
                     expression"
                .to_string(),
        });
    }
    let bare_not_null = out.len() - start == 1
        && !expr.contains(' ')
        && ctx.not_null_columns.contains(&expr.to_lowercase());

    if bare_not_null {
        if desc == Some(true) {
            out.push("DESC".to_string());
        }
        return Ok(());
    }

    out.truncate(start);
    let d = desc.unwrap_or(false);
    out.push(format!(
        "CASE WHEN {expr} IS NULL THEN 1 ELSE 0 END{}",
        if d { " DESC" } else { "" }
    ));
    out.push(",".to_string());
    out.push(expr);
    if d {
        out.push("DESC".to_string());
    }
    Ok(())
}

/// Join rendered pieces into a statement: no space around `.`, no space before
/// `,` and `)`, no space after `(`. Whitespace-insensitive either way, but
/// this keeps the emitted T-SQL close to what a human would write.
fn join_pieces(out: &[String]) -> String {
    // keywords that keep a space before a following `(`
    const SPACED_BEFORE_PAREN: [&str; 14] = [
        "AND", "OR", "NOT", "WHERE", "ON", "THEN", "ELSE", "WHEN", "LIKE", "IN", "IS", "NULL",
        "BETWEEN", "HAVING",
    ];
    let mut ret = String::new();
    let mut prev: Option<&str> = None;
    for piece in out {
        if ret.is_empty() {
            ret.push_str(piece);
            prev = Some(piece);
            continue;
        }
        let no_space_before = matches!(piece.as_str(), ")" | "," | ".")
            || (piece == "("
                && prev.is_some_and(|p| {
                    p.chars()
                        .last()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ']')
                        && !SPACED_BEFORE_PAREN.contains(&p)
                }));
        let no_space_after = matches!(prev, Some("(") | Some("."));
        if !no_space_before && !no_space_after {
            ret.push(' ');
        }
        ret.push_str(piece);
        prev = Some(piece);
    }
    ret
}

// ---------------------------------------------------------------------------
// Subject capture (shared by casts, ILIKE, IS <bool>)
// ---------------------------------------------------------------------------

fn is_plain_ident(piece: &str) -> bool {
    !piece.is_empty()
        && piece
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ']' || c == '.')
        && !NON_SUBJECT_WORDS.contains(&piece.to_lowercase().as_str())
}

/// Capture the start index of the "subject" expression that ends at the end of
/// `out`: a single primary token, a dotted name, or a balanced parenthesized
/// group optionally preceded by a function name. Returns `Err` (→ caller
/// reports unsupported) for anything more complex.
fn capture_subject(out: &[String], case_depth: usize) -> Result<usize, TranslateError> {
    if case_depth > 0 {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: "CASE ... END".to_string(),
            reason: "casts/ILIKE over CASE expressions are not supported in v1".to_string(),
        });
    }
    if out.is_empty() {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: ":: type".to_string(),
            reason: "cast without a subject expression".to_string(),
        });
    }

    let last = out.last().unwrap();
    if last == ")" {
        // walk back to the matching "("
        let mut balance = 0i32;
        let mut j = out.len();
        while j > 0 {
            j -= 1;
            if out[j] == ")" {
                balance += 1;
            } else if out[j] == "(" {
                balance -= 1;
                if balance == 0 {
                    // allow one function name in front of the group
                    if j >= 1 && is_plain_ident(&out[j - 1]) && out[j - 1] != ")" {
                        j -= 1;
                    }
                    return Ok(j);
                }
            }
        }
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: ")".to_string(),
            reason: "unbalanced parentheses before cast".to_string(),
        });
    }

    if !is_plain_ident(last) && !last.starts_with('\'') && !last.starts_with('@') {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: last.clone(),
            reason: "expression subject is not simple enough for v1".to_string(),
        });
    }

    let mut j = out.len() - 1;
    // dotted names: a . b
    while j >= 2 && out[j - 1] == "." && is_plain_ident(&out[j - 2]) {
        j -= 2;
    }
    Ok(j)
}

// ---------------------------------------------------------------------------
// Cast type parsing
// ---------------------------------------------------------------------------

/// Multi-word built-in type names a deparsed `::` cast can spell out. Matched
/// greedily after the optional schema qualifier; longest names first.
const MULTIWORD_PG_TYPES: [&str; 6] = [
    "timestamp with time zone",
    "timestamp without time zone",
    "time with time zone",
    "time without time zone",
    "double precision",
    "character varying",
];

/// Number of tokens (starting at `toks`) occupied by a type name, including an
/// optional `pg_catalog.` qualifier, multi-word built-in names (`timestamp
/// with time zone`), and an optional `(n[,m])` modifier.
fn type_token_len(toks: &[Tok]) -> usize {
    // optional qualifier `word . word`: the triple already includes the
    // first word of the type name (pg_catalog.int8)
    let qualified = matches!(
        (toks.first(), toks.get(1), toks.get(2)),
        (Some(Tok::Word(_)), Some(Tok::Op(o)), Some(Tok::Word(_))) if o == "."
    );
    // index of the first type-name word
    let name_start = if qualified { 2 } else { 0 };

    // multi-word names: `timestamp with time zone` truncated to `timestamp`
    // used to emit `CAST(x AS datetime2)` and leave `with time zone` behind
    // as stray tokens — a T-SQL syntax error on plain timestamp queries
    let mut len = if qualified { 3 } else { 0 };
    let mut matched_multi = false;
    for multi in MULTIWORD_PG_TYPES {
        let words: Vec<&str> = multi.split(' ').collect();
        let end = name_start + words.len();
        if toks.len() >= end
            && toks[name_start..end]
                .iter()
                .enumerate()
                .all(|(k, t)| matches!(t, Tok::Word(w) if w.eq_ignore_ascii_case(words[k])))
        {
            len = end;
            matched_multi = true;
            break;
        }
    }
    if !matched_multi {
        if toks
            .get(name_start)
            .is_some_and(|t| matches!(t, Tok::Word(_)))
        {
            len = name_start + 1;
        } else {
            return 0;
        }
    }
    // optional modifier list: ( n [, n]* )
    if let (Some(Tok::Op(o)), Some(Tok::Num(_))) = (toks.get(len), toks.get(len + 1))
        && o == "("
    {
        let mut j = len + 1;
        while let Some(t) = toks.get(j) {
            match t {
                Tok::Num(_) => j += 1,
                Tok::Op(op) if op == "," => j += 1,
                Tok::Op(op) if op == ")" => return j + 1,
                _ => break,
            }
        }
    }
    len
}

fn parse_cast_type(toks: &[Tok]) -> Result<&'static str, TranslateError> {
    let len = type_token_len(toks);
    // reconstruct the name after the optional schema qualifier
    // (pg_catalog.int8 → int8), joining multi-word names with spaces
    // (timestamp with time zone)
    let qualified = matches!(
        (toks.first(), toks.get(1), toks.get(2)),
        (Some(Tok::Word(_)), Some(Tok::Op(o)), Some(Tok::Word(_))) if o == "."
    );
    let start = if qualified { 2 } else { 0 };
    let mut parts: Vec<String> = Vec::new();
    for t in &toks[start..len] {
        match t {
            Tok::Word(w) => parts.push(w.clone()),
            Tok::QIdent(q) => parts.push(q.clone()),
            Tok::Op(o) if o == "." => continue,
            _ => break,
        }
    }
    let name = parts.join(" ");
    if name.is_empty() {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: "::".to_string(),
            reason: "missing type name after cast".to_string(),
        });
    }
    types::pg_type_to_mssql(&name).ok_or_else(|| TranslateError::UnsupportedConstruct {
        sql_fragment: format!("::{name}"),
        reason: "cast target type has no T-SQL mapping".to_string(),
    })
}

// ---------------------------------------------------------------------------
// ILIKE / IS <boolean>
// ---------------------------------------------------------------------------

fn pop_if_word(out: &mut Vec<String>, word: &str) -> bool {
    if out.last().is_some_and(|p| p.eq_ignore_ascii_case(word)) {
        out.pop();
        true
    } else {
        false
    }
}

fn translate_ilike(
    toks: &[Tok],
    i: usize,
    out: &mut Vec<String>,
    next_i: &mut usize,
    case_depth: usize,
) -> Result<(), TranslateError> {
    // `NOT ILIKE`: strip the NOT first so it does not block subject capture
    let negated = pop_if_word(out, "not");
    let start = capture_subject(out, case_depth)?;
    let lhs = out[start..].join(" ");
    out.truncate(start);

    let rhs = match toks.get(i + 1) {
        Some(Tok::Word(w)) => w.clone(),
        Some(Tok::QIdent(q)) => bracket_ident(q)?,
        Some(Tok::Str(s)) => tsql_string_literal(s),
        _ => {
            return Err(TranslateError::UnsupportedConstruct {
                sql_fragment: "ILIKE".to_string(),
                reason: "ILIKE pattern must be a simple literal or column".to_string(),
            });
        }
    };
    if let Some(Tok::Word(e)) = toks.get(i + 2) {
        if e.eq_ignore_ascii_case("escape") {
            return Err(TranslateError::UnsupportedConstruct {
                sql_fragment: "ILIKE ... ESCAPE".to_string(),
                reason: "ESCAPE clauses are not supported in v1".to_string(),
            });
        }
    }

    let body = format!("LOWER({lhs}) LIKE LOWER({rhs})");
    out.push(if negated {
        format!("NOT ({body})")
    } else {
        format!("({body})")
    });
    *next_i = i + 2;
    Ok(())
}

/// Parse an array constant in PostgreSQL output form: `'{v1,v2,…}'`. String
/// elements are quoted with `"…"` (inner quotes doubled); NULL elements never
/// satisfy `= ANY` / fail `<> ALL`, so they are dropped; boolean elements are
/// printed as t/f. Anything not numeric, quoted, NULL or t/f is rejected.
fn parse_array_literal(s: &str) -> Result<Vec<String>, TranslateError> {
    let t = s.trim();
    if !t.starts_with('{') || !t.ends_with('}') {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: format!("'{t}'"),
            reason: "ANY/ALL array constant must be in '{…}' form".to_string(),
        });
    }
    let inner = &t[1..t.len() - 1];
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }

    // split on commas, respecting "…" quotes with doubled inner quotes
    let mut raw_items: Vec<(String, bool)> = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        let mut cur = String::new();
        let mut quoted = false;
        if chars.peek() == Some(&'"') {
            quoted = true;
            chars.next();
            loop {
                match chars.next() {
                    Some('"') => {
                        if chars.peek() == Some(&'"') {
                            cur.push('"');
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    Some(c) => cur.push(c),
                    None => break,
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                cur.push(c);
                chars.next();
            }
        }
        raw_items.push((cur, quoted));
        match chars.next() {
            Some(',') => continue,
            _ => break,
        }
    }

    let mut items = Vec::with_capacity(raw_items.len());
    for (raw, quoted) in raw_items {
        if quoted {
            items.push(tsql_string_literal(&raw));
        } else {
            match raw.trim() {
                // T-SQL IN/NOT IN share PostgreSQL's three-valued semantics,
                // so NULL elements must stay in the list: dropping them
                // would flip `x <> ALL('{1,NULL}')` from "no rows" to
                // "every x <> 1"
                "NULL" => items.push("NULL".to_string()),
                "t" => items.push("1".to_string()),
                "f" => items.push("0".to_string()),
                v if !v.is_empty()
                    && v.chars().all(|c| {
                        c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')
                    }) =>
                {
                    items.push(v.to_string());
                }
                other => {
                    return Err(TranslateError::UnsupportedConstruct {
                        sql_fragment: format!("'{{{other}}}'"),
                        reason: "array constant contains an unsupported element".to_string(),
                    });
                }
            }
        }
    }
    Ok(items)
}

/// `x = ANY (ARRAY[v1, v2, …]::type[])` / `x <> ALL (ARRAY[…])` — the shape
/// PostgreSQL's deparser uses for IN-lists inside full queries (e.g. when an
/// aggregate or LIMIT forces the whole statement through translation).
/// Rewritten to IN / NOT IN; other operators expand into OR-chains (ANY) /
/// AND-chains (ALL), mirroring the plain-scan qual rendering.
fn translate_any_all(
    toks: &[Tok],
    i: usize,
    out: &mut Vec<String>,
    next_i: &mut usize,
    case_depth: usize,
    is_any: bool,
) -> Result<(), TranslateError> {
    // expected tail: ( ARRAY [ items ] [::type []] )
    let malformed = |what: &str| TranslateError::UnsupportedConstruct {
        sql_fragment: what.to_string(),
        reason: "only ANY/ALL over ARRAY[…] literals are supported \
                 (the deparser form of an IN-list)"
            .to_string(),
    };

    // the comparison operator was already emitted; take it off first, then
    // the subject must be a single simple expression
    const ANY_ALL_OPERATORS: [&str; 6] = ["=", "<>", "<", ">", "<=", ">="];
    let Some(oper) = out.last().cloned() else {
        return Err(malformed(if is_any { "ANY (…)" } else { "ALL (…)" }));
    };
    if !ANY_ALL_OPERATORS.contains(&oper.as_str()) {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: format!("{oper} ANY/ALL (…)"),
            reason: "ANY/ALL is only supported over simple comparison operators".to_string(),
        });
    }
    out.pop();
    let start = capture_subject(out, case_depth)?;
    if out.len() - start != 1 {
        return Err(TranslateError::UnsupportedConstruct {
            sql_fragment: if is_any { "ANY (…)" } else { "ALL (…)" }.to_string(),
            reason: "ANY/ALL subject is not a simple column reference".to_string(),
        });
    }
    let field = out[start].clone();
    out.truncate(start);

    // two deparser shapes: `ARRAY[v1, v2]` (ArrayExpr) and `'{v1,v2}'::type[]`
    // (a constant-folded IN-list)
    let mut items: Vec<String> = Vec::new();
    let mut k;
    if matches!(toks.get(i + 2), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("array"))
        && matches!(toks.get(i + 3), Some(Tok::Op(o)) if o == "[")
    {
        let mut j = i + 4;
        loop {
            match toks.get(j) {
                Some(Tok::Op(o)) if o == "]" => {
                    j += 1;
                    break;
                }
                Some(Tok::Op(o)) if o == "," && !items.is_empty() => {
                    j += 1;
                }
                Some(Tok::Op(o)) if o == "-" && matches!(toks.get(j + 1), Some(Tok::Num(_))) => {
                    if let Some(Tok::Num(n)) = toks.get(j + 1) {
                        items.push(format!("-{n}"));
                    }
                    j += 2;
                }
                Some(Tok::Num(n)) => {
                    items.push(n.clone());
                    j += 1;
                }
                Some(Tok::Str(s)) => {
                    items.push(tsql_string_literal(s));
                    j += 1;
                }
                Some(Tok::Param(p)) => {
                    items.push(format!("@P{p}"));
                    j += 1;
                }
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("true") => {
                    items.push("1".to_string());
                    j += 1;
                }
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("false") => {
                    items.push("0".to_string());
                    j += 1;
                }
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("null") => {
                    // keep NULL elements: IN/NOT IN semantics match PG
                    items.push("NULL".to_string());
                    j += 1;
                }
                _ => {
                    return Err(TranslateError::UnsupportedConstruct {
                        sql_fragment: "ARRAY[…]".to_string(),
                        reason: "array literal contains a non-literal element".to_string(),
                    });
                }
            }
        }
        k = j;
    } else if let Some(Tok::Str(s)) = toks.get(i + 2) {
        items = parse_array_literal(s)?;
        k = i + 3;
    } else {
        return Err(malformed(if is_any { "ANY (…)" } else { "ALL (…)" }));
    }
    // optional `::type[]` cast tail, then the closing parenthesis
    if matches!(toks.get(k), Some(Tok::Op(o)) if o == "::") {
        let len = type_token_len(&toks[k + 1..]);
        if len == 0 {
            return Err(malformed("::"));
        }
        k += 1 + len;
        if matches!(toks.get(k), Some(Tok::Op(o)) if o == "[") {
            if !matches!(toks.get(k + 1), Some(Tok::Op(o)) if o == "]") {
                return Err(TranslateError::UnsupportedConstruct {
                    sql_fragment: "[…]".to_string(),
                    reason: "array subscripts are not supported in v1".to_string(),
                });
            }
            k += 2;
        }
    }
    if !matches!(toks.get(k), Some(Tok::Op(o)) if o == ")") {
        return Err(malformed(if is_any { "ANY (…" } else { "ALL (…" }));
    }
    k += 1;

    let rendered = if items.is_empty() {
        // `x = ANY ('{}')` is FALSE and `x <> ALL ('{}')` is TRUE
        if is_any {
            "(1 = 0)".to_string()
        } else {
            "(1 = 1)".to_string()
        }
    } else if is_any && oper == "=" {
        format!("({field} IN ({}))", items.join(", "))
    } else if !is_any && oper == "<>" {
        format!("({field} NOT IN ({}))", items.join(", "))
    } else {
        let joiner = if is_any { " OR " } else { " AND " };
        let conds: Vec<String> = items
            .iter()
            .map(|v| format!("{field} {oper} {v}"))
            .collect();
        format!("({})", conds.join(joiner))
    };
    out.push(rendered);
    *next_i = k;
    Ok(())
}

fn translate_is(
    toks: &[Tok],
    i: usize,
    out: &mut Vec<String>,
    next_i: &mut usize,
    case_depth: usize,
) -> Result<(), TranslateError> {
    let (what, consumed) = match toks.get(i + 1) {
        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("true") => ("true", 2),
        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("false") => ("false", 2),
        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("unknown") => ("unknown", 2),
        // `IS NOT TRUE` and friends: three tokens
        Some(Tok::Word(n)) if n.eq_ignore_ascii_case("not") => match toks.get(i + 2) {
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("true") => ("true", 3),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("false") => ("false", 3),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("unknown") => ("unknown", 3),
            // `IS NOT NULL` passes through unchanged (emit and advance!)
            _ => {
                out.push("IS".to_string());
                *next_i = i + 1;
                return Ok(());
            }
        },
        // `IS NULL` passes through unchanged (emit and advance!)
        _ => {
            out.push("IS".to_string());
            *next_i = i + 1;
            return Ok(());
        }
    };

    let start = capture_subject(out, case_depth)?;
    let lhs = out[start..].join(" ");
    out.truncate(start);
    let negated = consumed == 3 || pop_if_word(out, "not");

    let rendered = match (what, negated) {
        ("true", false) => format!("{lhs} = 1"),
        ("true", true) => format!("{lhs} <> 1"),
        ("false", false) => format!("{lhs} = 0"),
        ("false", true) => format!("{lhs} <> 0"),
        ("unknown", false) => format!("{lhs} IS NULL"),
        ("unknown", true) => format!("{lhs} IS NOT NULL"),
        _ => unreachable!(),
    };
    out.push(rendered);
    *next_i = i + consumed;
    Ok(())
}

// ---------------------------------------------------------------------------
// Relation matching
// ---------------------------------------------------------------------------

/// Try to rename `schema.table` / `"schema"."table"` / bare `table` at `i`
/// into `[remote_schema].[remote_table]`. Returns the rendered name and how
/// many tokens were consumed.
/// Lookup maps for relation renaming: qualified `schema.table` always
/// resolves; bare table names resolve only when exactly one relation carries
/// that name across all schemas (the deparser qualifies everything, but
/// client-supplied join text can be bare). A name claimed by two relations
/// is ambiguous and rejected at match time instead of silently resolving to
/// the wrong table.
struct RelationMaps<'a> {
    by_two: HashMap<(String, String), &'a RelationMapping>,
    by_table: HashMap<String, &'a RelationMapping>,
    ambiguous_bare: HashSet<String>,
}

fn relation_maps(relations: &[RelationMapping]) -> RelationMaps<'_> {
    let mut maps = RelationMaps {
        by_two: HashMap::new(),
        by_table: HashMap::new(),
        ambiguous_bare: HashSet::new(),
    };
    for rel in relations {
        maps.by_two.insert(
            (
                rel.local_schema.to_lowercase(),
                rel.local_table.to_lowercase(),
            ),
            rel,
        );
        let bare = rel.local_table.to_lowercase();
        if maps.by_table.contains_key(&bare) || maps.ambiguous_bare.contains(&bare) {
            maps.by_table.remove(&bare);
            maps.ambiguous_bare.insert(bare);
        } else {
            maps.by_table.insert(bare, rel);
        }
    }
    maps
}

/// Does `sql` reference any of `relations` as a whole identifier? Used as a
/// safety net before executing client-supplied statement text remotely: a
/// substring hit (`users` inside `appusers`, or inside a string literal)
/// must not green-light the wrong text. Text that cannot even be lexed
/// counts as "does not mention".
pub(super) fn mentions_relation(sql: &str, relations: &[RelationMapping]) -> bool {
    let toks = match lex(sql) {
        Ok(toks) => toks,
        Err(_) => return false,
    };
    let maps = relation_maps(relations);
    for i in 0..toks.len() {
        if matches!(match_relation(&toks, i, &maps), Ok(Some(_))) {
            return true;
        }
    }
    false
}

/// Try to rename `schema.table` / `"schema"."table"` / bare `table` at `i`
/// into `[remote_schema].[remote_table]`. Returns the rendered name and how
/// many tokens were consumed, or an error for an ambiguous bare name.
fn match_relation(
    toks: &[Tok],
    i: usize,
    maps: &RelationMaps<'_>,
) -> Result<Option<(String, usize)>, TranslateError> {
    let as_word = |t: &Tok| -> Option<String> {
        match t {
            Tok::Word(w) => Some(w.to_lowercase()),
            Tok::QIdent(q) => Some(q.to_lowercase()),
            _ => None,
        }
    };

    // two-part: <part> . <part>
    if let (Some(a), Some(Tok::Op(o)), Some(b)) = (
        toks.get(i).and_then(as_word),
        toks.get(i + 1),
        toks.get(i + 2).and_then(as_word),
    ) && o == "."
        && !matches!(toks.get(i + 3), Some(Tok::Op(op)) if op == ".")
    {
        if let Some(rel) = maps.by_two.get(&(a, b)) {
            return Ok(Some((
                format!(
                    "{}.{}",
                    bracket_ident(&rel.remote_schema)?,
                    bracket_ident(&rel.remote_table)?
                ),
                3,
            )));
        }
        return Ok(None);
    }

    // one-part: bare table name, unambiguous across schemas
    if let Some(name) = toks.get(i).and_then(as_word)
        && !matches!(toks.get(i + 1), Some(Tok::Op(op)) if op == ".")
        && (i == 0 || !matches!(toks.get(i - 1), Some(Tok::Op(op)) if op == "."))
    {
        if maps.ambiguous_bare.contains(&name) {
            return Err(TranslateError::UnsupportedConstruct {
                sql_fragment: name,
                reason: "bare table name is ambiguous between foreign tables of \
                         different schemas; qualify it with the schema name"
                    .to_string(),
            });
        }
        if let Some(rel) = maps.by_table.get(&name) {
            return Ok(Some((
                format!(
                    "{}.{}",
                    bracket_ident(&rel.remote_schema)?,
                    bracket_ident(&rel.remote_table)?
                ),
                1,
            )));
        }
    }

    Ok(None)
}

fn bracket_ident(name: &str) -> Result<String, TranslateError> {
    if name.contains(']') || name.is_empty() {
        return Err(TranslateError::InvalidIdentifier(name.to_string()));
    }
    Ok(format!("[{name}]"))
}

/// Render a string literal for T-SQL. Values containing non-ASCII characters
/// get the `N'…'` form so a server collation with a legacy code page cannot
/// silently mangle them into `?`; plain literals keep varchar comparison
/// semantics (and index use) for the ASCII case.
fn tsql_string_literal(s: &str) -> String {
    let escaped = s.replace('\'', "''");
    if s.is_ascii() {
        format!("'{escaped}'")
    } else {
        format!("N'{escaped}'")
    }
}

/// Check whether the last meaningful piece is one of `words`. A dangling
/// dotted-qualifier tail (`… ident .`) is skipped: while `o.active` is being
/// emitted piecewise the tail `o .` is still in `out`, so the piece before it
/// decides whether the column about to land is in predicate position.
fn last_piece_is(out: &[String], words: &[&str]) -> bool {
    let mut end = out.len();
    while end >= 2 && out[end - 1] == "." && is_plain_ident(&out[end - 2]) {
        end -= 2;
    }
    out.get(end.wrapping_sub(1))
        .is_some_and(|p| words.iter().any(|w| p.eq_ignore_ascii_case(w)))
}

fn next_is_continuation(toks: &[Tok], i: usize) -> bool {
    match toks.get(i) {
        Some(Tok::Word(w)) => PREDICATE_CONTINUATION.contains(&w.to_lowercase().as_str()),
        Some(Tok::Op(o)) => PREDICATE_CONTINUATION.contains(&o.as_str()),
        None => true,
        _ => false,
    }
}
