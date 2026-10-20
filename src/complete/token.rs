use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Word,
    QuotedIdent,
    Str,
    Number,
    Dot,
    Comma,
    LParen,
    RParen,
    Semicolon,
    Op,
    Comment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Range<usize>,
    /// False for a string, quoted identifier or block comment still open at the
    /// end of the input — the normal state while the user is typing one.
    pub terminated: bool,
}

pub fn tokenize(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = sql[i..].chars().next().expect("i is on a char boundary");
        let start = i;
        let rest = &sql[i..];
        let (kind, end, terminated) = if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        } else if rest.starts_with("--") || rest.starts_with("//") {
            match rest.find('\n') {
                Some(n) => (TokenKind::Comment, i + n, true),
                None => (TokenKind::Comment, sql.len(), false),
            }
        } else if let Some(body) = rest.strip_prefix("/*") {
            match body.find("*/") {
                Some(n) => (TokenKind::Comment, i + 2 + n + 2, true),
                None => (TokenKind::Comment, sql.len(), false),
            }
        } else if let Some(body) = rest.strip_prefix("$$") {
            match body.find("$$") {
                Some(n) => (TokenKind::Str, i + 2 + n + 2, true),
                None => (TokenKind::Str, sql.len(), false),
            }
        } else if c == '\'' {
            let (end, t) = quoted(sql, i, b'\'', true);
            (TokenKind::Str, end, t)
        } else if c == '"' {
            let (end, t) = quoted(sql, i, b'"', false);
            (TokenKind::QuotedIdent, end, t)
        } else if c.is_ascii_digit() {
            let n = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.'))
                .unwrap_or(rest.len());
            (TokenKind::Number, i + n, true)
        } else if c.is_alphabetic() || c == '_' {
            let n = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '$'))
                .unwrap_or(rest.len());
            (TokenKind::Word, i + n, true)
        } else {
            let kind = match c {
                '.' => TokenKind::Dot,
                ',' => TokenKind::Comma,
                '(' => TokenKind::LParen,
                ')' => TokenKind::RParen,
                ';' => TokenKind::Semicolon,
                _ => TokenKind::Op,
            };
            (kind, i + c.len_utf8(), true)
        };
        out.push(Token {
            kind,
            span: start..end,
            terminated,
        });
        i = end;
    }
    out
}

/// Scan a quoted run starting at `start`. A doubled quote is an escaped quote;
/// string literals also allow backslash escapes.
fn quoted(sql: &str, start: usize, q: u8, backslash: bool) -> (usize, bool) {
    let b = sql.as_bytes();
    let mut i = start + 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
        } else if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
            } else {
                return (i + 1, true);
            }
        } else {
            i += 1;
        }
    }
    (sql.len(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokenize(sql).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn splits_a_qualified_name() {
        use TokenKind::*;
        assert_eq!(
            kinds("SELECT * FROM db.\"s c\".t;"),
            vec![Word, Op, Word, Word, Dot, QuotedIdent, Dot, Word, Semicolon]
        );
    }

    #[test]
    fn unterminated_quotes_and_comments_run_to_the_end() {
        let t = tokenize("SELECT 'abc");
        assert_eq!(t[1].kind, TokenKind::Str);
        assert!(!t[1].terminated);
        let t = tokenize("FROM \"my tab");
        assert_eq!(t[1].kind, TokenKind::QuotedIdent);
        assert!(!t[1].terminated);
        let t = tokenize("/* open");
        assert!(!t[0].terminated);
    }

    #[test]
    fn handles_escaped_quotes_and_multibyte_text() {
        let t = tokenize("'it''s' \"a\"\"b\" 日本");
        assert_eq!(t.len(), 3);
        assert!(t.iter().all(|t| t.terminated));
        assert_eq!(t[2].kind, TokenKind::Word);
    }
}
