use std::sync::OnceLock;

pub const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

#[derive(Debug, PartialEq, Eq)]
pub struct ReleaseEntry<'a> {
    pub title: &'a str,
    pub bullets: Vec<&'a str>,
}

/// The changelog uses `##` release headings and single-line `-` bullets.
pub fn parse_changelog(markdown: &str) -> Vec<ReleaseEntry<'_>> {
    let mut entries: Vec<ReleaseEntry<'_>> = Vec::new();
    for line in markdown.lines().map(str::trim_start) {
        if let Some(title) = line.strip_prefix("## ") {
            entries.push(ReleaseEntry {
                title: title.trim(),
                bullets: Vec::new(),
            });
        } else if let Some(bullet) = line.strip_prefix("- ") {
            if let Some(entry) = entries.last_mut() {
                let bullet = bullet.trim();
                if !bullet.is_empty() {
                    entry.bullets.push(bullet);
                }
            }
        }
    }
    entries.retain(|entry| !entry.title.is_empty() && !entry.bullets.is_empty());
    entries
}

pub fn releases() -> &'static [ReleaseEntry<'static>] {
    static RELEASES: OnceLock<Vec<ReleaseEntry<'static>>> = OnceLock::new();
    RELEASES.get_or_init(|| parse_changelog(CHANGELOG))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_changelog_has_release_history_and_nonempty_bullets() {
        let entries = releases();
        assert!(!entries.is_empty());
        assert_eq!(entries[0].title, "Unreleased");
        for version in 46..=55 {
            let title = format!("v0.1.{version}");
            assert!(entries.iter().any(|entry| entry.title == title));
        }
        for entry in entries {
            assert!(!entry.title.is_empty());
            assert!(!entry.bullets.is_empty());
            assert!(entry.bullets.iter().all(|bullet| !bullet.trim().is_empty()));
        }
    }

    #[test]
    fn parses_release_sections_in_order_without_preamble_or_empty_entries() {
        let entries = parse_changelog(
            "# Changelog\r\n- Preamble\r\n\r\n## Unreleased\r\n- Offline notes\r\n\
             ## v1.2.0\n\n- First change\n- Second change\n## Empty\n- \n## \n- Orphan\n",
        );
        assert_eq!(
            entries,
            vec![
                ReleaseEntry {
                    title: "Unreleased",
                    bullets: vec!["Offline notes"],
                },
                ReleaseEntry {
                    title: "v1.2.0",
                    bullets: vec!["First change", "Second change"],
                },
            ]
        );
        assert!(parse_changelog("").is_empty());
    }
}
