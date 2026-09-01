//! Pure PostgreSQL-SQL → T-SQL translator for full-query pushdown (TZ §4.3).
//!
//! Input is a deparsed PostgreSQL statement (PG dialect) as delivered by the
//! framework's `FullQuery`. Output is a single valid T-SQL statement. The
//! translator is stateless: `translate(sql, ctx)` takes the relations mapping
//! (and the set of boolean columns) and nothing else, which keeps it fully
//! covered by plain unit tests. Anything outside the supported construct set
//! yields a structured [`TranslateError::UnsupportedConstruct`] — never
//! silently wrong SQL.

use std::collections::HashMap;
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
const FORBIDDEN_CHARS: [char; 6] = ['~', '^', '@', '#', '?', '['];

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
            i += if c == '\'' { 1 } else { 2 };
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(TranslateError::Unterminated("string literal".into())),
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
            // E'' escape sequences are not translated in v1
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

        if FORBIDDEN_CHARS.contains(&c) || c == ']' {
            let reason = match c {
                '~' => "POSIX regex operators are not supported".to_string(),
                '^' => "power operator has no T-SQL equivalent (^ is XOR in T-SQL)".to_string(),
                '[' | ']' => "array subscripts are not supported in v1".to_string(),
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

        if "()=<>+-*/%.,;&|!".contains(c) {
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

// ---------------------------------------------------------------------------
// Depth-0 clause analysis (LIMIT / OFFSET / ORDER BY / set operations)
// ---------------------------------------------------------------------------

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
                match toks.get(i + 1) {
                    Some(Tok::Num(n)) => {
                        clauses.limit = Some(LimitValue::Num(n.clone()));
                        i += 1;
                    }
                    Some(Tok::Param(p)) => {
                        clauses.limit = Some(LimitValue::Param(p.clone()));
                        i += 1;
                    }
                    Some(Tok::Word(a)) if a.eq_ignore_ascii_case("all") => {
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
                    "offset" => match toks.get(i + 1) {
                        Some(Tok::Num(n)) => {
                            clauses.offset = Some(LimitValue::Num(n.clone()));
                            i += 1;
                        }
                        Some(Tok::Param(p)) => {
                            clauses.offset = Some(LimitValue::Param(p.clone()));
                            i += 1;
                        }
                        _ => {
                            return Err(TranslateError::UnsupportedConstruct {
                                sql_fragment: "OFFSET".to_string(),
                                reason: "OFFSET without a plain constant or parameter".to_string(),
                            });
                        }
                    },
                    "order" => {
                        if let Some(Tok::Word(b)) = toks.get(i + 1) {
                            if b.eq_ignore_ascii_case("by") {
                                clauses.has_order_by = true;
                                i += 1;
                            }
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
/// words that may not appear as the subject of a cast / ILIKE capture
const NON_SUBJECT_WORDS: [&str; 7] = ["and", "or", "not", "as", "on", "when", "then"];

pub fn translate(sql: &str, ctx: &TranslateContext) -> Result<String, TranslateError> {
    let toks = lex(sql)?;
    let clauses = analyze(&toks)?;

    // relation lookups (local names are matched case-insensitively: PG folds
    // unquoted identifiers, and we only ever create lowercase foreign tables)
    let mut by_two: HashMap<(String, String), &RelationMapping> = HashMap::new();
    let mut by_table: HashMap<String, &RelationMapping> = HashMap::new();
    for rel in &ctx.relations {
        by_two.insert(
            (
                rel.local_schema.to_lowercase(),
                rel.local_table.to_lowercase(),
            ),
            rel,
        );
        // bare unqualified names only resolve for the default search_path
        // schema (public); this mirrors how the deparser omits qualification
        if rel.local_schema.eq_ignore_ascii_case("public") {
            by_table.insert(rel.local_table.to_lowercase(), rel);
        }
    }

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

    let mut i = 0usize;
    while i < toks.len() {
        match &toks[i] {
            Tok::Op(o) => {
                match o.as_str() {
                    "(" => depth += 1,
                    ")" => depth = depth.saturating_sub(1),
                    ";" => {
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
            Tok::Str(s) => out.push(format!("'{}'", s.replace('\'', "''"))),
            Tok::Param(p) => out.push(format!("@P{p}")),
            Tok::QIdent(name) => {
                // quoted names take part in relation matching too
                if let Some((remote, consumed)) = match_relation(&toks, i, &by_two, &by_table) {
                    out.push(remote);
                    i += consumed;
                    continue;
                }
                out.push(bracket_ident(name)?);
            }
            Tok::Word(w) => {
                // --- relation renaming ------------------------------------
                if let Some((remote, consumed)) = match_relation(&toks, i, &by_two, &by_table) {
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
                        i += 2;
                        continue;
                    }
                    "offset" if depth == 0 => {
                        // T-SQL OFFSET/FETCH must follow an ORDER BY
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
                        i += 2;
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

                out.push(w.clone());
            }
        }
        i += 1;
    }

    Ok(join_pieces(&out))
}

/// Join rendered pieces into a statement: no space around `.`, no space before
/// `,` and `)`, no space after `(`. Whitespace-insensitive either way, but
/// this keeps the emitted T-SQL close to what a human would write.
fn join_pieces(out: &[String]) -> String {
    let mut ret = String::new();
    for piece in out {
        if ret.is_empty() {
            ret.push_str(piece);
            continue;
        }
        let no_space_before = matches!(piece.as_str(), ")" | "," | ".")
            || (piece == "("
                && ret
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ']'));
        let no_space_after = matches!(ret.chars().last(), Some('(' | '.'));
        if !no_space_before && !no_space_after {
            ret.push(' ');
        }
        ret.push_str(piece);
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

/// Number of tokens (starting at `toks`) occupied by a type name, including an
/// optional `pg_catalog.` qualifier and an optional `(n[,m])` modifier.
fn type_token_len(toks: &[Tok]) -> usize {
    let mut len = 0usize;
    // optional qualifier: word . word
    if let (Some(Tok::Word(_)), Some(Tok::Op(o)), Some(Tok::Word(_))) =
        (toks.first(), toks.get(1), toks.get(2))
        && o == "."
    {
        len += 3;
    }
    if len == 0 {
        if toks.first().is_some_and(|t| matches!(t, Tok::Word(_))) {
            len += 1;
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
    // keep only the last name component (pg_catalog.int8 → int8)
    let mut name = String::new();
    for t in &toks[..len] {
        match t {
            Tok::Word(w) => {
                name = w.clone();
            }
            Tok::QIdent(q) => name = q.clone(),
            Tok::Op(o) if o == "." => continue,
            _ => break,
        }
    }
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
        Some(Tok::Str(s)) => format!("'{}'", s.replace('\'', "''")),
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
            _ => return Ok(()), // IS NOT NULL passes through unchanged
        },
        _ => return Ok(()), // IS NULL passes through unchanged
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
fn match_relation(
    toks: &[Tok],
    i: usize,
    by_two: &HashMap<(String, String), &RelationMapping>,
    by_table: &HashMap<String, &RelationMapping>,
) -> Option<(String, usize)> {
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
        if let Some(rel) = by_two.get(&(a, b)) {
            return Some((
                format!(
                    "{}.{}",
                    bracket_ident(&rel.remote_schema).ok()?,
                    bracket_ident(&rel.remote_table).ok()?
                ),
                3,
            ));
        }
        return None;
    }

    // one-part: bare table name from the public schema
    if let Some(name) = toks.get(i).and_then(as_word)
        && !matches!(toks.get(i + 1), Some(Tok::Op(op)) if op == ".")
        && (i == 0 || !matches!(toks.get(i - 1), Some(Tok::Op(op)) if op == "."))
        && let Some(rel) = by_table.get(&name)
    {
        return Some((
            format!(
                "{}.{}",
                bracket_ident(&rel.remote_schema).ok()?,
                bracket_ident(&rel.remote_table).ok()?
            ),
            1,
        ));
    }

    None
}

fn bracket_ident(name: &str) -> Result<String, TranslateError> {
    if name.contains(']') || name.is_empty() {
        return Err(TranslateError::InvalidIdentifier(name.to_string()));
    }
    Ok(format!("[{name}]"))
}

fn last_piece_is(out: &[String], words: &[&str]) -> bool {
    out.last()
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
