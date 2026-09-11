#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub exact: bool,
}

pub fn parse(query: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;

    let flush = |buf: &mut String, exact: bool, terms: &mut Vec<Term>| {
        let text = buf.trim().to_lowercase();
        buf.clear();
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

pub fn required_expr(exact: &[&Term]) -> String {
    exact
        .iter()
        .map(|t| format!("\"{}\"", escape(&t.text)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub fn escape(term: &str) -> String {
    term.replace('"', "\"\"")
}
