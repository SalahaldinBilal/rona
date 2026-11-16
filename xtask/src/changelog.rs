const UNRELEASED: &str = "## [Unreleased]";

/// Turns the Unreleased section into `## [<version>] - <date>` and leaves an empty Unreleased above it.
/// Also returns whether the section had no notes.
pub fn cut_release(changelog: &str, version: &str, date: &str) -> Result<(String, bool), String> {
    let start = changelog.find(UNRELEASED).ok_or(format!("no \"{UNRELEASED}\" heading in CHANGELOG.md"))?;
    let body_start = start + UNRELEASED.len();
    let end = section_end(changelog, body_start);
    let body = changelog[body_start..end].trim();
    let released = format!("## [{version}] - {date}");
    let replacement = if body.is_empty() {
        format!("{UNRELEASED}\n\n{released}\n")
    } else {
        format!("{UNRELEASED}\n\n{released}\n\n{body}\n")
    };
    Ok((format!("{}{replacement}{}", &changelog[..start], &changelog[end..]), body.is_empty()))
}

/// The notes under `## [<version>]`, which become the release notes.
pub fn release_notes(changelog: &str, version: &str) -> Result<String, String> {
    let heading = format!("## [{version}]");
    let start = changelog
        .find(&heading)
        .ok_or(format!("no \"{heading}\" section in CHANGELOG.md, run `cargo xtask bump {version}` before tagging"))?;
    let body_start = changelog[start..].find('\n').map_or(changelog.len(), |line_end| start + line_end + 1);
    Ok(changelog[body_start..section_end(changelog, body_start)].trim().to_string())
}

fn section_end(changelog: &str, from: usize) -> usize {
    changelog[from..].find("\n## [").map_or(changelog.len(), |offset| from + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGELOG: &str = "# Changelog

## [Unreleased]

### Added
- Chapter skipping.

## [0.1.0] - 2026-10-01

### Added
- First release.
";

    #[test]
    fn cutting_a_release_moves_the_notes_under_the_version() {
        let (updated, empty) = cut_release(CHANGELOG, "0.2.0", "2026-11-16").unwrap();
        assert!(!empty);
        assert!(updated.contains("## [Unreleased]\n\n## [0.2.0] - 2026-11-16\n\n### Added\n- Chapter skipping.\n"));
        assert!(updated.contains("## [0.1.0] - 2026-10-01\n\n### Added\n- First release.\n"));
        assert_eq!(release_notes(&updated, "0.2.0").unwrap(), "### Added\n- Chapter skipping.");
    }

    #[test]
    fn an_empty_unreleased_section_is_reported() {
        let changelog = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-10-01\n\n- First release.\n";
        let (updated, empty) = cut_release(changelog, "0.1.1", "2026-11-16").unwrap();
        assert!(empty);
        assert_eq!(release_notes(&updated, "0.1.1").unwrap(), "");
    }

    #[test]
    fn notes_of_the_last_section_run_to_the_end() {
        assert_eq!(release_notes(CHANGELOG, "0.1.0").unwrap(), "### Added\n- First release.");
    }

    #[test]
    fn a_missing_version_is_an_error() {
        assert!(release_notes(CHANGELOG, "0.3.0").is_err());
    }
}
