//! Generic property storage (`cmPropertyMap`).

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct Props {
    map: BTreeMap<String, String>,
}

impl Props {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(String::as_str)
    }

    pub fn set(&mut self, name: &str, value: String) {
        self.map.insert(name.to_owned(), value);
    }

    pub fn remove(&mut self, name: &str) {
        self.map.remove(name);
    }

    /// `cmPropertyMap::AppendProperty`: list-append (or string concat).
    pub fn append(&mut self, name: &str, value: &str, as_string: bool) {
        if value.is_empty() {
            return;
        }
        match self.map.get_mut(name) {
            Some(existing) if !existing.is_empty() => {
                if !as_string {
                    existing.push(';');
                }
                existing.push_str(value);
            }
            _ => {
                self.map.insert(name.to_owned(), value.to_owned());
            }
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.map.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.map.keys()
    }
}
