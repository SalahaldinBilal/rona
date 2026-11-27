import assert from "node:assert/strict";
import { test } from "node:test";

import { isVersion, setLockVersion, setPackageVersion } from "../lib/version.mts";

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

const LOCK = `[[package]]
name = "rona"
version = "0.1.0"
dependencies = [
 "rona-plugin",
]

[[package]]
name = "rona-plugin"
version = "0.1.0"

[[package]]
name = "ureq"
version = "3.5.2"
source = "registry+https://github.com/rust-lang/crates.io-index"
`;

test("only the app's entry in Cargo.lock changes", () => {
    const updated = setLockVersion(LOCK, "rona", "0.2.0") ?? "";
    assert.ok(updated.includes('name = "rona"\nversion = "0.2.0"\n'));
    assert.ok(updated.includes('name = "rona-plugin"\nversion = "0.1.0"\n'));
    assert.ok(updated.includes('version = "3.5.2"'));
});

test("a Cargo.lock with Windows line endings is still found", () => {
    const updated = setLockVersion(LOCK.replaceAll("\n", "\r\n"), "rona", "0.2.0") ?? "";
    assert.ok(updated.includes('name = "rona"\r\nversion = "0.2.0"\r\n'));
});

test("versions must be three numbers", () => {
    assert.ok(isVersion("0.2.0"));
    assert.ok(isVersion("1.10.3"));
    assert.ok(!isVersion("v0.2.0"));
    assert.ok(!isVersion("0.2"));
    assert.ok(!isVersion("0.2.0-beta"));
    assert.ok(!isVersion("0..1"));
});
