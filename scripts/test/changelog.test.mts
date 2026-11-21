import assert from "node:assert/strict";
import { test } from "node:test";

import { cutRelease, releaseNotes } from "../lib/changelog.mts";

const CHANGELOG = `# Changelog

## [Unreleased]

### Added
- Chapter skipping.

## [0.1.0] - 2026-10-01

### Added
- First release.
`;

test("cutting a release moves the notes under the version", () => {
    const { text, empty } = cutRelease(CHANGELOG, "0.2.0", "2026-11-16");
    assert.equal(empty, false);
    assert.ok(text.includes("## [Unreleased]\n\n## [0.2.0] - 2026-11-16\n\n### Added\n- Chapter skipping.\n"));
    assert.ok(text.includes("## [0.1.0] - 2026-10-01\n\n### Added\n- First release.\n"));
    assert.equal(releaseNotes(text, "0.2.0"), "### Added\n- Chapter skipping.");
});

test("an empty Unreleased section is reported", () => {
    const changelog = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-10-01\n\n- First release.\n";
    const { text, empty } = cutRelease(changelog, "0.1.1", "2026-11-16");
    assert.equal(empty, true);
    assert.equal(releaseNotes(text, "0.1.1"), "");
});

test("the notes of the last section run to the end", () => {
    assert.equal(releaseNotes(CHANGELOG, "0.1.0"), "### Added\n- First release.");
});

test("a missing version is an error", () => {
    assert.throws(() => releaseNotes(CHANGELOG, "0.3.0"));
});
