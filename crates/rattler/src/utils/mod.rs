mod encoding;
pub use encoding::{AsyncEncoding, Encoding};

#[cfg(test)]
pub(crate) mod simple_channel_server;

macro_rules! regex {
    ($re:literal $(,)?) => {{
        static RE: once_cell::sync::OnceCell<regex::Regex> = once_cell::sync::OnceCell::new();
        RE.get_or_init(|| regex::Regex::new($re).unwrap())
    }};
}

pub use regex;
