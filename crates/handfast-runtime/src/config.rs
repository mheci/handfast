//! Plugin configuration shared by every plugin on this host.
//!
//! Upstream stores per-plugin settings in a KConfig file and lets each plugin
//! read its own keys. Handfast keeps the same model but one object: the daemon
//! loads it once at start, and every plugin instance reads it through
//! [`PluginContext::config`].
//!
//! The config is deliberately explicit rather than permissive. A plugin that
//! needs a list — `runcommand`'s allowed commands, `sendnotifications`'s
//! app filter — gets an empty list when the key is absent, so the safe
//! behavior is the default and a user has to opt in to anything active.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Settings for every plugin, keyed by section then key.
///
/// Sections are plugin names, matching [`crate::PluginMeta::name`]. Keys inside
/// a section are plugin-defined strings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginConfig {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl PluginConfig {
    /// An empty config: every plugin sees no keys and takes its safe default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty config that lives as long as the process.
    ///
    /// Used by tests and by any call site that has no settings to pass, so a
    /// missing config never has to be modelled as an `Option`.
    #[must_use]
    pub fn empty() -> &'static Self {
        static EMPTY: std::sync::OnceLock<PluginConfig> = std::sync::OnceLock::new();
        EMPTY.get_or_init(PluginConfig::new)
    }

    /// Read the config from a TOML file.
    ///
    /// The format is a table per plugin:
    ///
    /// ```toml
    /// [runcommand]
    /// commands = "lock:loginctl lock-session"
    ///
    /// [sendnotifications]
    /// include_body = "true"
    /// ```
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or is not valid TOML with string
    /// values. An absent file is an error: pass [`Self::new`] for no config.
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// Parse the config from TOML text.
    ///
    /// # Errors
    ///
    /// Fails when the text is not valid TOML, or a plugin table holds a value
    /// that is not a string, integer or boolean.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: toml_lite::Value = toml_lite::parse(text)?;
        let mut sections = BTreeMap::new();
        for (plugin, entries) in value.into_tables()? {
            let mut section = BTreeMap::new();
            for (key, value) in entries {
                section.insert(key, value.into_scalar()?);
            }
            sections.insert(plugin, section);
        }
        Ok(Self { sections })
    }

    /// Add or replace one key.
    pub fn insert(&mut self, plugin: &str, key: &str, value: impl Into<String>) {
        self.sections
            .entry(plugin.to_string())
            .or_default()
            .insert(key.to_string(), value.into());
    }

    /// One key from one plugin section.
    #[must_use]
    pub fn get(&self, plugin: &str, key: &str) -> Option<&str> {
        self.sections
            .get(plugin)
            .and_then(|section| section.get(key))
            .map(String::as_str)
    }

    /// One key parsed as a boolean.
    ///
    /// `true`/`false` in any case, plus `1`/`0`. Anything else is `None`, so a
    /// typo cannot silently enable a feature.
    #[must_use]
    pub fn get_bool(&self, plugin: &str, key: &str) -> Option<bool> {
        match self.get(plugin, key)? {
            value if value.eq_ignore_ascii_case("true") || value == "1" => Some(true),
            value if value.eq_ignore_ascii_case("false") || value == "0" => Some(false),
            _ => None,
        }
    }

    /// One key parsed as a boolean with a default when it is absent or invalid.
    #[must_use]
    pub fn get_bool_or(&self, plugin: &str, key: &str, default: bool) -> bool {
        self.get_bool(plugin, key).unwrap_or(default)
    }

    /// One key split on commas into a set of trimmed, non-empty entries.
    ///
    /// Used for the allowlists that decide what a plugin may act on. An absent
    /// key yields an empty set, which every consumer treats as "nothing
    /// allowed", so forgetting the key is safe rather than open.
    #[must_use]
    pub fn get_list(&self, plugin: &str, key: &str) -> BTreeSet<String> {
        self.get(plugin, key)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether a plugin has any configuration at all.
    #[must_use]
    pub fn has_section(&self, plugin: &str) -> bool {
        self.sections
            .get(plugin)
            .is_some_and(|section| !section.is_empty())
    }

    /// Every plugin name that has a section, for diagnostics.
    #[must_use]
    pub fn sections(&self) -> impl Iterator<Item = &str> {
        self.sections.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_is_inert() {
        let config = PluginConfig::new();
        assert_eq!(config.get("runcommand", "commands"), None);
        assert!(!config.get_bool_or("runcommand", "enabled", false));
        assert!(config.get_list("runcommand", "commands").is_empty());
        assert!(!config.has_section("runcommand"));
    }

    #[test]
    fn a_missing_allowlist_is_empty_not_open() {
        let config = PluginConfig::parse("[runcommand]\nenabled = true\n").unwrap();
        assert_eq!(config.get_bool("runcommand", "enabled"), Some(true));
        assert!(
            config.get_list("runcommand", "commands").is_empty(),
            "a plugin with no allowlist must allow nothing"
        );
    }

    #[test]
    fn lists_are_split_trimmed_and_deduplicated() {
        let config =
            PluginConfig::parse("[sendnotifications]\napps = \"firefox, kmail ,, firefox\"\n")
                .unwrap();
        let apps = config.get_list("sendnotifications", "apps");
        assert_eq!(apps.len(), 2);
        assert!(apps.contains("firefox"));
        assert!(apps.contains("kmail"));
    }

    #[test]
    fn booleans_accept_the_common_spellings_and_reject_typos() {
        let config = PluginConfig::parse(
            "[p]\na = \"true\"\nb = \"FALSE\"\nc = \"1\"\nd = \"0\"\ne = \"yes\"\n",
        )
        .unwrap();
        assert_eq!(config.get_bool("p", "a"), Some(true));
        assert_eq!(config.get_bool("p", "b"), Some(false));
        assert_eq!(config.get_bool("p", "c"), Some(true));
        assert_eq!(config.get_bool("p", "d"), Some(false));
        assert_eq!(config.get_bool("p", "e"), None);
    }

    #[test]
    fn integers_and_booleans_become_strings() {
        let config = PluginConfig::parse("[p]\ncount = 5\nflag = true\n").unwrap();
        assert_eq!(config.get("p", "count"), Some("5"));
        assert_eq!(config.get("p", "flag"), Some("true"));
    }

    #[test]
    fn sections_are_reported() {
        let config = PluginConfig::parse("[runcommand]\nenabled = true\n").unwrap();
        assert!(config.has_section("runcommand"));
        assert_eq!(config.sections().collect::<Vec<_>>(), vec!["runcommand"]);
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut config = PluginConfig::new();
        config.insert("sftp", "mount", "/tmp/x");
        assert_eq!(config.get("sftp", "mount"), Some("/tmp/x"));
        assert!(config.has_section("sftp"));
    }
}

/// A very small TOML subset: tables of scalar string values.
///
/// The workspace has no TOML dependency and adding one for a handful of
/// `key = "value"` lines is not worth the supply-chain surface. This parser
/// accepts exactly what a plugin config needs and rejects everything else with
/// a line number, so a malformed config fails at start rather than at runtime.
mod toml_lite {
    use std::collections::BTreeMap;

    /// One scalar value as written in the file.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Scalar {
        String(String),
        Integer(i64),
        Boolean(bool),
    }

    impl Scalar {
        pub(super) fn into_string(self) -> String {
            match self {
                Self::String(value) => value,
                Self::Integer(value) => value.to_string(),
                Self::Boolean(value) => value.to_string(),
            }
        }
    }

    /// A parsed document: tables in order, each holding scalar entries.
    #[derive(Debug, Default)]
    pub(super) struct Value {
        tables: Vec<(String, Vec<(String, Scalar)>)>,
    }

    impl Value {
        pub(super) fn into_tables(self) -> Result<Vec<(String, Vec<(String, Scalar)>)>, String> {
            Ok(self.tables)
        }
    }

    impl Scalar {
        pub(super) fn into_scalar(self) -> Result<String, String> {
            Ok(self.into_string())
        }
    }

    /// Parse the document.
    ///
    /// # Errors
    ///
    /// Fails on a duplicate table, a key outside a table, a line that is not a
    /// table header or an assignment, or an unterminated string.
    pub(super) fn parse(text: &str) -> Result<Value, String> {
        let mut value = Value::default();
        let mut current: Option<usize> = None;
        for (index, raw) in text.lines().enumerate() {
            let line_number = index + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                let Some(name) = rest.strip_suffix(']') else {
                    return Err(format!("line {line_number}: unterminated table header"));
                };
                let name = name.trim();
                if name.is_empty() {
                    return Err(format!("line {line_number}: empty table name"));
                }
                if value.tables.iter().any(|(existing, _)| existing == name) {
                    return Err(format!("line {line_number}: duplicate table [{name}]"));
                }
                value.tables.push((name.to_string(), Vec::new()));
                current = Some(value.tables.len() - 1);
                continue;
            }
            let Some(index) = current else {
                return Err(format!(
                    "line {line_number}: key outside a table; start with [plugin]"
                ));
            };
            let Some((key, rest)) = line.split_once('=') else {
                return Err(format!("line {line_number}: expected key = value"));
            };
            let key = key.trim();
            if key.is_empty() {
                return Err(format!("line {line_number}: empty key"));
            }
            let scalar = parse_scalar(rest.trim(), line_number)?;
            let table = &mut value.tables[index].1;
            if table.iter().any(|(existing, _)| existing == key) {
                return Err(format!("line {line_number}: duplicate key {key}"));
            }
            table.push((key.to_string(), scalar));
        }
        Ok(value)
    }

    fn parse_scalar(text: &str, line_number: usize) -> Result<Scalar, String> {
        match text.as_bytes().first() {
            Some(b'"') => {
                let rest = &text[1..];
                let Some(end) = rest.find('"') else {
                    return Err(format!("line {line_number}: unterminated string"));
                };
                let trailing = rest[end + 1..].trim();
                if !trailing.is_empty() && !trailing.starts_with('#') {
                    return Err(format!("line {line_number}: trailing text after string"));
                }
                Ok(Scalar::String(unescape(&rest[..end])))
            }
            Some(b'\'') => {
                let rest = &text[1..];
                let Some(end) = rest.find('\'') else {
                    return Err(format!("line {line_number}: unterminated string"));
                };
                Ok(Scalar::String(rest[..end].to_string()))
            }
            _ => {
                // Strip a trailing comment before interpreting the scalar.
                let bare = match text.split_once(" #") {
                    Some((head, _)) => head.trim(),
                    None => text,
                };
                match bare {
                    "true" => Ok(Scalar::Boolean(true)),
                    "false" => Ok(Scalar::Boolean(false)),
                    other => other.parse::<i64>().map(Scalar::Integer).map_err(|_| {
                        format!("line {line_number}: expected a string, integer or boolean")
                    }),
                }
            }
        }
    }

    fn unescape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(character) = chars.next() {
            if character != '\\' {
                out.push(character);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_tables_and_scalars() {
            let value = parse("[a]\nx = \"one\"\ny = 2\nz = true\n# comment\n\n[b]\nq = 'lit'\n")
                .unwrap();
            assert_eq!(value.tables.len(), 2);
            assert_eq!(value.tables[0].0, "a");
            assert_eq!(value.tables[0].1.len(), 3);
            assert_eq!(value.tables[1].0, "b");
        }

        #[test]
        fn rejects_malformed_input() {
            assert!(parse("x = 1\n").is_err(), "key outside a table");
            assert!(parse("[a\n").is_err(), "unterminated header");
            assert!(parse("[a]\n[a]\n").is_err(), "duplicate table");
            assert!(parse("[a]\nx = 1\nx = 2\n").is_err(), "duplicate key");
            assert!(parse("[a]\nx = \"open\n").is_err(), "unterminated string");
            assert!(parse("[a]\nx = nope\n").is_err(), "not a scalar");
        }

        #[test]
        fn escapes_are_decoded() {
            let value = parse("[a]\nx = \"a\\tb\\nc\\\\d\"\n").unwrap();
            assert_eq!(
                value.tables[0].1[0].1,
                Scalar::String("a\tb\nc\\d".to_string())
            );
        }
    }
}
