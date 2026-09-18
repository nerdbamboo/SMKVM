//! A reader for Qt's `QSettings` INI files, which is the shape Barrier's GUI
//! configuration takes.
//!
//! Qt writes nested structures by flattening them into dotted-with-backslashes
//! keys — `screens\8\name=WIN-STUDY` — and percent-escapes characters that
//! would otherwise be significant. Both need undoing to get at the values.

use std::collections::BTreeMap;

/// A parsed settings file: section name to key/value pairs. Keys keep their
/// backslash-separated structure so callers can pick out arrays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ini {
    pub sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl Ini {
    pub fn parse(text: &str) -> Ini {
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current = String::from("General");

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                current = unescape(name);
                sections.entry(current.clone()).or_default();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            sections
                .entry(current.clone())
                .or_default()
                .insert(unescape(key.trim()), unquote(value.trim()));
        }
        sections.entry("General".into()).or_default();
        Ini { sections }
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(String::as_str)
    }

    /// Look a key up in `section`, falling back to `General`. Barrier spreads
    /// related settings across both.
    pub fn get_any(&self, section: &str, key: &str) -> Option<&str> {
        self.get(section, key).or_else(|| self.get("General", key))
    }

    pub fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        match self.get_any(section, key)?.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        }
    }

    pub fn get_u32(&self, section: &str, key: &str) -> Option<u32> {
        self.get_any(section, key)?.parse().ok()
    }

    /// Entries of a Qt-style array: `prefix\<n>\<field>`.
    ///
    /// Returns `(index, field, value)` triples so a caller can rebuild the
    /// structure without knowing its shape in advance.
    pub fn array<'a>(
        &'a self,
        section: &str,
        prefix: &'a str,
    ) -> impl Iterator<Item = (u32, &'a str, &'a str)> + 'a {
        self.sections
            .get(section)
            .into_iter()
            .flat_map(|m| m.iter())
            .filter_map(move |(k, v)| {
                let rest = k.strip_prefix(prefix)?.strip_prefix('\\')?;
                let (idx, field) = rest.split_once('\\')?;
                Some((idx.parse().ok()?, field, v.as_str()))
            })
    }
}

impl From<BTreeMap<String, BTreeMap<String, String>>> for Ini {
    fn from(sections: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        Ini { sections }
    }
}

/// Qt escapes characters it cannot write literally as `%XX`.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                // A NUL is Qt's separator for composite keys; drop it rather
                // than carry an unprintable byte into a section name.
                if b != 0 {
                    out.push(b as char);
                }
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn unquote(s: &str) -> String {
    let trimmed = if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    };
    unescape(trimmed)
}
