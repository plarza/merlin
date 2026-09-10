//! Query syntax, shared by the memory and message searches.
//!
//! One convention borrowed from web search: a quoted term is a requirement, and everything unquoted describes the subject.
//! `world cup "2025"` means rows that definitely contain 2025, ranked by how much they are about the world cup, whatever words they used for it.

/// One parsed query term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub exact: bool,
}

/// Split a query into terms, treating double-quoted runs as exact.
/// An unterminated quote is treated as if it closed at the end.
pub fn parse(query: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;

    let flush = |buf: &mut String, exact: bool, terms: &mut Vec<Term>| {
        let text = buf.trim().to_lowercase();
        buf.clear();
        // Trigram needs three characters; exact terms are useful shorter.
        let floor = if exact { 1 } else { 3 };
        if text.chars().count() >= floor {
            terms.push(Term { text, exact });
        }
    };

    for c in query.chars() {
        match c {
            '"' => {
                flush(&mut buf, in_quotes, &mut terms);
                in_quotes = !in_quotes;
            }
            c if c.is_whitespace() && !in_quotes => flush(&mut buf, false, &mut terms),
            c if c.is_alphanumeric() || in_quotes => buf.push(c),
            _ => flush(&mut buf, false, &mut terms),
        }
    }
    flush(&mut buf, in_quotes, &mut terms);
    terms
}

/// The unquoted part of a query, which is the part that gets embedded.
///
/// Taken from the raw text rather than the parsed terms, because the embedder wants natural phrasing and parsing drops the short words that carry the grammar.
/// Empty when every term was quoted, in which case no embedding is needed at all.
pub fn loose_text(query: &str) -> String {
    let mut out = String::new();
    let mut in_quotes = false;
    for c in query.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if !in_quotes => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Quoted terms ANDed together: every one must be present.
pub fn required_expr(exact: &[&Term]) -> String {
    exact
        .iter()
        .map(|t| format!("\"{}\"", escape(&t.text)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// FTS5 string literals escape a quote by doubling it.
pub fn escape(term: &str) -> String {
    term.replace('"', "\"\"")
}
