import assert from "node:assert/strict";
import { test } from "node:test";

import { tokenFromCredentials } from "../lib/publish-sdk.mts";

test("reads the token cargo login writes, with Windows line endings too", () => {
    assert.equal(tokenFromCredentials('[registry]\ntoken = "cio123"\n'), "cio123");
    assert.equal(tokenFromCredentials('[registry]\r\ntoken = "cio123"\r\n'), "cio123");
    assert.equal(tokenFromCredentials('[registries.crates-io]\ntoken = "cio456"\n'), "cio456");
});

test("a token for another registry is not taken for crates.io", () => {
    assert.equal(tokenFromCredentials('[registries.company]\ntoken = "other"\n'), null);
    assert.equal(tokenFromCredentials('[registries.company]\ntoken = "other"\n\n[registry]\ntoken = "cio"\n'), "cio");
    assert.equal(tokenFromCredentials(""), null);
});
