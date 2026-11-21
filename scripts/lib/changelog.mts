import { TaskError } from "./util.mts";

const UNRELEASED = "## [Unreleased]";

/**
 * Turns the Unreleased section into `## [<version>] - <date>` and leaves an empty Unreleased above it.
 * Also says whether the section had no notes.
 */
export function cutRelease(changelog: string, version: string, date: string): { text: string; empty: boolean } {
    const start = changelog.indexOf(UNRELEASED);
    if (start < 0) {
        throw new TaskError(`no "${UNRELEASED}" heading in CHANGELOG.md`);
    }
    const bodyStart = start + UNRELEASED.length;
    const end = sectionEnd(changelog, bodyStart);
    const body = changelog.slice(bodyStart, end).trim();
    const released = `## [${version}] - ${date}`;
    const replacement = body === "" ? `${UNRELEASED}\n\n${released}\n` : `${UNRELEASED}\n\n${released}\n\n${body}\n`;
    return { text: changelog.slice(0, start) + replacement + changelog.slice(end), empty: body === "" };
}

/** The notes under `## [<version>]`, which become the release notes. */
export function releaseNotes(changelog: string, version: string): string {
    const heading = `## [${version}]`;
    const start = changelog.indexOf(heading);
    if (start < 0) {
        throw new TaskError(
            `no "${heading}" section in CHANGELOG.md, run \`npm run version:bump ${version}\` before tagging`,
        );
    }
    const lineEnd = changelog.indexOf("\n", start);
    const bodyStart = lineEnd < 0 ? changelog.length : lineEnd + 1;
    return changelog.slice(bodyStart, sectionEnd(changelog, bodyStart)).trim();
}

function sectionEnd(changelog: string, from: number): number {
    const next = changelog.indexOf("\n## [", from);
    return next < 0 ? changelog.length : next;
}
