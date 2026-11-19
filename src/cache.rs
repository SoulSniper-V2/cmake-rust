use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub(crate) type Cache = BTreeMap<String, CacheEntry>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(from = "StoredEntry")]
pub(crate) struct CacheEntry {
    pub value: String,
    pub kind: String,
    pub help: String,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredEntry {
    Legacy(String),
    Typed {
        value: String,
        kind: String,
        help: String,
        #[serde(default)]
        properties: BTreeMap<String, String>,
    },
}

impl From<StoredEntry> for CacheEntry {
    fn from(entry: StoredEntry) -> Self {
        match entry {
            StoredEntry::Legacy(value) => Self::new(value, "UNINITIALIZED", ""),
            StoredEntry::Typed {
                value,
                kind,
                help,
                properties,
            } => Self {
                value,
                kind,
                help,
                properties,
            },
        }
    }
}

impl CacheEntry {
    pub fn new(value: impl Into<String>, kind: &str, help: &str) -> Self {
        Self {
            value: value.into(),
            kind: kind.into(),
            help: help.into(),
            properties: BTreeMap::new(),
        }
    }

    pub fn property(&self, name: &str) -> Option<&str> {
        match name {
            "VALUE" => Some(&self.value),
            "TYPE" => Some(&self.kind),
            "HELPSTRING" => Some(&self.help),
            _ => self.properties.get(name).map(String::as_str),
        }
    }

    pub fn set_property(&mut self, name: &str, value: String) {
        match name {
            "VALUE" => self.value = value,
            "TYPE" => self.kind = value,
            "HELPSTRING" => self.help = value,
            _ => {
                self.properties.insert(name.to_owned(), value);
            }
        }
    }
}
