import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { test } from "node:test";

import { blake2b } from "../lib/blake2b.mts";
import { generateKeyPair, scryptParameters, signData, verifySignature } from "../lib/minisign.mts";

// Small limits keep the tests fast. The key stores them, so signing reads them back from it.
const QUICK = { opslimit: 1048576, memlimit: 16777216 };

test("BLAKE2b matches Node's at every block boundary", () => {
    for (const length of [0, 1, 74, 127, 128, 129, 256, 300]) {
        const data = randomBytes(length);
        assert.deepEqual(blake2b(data, 64), createHash("blake2b512").update(data).digest(), `${length} bytes`);
    }
});

test("BLAKE2b-256 matches the RFC 7693 test vector", () => {
    assert.equal(
        blake2b(Buffer.from("abc"), 32).toString("hex"),
        "bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d52319",
    );
});

// libsodium's own choices for minisign's default limits, so keys made by minisign decrypt here.
test("scrypt parameters follow libsodium", () => {
    assert.deepEqual(scryptParameters(33554432, 1073741824), { N: 1048576, r: 8, p: 1 });
    assert.deepEqual(scryptParameters(524288, 16777216), { N: 16384, r: 8, p: 1 });
});

test("a signature verifies with its public key and fails for changed data", () => {
    const { publicKey, secretKey } = generateKeyPair("test password", QUICK);
    const data = Buffer.from("installer bytes");
    const signature = signData(data, secretKey, "test password");
    assert.ok(verifySignature(data, signature, publicKey));

    const tampered = Buffer.from(data);
    tampered[0] ^= 1;
    assert.ok(!verifySignature(tampered, signature, publicKey));
});

test("a signature from another key fails", () => {
    const signer = generateKeyPair("one", QUICK);
    const other = generateKeyPair("two", QUICK);
    const data = Buffer.from("installer bytes");
    assert.ok(!verifySignature(data, signData(data, signer.secretKey, "one"), other.publicKey));
});

// Made by the Rust minisign crate, which also made the release key, with the password "fixture password".
const RUST_PUBLIC_KEY = `untrusted comment: minisign public key: DE343C9638D875F6
RWT2ddg4ljw03iURz1A/kNXu5XlG3EZK36xINMms1Bjckdc9XYH+Yqda
`;
const RUST_SECRET_KEY = `untrusted comment: rsign encrypted secret key
RWRTY0Iysdz1Gjqf/H1w7SUPd4NxSMJUY2mDiPJEj0cDnYjiN+UAABAAAAAAAAAAAAIAAAAAxbMJckd+2BFrmpYtX4ICFwqnmf6e7OWgdir9yx8F7vwsB2HZlwX5GnWK/LWMGwbLUOHmyAUxBQ4s+Ocbm2GitQtH/CNKDgl/+7e07PwIhFGdD9TqsBTn5X0V0wm08gyb3+IYWaRGdTQ=
`;

test("a secret key made by the Rust minisign crate signs", () => {
    const data = Buffer.from("installer bytes");
    const signature = signData(data, RUST_SECRET_KEY, "fixture password");
    assert.ok(verifySignature(data, signature, RUST_PUBLIC_KEY));
    assert.throws(() => signData(data, RUST_SECRET_KEY, "wrong"), /wrong password/);
});

test("a wrong password is refused", () => {
    const { secretKey } = generateKeyPair("right", QUICK);
    assert.throws(() => signData(Buffer.from("data"), secretKey, "wrong"), /wrong password/);
});
