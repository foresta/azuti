use std::ops::Range;

use super::token::{tokenize, Token, TokenKind};
use crate::catalog::IdentPart;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Relation,
    Column,
    Database,
    Schema,
    Keyword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    pub expect: Expect,
    pub qualifier: Vec<IdentPart>,
    pub prefix: IdentPart,
    pub replace: Range<usize>,
}

const RELATION_AFTER: &[&str] = &["FROM", "JOIN", "INTO", "UPDATE", "TABLE"];
/// Relations only when they open the statement: `DESC` is also the sort order
/// in `ORDER BY x DESC`.
const RELATION_AFTER_LEADING: &[&str] = &["DESCRIBE", "DESC"];
const COLUMN_AFTER: &[&str] = &[
    "SELECT", "WHERE", "ON", "BY", "HAVING", "AND", "OR", "NOT", "QUALIFY", "DISTINCT",
];

/// Returns None when the cursor is inside a string literal or a comment.
pub fn analyze(sql: &str, cursor: usize) -> Option<CompletionContext> {
    let tokens = tokenize(sql);
    let inside = |t: &Token| t.span.start < cursor && (cursor < t.span.end || !t.terminated);
    if tokens
        .iter()
        .any(|t| matches!(t.kind, TokenKind::Str | TokenKind::Comment) && inside(t))
    {
        return None;
    }

    let before: Vec<&Token> = tokens
        .iter()
        .filter(|t| t.span.start < cursor && t.kind != TokenKind::Comment)
        .collect();
    let stmt_start = before
        .iter()
        .rposition(|t| t.kind == TokenKind::Semicolon)
        .map_or(0, |i| i + 1);
    let before = &before[stmt_start..];

    let ident = |t: &Token| matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent);
    let mut idx = before.len();
    let (prefix, replace) = match before.last() {
        Some(t) if ident(t) && t.span.end >= cursor => {
            idx -= 1;
            (part(sql, t, cursor), t.span.clone())
        }
        _ => (
            IdentPart {
                text: String::new(),
                quoted: false,
            },
            cursor..cursor,
        ),
    };

    let mut qualifier = Vec::new();
    while idx >= 2 && before[idx - 1].kind == TokenKind::Dot && ident(before[idx - 2]) {
        qualifier.push(part(sql, before[idx - 2], before[idx - 2].span.end));
        idx -= 2;
    }
    qualifier.reverse();

    let expect = expect_from(sql, &before[..idx]);
    Some(CompletionContext {
        expect,
        qualifier,
        prefix,
        replace,
    })
}

fn part(sql: &str, t: &Token, cursor: usize) -> IdentPart {
    let end = cursor.min(t.span.end);
    match t.kind {
        TokenKind::QuotedIdent => {
            let inner = &sql[t.span.start + 1..end];
            let inner = inner
                .strip_suffix('"')
                .filter(|_| t.terminated && end == t.span.end);
            IdentPart {
                text: inner
                    .unwrap_or(&sql[t.span.start + 1..end])
                    .replace("\"\"", "\""),
                quoted: true,
            }
        }
        _ => IdentPart {
            text: sql[t.span.start..end].to_string(),
            quoted: false,
        },
    }
}

fn keyword(sql: &str, t: &Token) -> Option<String> {
    (t.kind == TokenKind::Word).then(|| sql[t.span.clone()].to_ascii_uppercase())
}

fn expect_from(sql: &str, before: &[&Token]) -> Expect {
    let Some(prev) = before.last() else {
        return Expect::Keyword;
    };
    if prev.kind == TokenKind::Comma {
        return clause_of_list(sql, before);
    }
    let Some(kw) = keyword(sql, prev) else {
        return if prev.kind == TokenKind::LParen || prev.kind == TokenKind::Op {
            Expect::Column
        } else {
            Expect::Keyword
        };
    };
    let prev2 = before
        .len()
        .checked_sub(2)
        .and_then(|i| keyword(sql, before[i]));
    match kw.as_str() {
        "DATABASE" if prev2.as_deref() == Some("USE") => Expect::Database,
        "SCHEMA" if prev2.as_deref() == Some("USE") => Expect::Schema,
        k if RELATION_AFTER.contains(&k) => Expect::Relation,
        k if RELATION_AFTER_LEADING.contains(&k) && before.len() == 1 => Expect::Relation,
        k if COLUMN_AFTER.contains(&k) => Expect::Column,
        _ => Expect::Keyword,
    }
}

/// After a comma, the list belongs to the nearest clause keyword at the same
/// parenthesis depth (`FROM a, b` lists relations, `SELECT a, b` columns).
fn clause_of_list(sql: &str, before: &[&Token]) -> Expect {
    let mut depth = 0i32;
    for t in before.iter().rev() {
        match t.kind {
            TokenKind::RParen => depth += 1,
            TokenKind::LParen if depth == 0 => return Expect::Column,
            TokenKind::LParen => depth -= 1,
            TokenKind::Word if depth == 0 => match keyword(sql, t).as_deref() {
                Some("FROM") => return Expect::Relation,
                Some(k) if COLUMN_AFTER.contains(&k) => return Expect::Column,
                _ => {}
            },
            _ => {}
        }
    }
    Expect::Keyword
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|` marks the cursor.
    fn at(sql_with_cursor: &str) -> Option<CompletionContext> {
        let cursor = sql_with_cursor.find('|').unwrap();
        let sql = sql_with_cursor.replacen('|', "", 1);
        analyze(&sql, cursor)
    }

    fn texts(parts: &[IdentPart]) -> Vec<&str> {
        parts.iter().map(|p| p.text.as_str()).collect()
    }

    #[test]
    fn relation_after_from_with_prefix() {
        let c = at("SELECT * FROM ord|").unwrap();
        assert_eq!(c.expect, Expect::Relation);
        assert_eq!(c.prefix.text, "ord");
        assert_eq!(c.replace, 14..17);
        assert!(c.qualifier.is_empty());
    }

    #[test]
    fn relation_with_qualifier_and_empty_prefix() {
        let c = at("select * from db.sch.|").unwrap();
        assert_eq!(c.expect, Expect::Relation);
        assert_eq!(texts(&c.qualifier), vec!["db", "sch"]);
        assert_eq!(c.prefix.text, "");
    }

    #[test]
    fn quoted_qualifier_and_open_quoted_prefix() {
        let c = at("SELECT * FROM \"My Db\".PUBLIC.\"or|").unwrap();
        assert_eq!(c.expect, Expect::Relation);
        assert_eq!(
            c.qualifier[0],
            IdentPart {
                text: "My Db".into(),
                quoted: true
            }
        );
        assert_eq!(
            c.prefix,
            IdentPart {
                text: "or".into(),
                quoted: true
            }
        );
    }

    #[test]
    fn join_and_comma_lists_are_relations() {
        assert_eq!(
            at("SELECT * FROM a JOIN b|").unwrap().expect,
            Expect::Relation
        );
        assert_eq!(at("SELECT * FROM a, |").unwrap().expect, Expect::Relation);
        assert_eq!(at("SELECT a, |").unwrap().expect, Expect::Column);
    }

    #[test]
    fn desc_is_a_relation_only_at_the_start_of_a_statement() {
        assert_eq!(at("DESC ord|").unwrap().expect, Expect::Relation);
        assert_eq!(at("SELECT 1; DESCRIBE t|").unwrap().expect, Expect::Relation);
        assert_ne!(at("SELECT * FROM t ORDER BY a DESC LIM|").unwrap().expect, Expect::Relation);
    }

    #[test]
    fn comments_between_keyword_and_name_are_skipped() {
        assert_eq!(at("SELECT * FROM /* c */ ord|").unwrap().expect, Expect::Relation);
        assert_eq!(at("SELECT * FROM -- c\n ord|").unwrap().expect, Expect::Relation);
    }

    #[test]
    fn replace_covers_the_whole_word_under_the_cursor() {
        let c = at("SELECT * FROM ord|ers").unwrap();
        assert_eq!(c.prefix.text, "ord");
        assert_eq!(c.replace, 14..20);
    }

    #[test]
    fn only_the_statement_under_the_cursor_counts() {
        assert_eq!(
            at("SELECT * FROM a; SELECT |").unwrap().expect,
            Expect::Column
        );
    }

    #[test]
    fn no_completion_inside_strings_or_comments() {
        assert!(at("SELECT 'FROM |").is_none());
        assert!(at("SELECT 1 -- FROM |").is_none());
    }

    #[test]
    fn use_database_and_schema() {
        assert_eq!(at("USE DATABASE |").unwrap().expect, Expect::Database);
        assert_eq!(at("USE SCHEMA |").unwrap().expect, Expect::Schema);
    }
}
