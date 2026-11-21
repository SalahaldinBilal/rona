import assert from "node:assert/strict";
import { test } from "node:test";

import { isVersion, setPackageVersion } from "../lib/version.mts";

const CARGO = `[package]
name = "rona"
version = "0.1.0"
edition = "2024"

[dependencies]
glow = "0.18"
ureq = { version = "3.5.2", default-features = false }
`;

test("only the package version changes", () => {
    const updated = setPackageVersion(CARGO, "0.2.0") ?? "";
    assert.ok(updated.includes('\nversion = "0.2.0"\n'));
    assert.ok(updated.includes('ureq = { version = "3.5.2", default-features = false }'));
    assert.ok(!updated.includes("0.1.0"));
    assert.ok(updated.endsWith("\n"));
});

test("a Cargo.toml checked out with Windows line endings keeps them", () => {
    const updated = setPackageVersion(CARGO.replaceAll("\n", "\r\n"), "0.2.0") ?? "";
    assert.ok(updated.includes('\r\nversion = "0.2.0"\r\n'));
});

test("versions must be three numbers", () => {
    assert.ok(isVersion("0.2.0"));
    assert.ok(isVersion("1.10.3"));
    assert.ok(!isVersion("v0.2.0"));
    assert.ok(!isVersion("0.2"));
    assert.ok(!isVersion("0.2.0-beta"));
    assert.ok(!isVersion("0..1"));
});
