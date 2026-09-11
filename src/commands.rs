#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Help,
    Model(Option<&'a str>),
    Reasoning(Option<&'a str>),
}

impl<'a> Command<'a> {
    pub fn parse(body: &'a str) -> Option<Self> {
        match body {
            "/help" => Some(Self::Help),
            "/model" => Some(Self::Model(None)),
            "/reasoning" => Some(Self::Reasoning(None)),
            _ => {
                if let Some(model) = argument(body, "/model ") {
                    return Some(Self::Model(Some(model)));
                }
                if let Some(effort) = argument(body, "/reasoning ") {
                    return Some(Self::Reasoning(Some(effort)));
                }
                None
            }
        }
    }
}

fn argument<'a>(body: &'a str, prefix: &str) -> Option<&'a str> {
    let value = body.strip_prefix(prefix)?;
    (!value.is_empty() && !value.chars().any(char::is_whitespace)).then_some(value)
}
