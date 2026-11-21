import path from "node:path";

import { cutRelease } from "./changelog.mts";
import { read, root, TaskError, write } from "./util.mts";

const PACKAGE_VERSION = /^version = "([^"]*)"$/m;

export function bump(version: string): void {
    if (!isVersion(version)) {
        throw new TaskError("Usage: node scripts/tasks.mts bump <version>  (for example 0.2.0)");
    }
    const cargoPath = path.join(root, "Cargo.toml");
    const updated = setPackageVersion(read(cargoPath), version);
    if (updated === undefined) {
        throw new TaskError('no `version = "..."` line in Cargo.toml');
    }
    write(cargoPath, updated);
    console.log("Updated Cargo.toml");

    const changelogPath = path.join(root, "CHANGELOG.md");
    const { text, empty } = cutRelease(read(changelogPath), version, today());
    write(changelogPath, text);
    if (empty) {
        console.error("Warning: the Unreleased section of CHANGELOG.md is empty, so this release has no notes.");
    }
    console.log(`Updated CHANGELOG.md\n\nBumped to ${version}. Cargo.lock follows on the next build.`);
}

export function packageVersion(): string {
    const match = PACKAGE_VERSION.exec(read(path.join(root, "Cargo.toml")));
    if (!match) {
        throw new TaskError('no `version = "..."` line in Cargo.toml');
    }
    return match[1];
}

export function isVersion(text: string): boolean {
    return /^\d+\.\d+\.\d+$/.test(text);
}

// Only a line that starts with `version =` is the package's, dependency versions sit inside `{ ... }`.
export function setPackageVersion(cargo: string, version: string): string | undefined {
    if (!PACKAGE_VERSION.test(cargo)) {
        return undefined;
    }
    return cargo.replace(PACKAGE_VERSION, `version = "${version}"`);
}

function today(): string {
    const now = new Date();
    const pad = (value: number) => String(value).padStart(2, "0");
    return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}
