use crate::error::{Error, Result};
use crate::version::Version;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_ENTRIES: usize = 2000;
pub const MAX_ITEMS: usize = 500;
pub const MAX_TEXT: usize = 2000;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Section {
    #[serde(default)]
    pub title: String,
    pub items: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub version: String,
    #[serde(default)]
    pub date: String,
    pub sections: Vec<Section>,
}

fn err(why: impl std::fmt::Display) -> Error {
    Error::Metadata(format!("changelog: {why}"))
}

fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        match after.find("](").and_then(|mid| after[mid + 2..].find(')').map(|close| (mid, mid + 2 + close))) {
            Some((mid, close)) if !after[..mid].contains('[') => {
                out.push_str(&rest[..open]);
                out.push_str(&after[..mid]);
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace("__", "").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn heading(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("## ")?.trim();
    let (head, date) = match rest.split_once(" - ").or_else(|| rest.split_once(" – ")) {
        Some((h, d)) => (h.trim(), d.trim()),
        None => (rest, ""),
    };
    let version = head.trim_start_matches('[').trim_end_matches(']').trim().trim_start_matches(['v', 'V']).to_string();
    Some((version, date.to_string()))
}

fn bullet(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ "))
}

fn is_link_ref(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.contains("]:")
}

pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<Entry> = None;
    let mut skipping = false;
    let mut item_open = false;
    let mut seen = HashSet::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.starts_with("## ") {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            item_open = false;
            let (version, date) = heading(line).ok_or_else(|| err(format!("bad heading {line}")))?;
            if version.eq_ignore_ascii_case("unreleased") {
                skipping = true;
                continue;
            }
            skipping = false;
            Version::parse(&version).map_err(|_| err(format!("{version} is not a version (x.y.z)")))?;
            if !seen.insert(version.clone()) {
                return Err(err(format!("{version} is listed twice")));
            }
            current = Some(Entry { version, date, sections: Vec::new() });
            continue;
        }
        if skipping {
            continue;
        }
        let Some(entry) = current.as_mut() else { continue };
        if let Some(title) = line.strip_prefix("### ") {
            entry.sections.push(Section { title: plain(title), items: Vec::new() });
            item_open = false;
            continue;
        }
        if line.trim().is_empty() || is_link_ref(line) || line.starts_with('#') {
            item_open = false;
            continue;
        }
        if let Some(text) = bullet(line) {
            if entry.sections.is_empty() {
                entry.sections.push(Section::default());
            }
            entry.sections.last_mut().expect("section").items.push(plain(text));
            item_open = true;
        } else if item_open && raw.starts_with([' ', '\t']) {
            let item = entry.sections.last_mut().and_then(|s| s.items.last_mut()).expect("item");
            item.push(' ');
            item.push_str(&plain(line));
        }
    }
    if let Some(e) = current.take() {
        entries.push(e);
    }
    for e in &mut entries {
        e.sections.retain(|s| !s.items.is_empty());
    }
    entries.sort_by(|a, b| Version::parse(&b.version).ok().cmp(&Version::parse(&a.version).ok()));
    validate(&entries)?;
    Ok(entries)
}

pub fn validate(entries: &[Entry]) -> Result<()> {
    if entries.len() > MAX_ENTRIES {
        return Err(err("too many versions"));
    }
    let mut seen = HashSet::new();
    for e in entries {
        Version::parse(&e.version)?;
        if !seen.insert(&e.version) {
            return Err(err(format!("{} is listed twice", e.version)));
        }
        if e.date.len() > 64 || e.sections.iter().map(|s| s.items.len()).sum::<usize>() > MAX_ITEMS {
            return Err(err(format!("{} is too long", e.version)));
        }
        if e.sections.iter().any(|s| s.title.len() > MAX_TEXT || s.items.iter().any(|i| i.len() > MAX_TEXT)) {
            return Err(err(format!("{} has an overlong line", e.version)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Changelog\n\nAll notable changes.\n\n## [Unreleased]\n### Added\n- not yet\n\n## [0.2.0] - 2026-11-01\n### Added\n- Force close from the **patcher**\n- See [docs](https://example.com) for\n  details\n### Fixed\n* Crash on exit\n\n## 0.1.0 - 2026-10-10\n- First release\n\n[Unreleased]: https://example.com/compare\n[0.2.0]: https://example.com/0.2.0\n";

    #[test]
    fn parses_keep_a_changelog() {
        let e = parse(SAMPLE).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].version, "0.2.0");
        assert_eq!(e[0].date, "2026-11-01");
        assert_eq!(e[0].sections[0].title, "Added");
        assert_eq!(e[0].sections[0].items, vec!["Force close from the patcher", "See docs for details"]);
        assert_eq!(e[0].sections[1].items, vec!["Crash on exit"]);
        assert_eq!(e[1].version, "0.1.0");
        assert_eq!(e[1].sections[0].title, "");
        assert_eq!(e[1].sections[0].items, vec!["First release"]);
    }

    #[test]
    fn rejects_bad_and_duplicate_versions() {
        assert!(parse("## [banana]\n- x\n").is_err());
        assert!(parse("## 1.0.0\n- a\n## [1.0.0]\n- b\n").is_err());
    }
}
