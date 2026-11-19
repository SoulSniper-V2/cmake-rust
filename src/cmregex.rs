//! TEMPORARY stand-in for the kwsys regex port (being written separately).
//! Translates the CMake regex syntax to the `regex` crate for now.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Match {
    pub groups: [Option<(usize, usize)>; 10],
}

pub struct Regex {
    inner: regex::bytes::Regex,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, String> {
        let mut translated = String::from("(?-u)");
        let mut chars = pattern.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if let Some(next) = chars.next() {
                        translated.push_str(&regex::escape(&next.to_string()));
                    }
                }
                '{' | '}' => translated.push_str(&regex::escape(&ch.to_string())),
                _ => translated.push(ch),
            }
        }
        regex::bytes::Regex::new(&translated)
            .map(|inner| Regex { inner })
            .map_err(|error| error.to_string())
    }

    pub fn find_at(&self, text: &str, start: usize, _not_bol: bool) -> Option<Match> {
        let captures = self.inner.captures_at(text.as_bytes(), start)?;
        let mut result = Match::default();
        for index in 0..10 {
            result.groups[index] = captures.get(index).map(|m| (m.start(), m.end()));
        }
        Some(result)
    }

    pub fn find(&self, text: &str) -> Option<Match> {
        self.find_at(text, 0, false)
    }
}
