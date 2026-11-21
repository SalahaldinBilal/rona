// minisign keys and signatures, in the format the app's minisign-verify reads. Signatures are always the
// prehashed kind ("ED"): Ed25519 over the file's BLAKE2b-512, then a global signature over that signature and
// the trusted comment. Secret keys are encrypted by XOR with scrypt output, as libsodium's
// crypto_pwhash_scryptsalsa208sha256 makes it from the opslimit and memlimit stored in the key.

import { createHash, createPrivateKey, createPublicKey, randomBytes, scryptSync, sign, verify } from "node:crypto";
import type { KeyObject } from "node:crypto";

import { blake2b } from "./blake2b.mts";
import { TaskError } from "./util.mts";

const KEY_ALGORITHM = Buffer.from("Ed");
const PREHASHED_ALGORITHM = Buffer.from("ED");
const SCRYPT = Buffer.from("Sc");
const NO_KDF = Buffer.from([0, 0]);
const CHECKSUM_ALGORITHM = Buffer.from("B2");
const KEY_ID_BYTES = 8;
const SECRET_BYTES = 64;
const CHECKSUM_BYTES = 32;
const KEYNUM_BYTES = KEY_ID_BYTES + SECRET_BYTES + CHECKSUM_BYTES;
const SALT_BYTES = 32;
const SECRET_KEY_BYTES = 6 + SALT_BYTES + 8 + 8 + KEYNUM_BYTES;
const PKCS8_ED25519_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");

/** The key the app compiles in and checks updates against. */
export const PUBLIC_KEY_PATH = "assets/update-key.pub";

/** What minisign itself uses for new keys. */
export const SENSITIVE_LIMITS = { opslimit: 33554432, memlimit: 1073741824 };

export interface ScryptParameters {
    N: number;
    r: number;
    p: number;
}

/** libsodium's pickparams, which turns minisign's opslimit and memlimit into scrypt's N, r and p. */
export function scryptParameters(opslimit: number, memlimit: number): ScryptParameters {
    const ops = Math.max(opslimit, 32768);
    const r = 8;
    const smallestAbove = (maxN: number) => {
        let log2 = 1;
        while (log2 < 63 && 2 ** log2 <= Math.floor(maxN / 2)) {
            log2++;
        }
        return log2;
    };
    if (ops < Math.floor(memlimit / 32)) {
        return { N: 2 ** smallestAbove(Math.floor(ops / (r * 4))), r, p: 1 };
    }
    const log2 = smallestAbove(Math.floor(memlimit / (r * 128)));
    const maxrp = Math.min(Math.floor(Math.floor(ops / 4) / 2 ** log2), 0x3fffffff);
    return { N: 2 ** log2, r, p: Math.floor(maxrp / r) };
}

function keyStream(password: string, salt: Uint8Array, opslimit: number, memlimit: number): Buffer {
    const { N, r, p } = scryptParameters(opslimit, memlimit);
    return scryptSync(password, salt, KEYNUM_BYTES, { N, r, p, maxmem: 256 * N * r * p + 32 * 1024 * 1024 });
}

function xor(data: Buffer, stream: Buffer): Buffer {
    return Buffer.from(data.map((byte, index) => byte ^ stream[index]));
}

function checksum(keyId: Buffer, secret: Buffer): Buffer {
    return blake2b(Buffer.concat([KEY_ALGORITHM, keyId, secret]), CHECKSUM_BYTES);
}

function privateKeyFromSeed(seed: Buffer): KeyObject {
    return createPrivateKey({ key: Buffer.concat([PKCS8_ED25519_PREFIX, seed]), format: "der", type: "pkcs8" });
}

function rawPublicKey(privateKey: KeyObject): Buffer {
    return createPublicKey(privateKey).export({ format: "der", type: "spki" }).subarray(-32);
}

function publicKeyObject(raw: Buffer): KeyObject {
    const spkiPrefix = Buffer.from("302a300506032b6570032100", "hex");
    return createPublicKey({ key: Buffer.concat([spkiPrefix, raw]), format: "der", type: "spki" });
}

// minisign prints key ids as a little-endian number.
function keyIdText(keyId: Buffer): string {
    return Buffer.from(keyId).reverse().toString("hex").toUpperCase();
}

// The base64 line of a minisign box, after its "untrusted comment:" line.
function boxPayload(text: string, what: string): Buffer {
    const line = text
        .split(/\r?\n/)
        .map((line) => line.trim())
        .find((line) => line !== "" && !line.startsWith("untrusted comment:"));
    if (!line) {
        throw new TaskError(`the ${what} is empty`);
    }
    return Buffer.from(line, "base64");
}

function u64(value: number): Buffer {
    const bytes = Buffer.alloc(8);
    bytes.writeBigUInt64LE(BigInt(value));
    return bytes;
}

export interface KeyPair {
    publicKey: string;
    secretKey: string;
}

/** A new key pair in minisign's text format, the secret key encrypted with `password`. */
export function generateKeyPair(password: string, limits = SENSITIVE_LIMITS): KeyPair {
    const seed = randomBytes(32);
    const publicRaw = rawPublicKey(privateKeyFromSeed(seed));
    const keyId = randomBytes(KEY_ID_BYTES);
    const secret = Buffer.concat([seed, publicRaw]);
    const keynum = Buffer.concat([keyId, secret, checksum(keyId, secret)]);
    const salt = randomBytes(SALT_BYTES);
    const encrypted = xor(keynum, keyStream(password, salt, limits.opslimit, limits.memlimit));
    const secretBox = Buffer.concat([
        KEY_ALGORITHM,
        SCRYPT,
        CHECKSUM_ALGORITHM,
        salt,
        u64(limits.opslimit),
        u64(limits.memlimit),
        encrypted,
    ]);
    return {
        publicKey: `untrusted comment: minisign public key: ${keyIdText(keyId)}\n${Buffer.concat([KEY_ALGORITHM, keyId, publicRaw]).toString("base64")}\n`,
        secretKey: `untrusted comment: minisign encrypted secret key\n${secretBox.toString("base64")}\n`,
    };
}

interface SecretKey {
    keyId: Buffer;
    privateKey: KeyObject;
}

function readSecretKey(text: string, password: string): SecretKey {
    const bytes = boxPayload(text, "signing key");
    if (bytes.length !== SECRET_KEY_BYTES || !bytes.subarray(0, 2).equals(KEY_ALGORITHM)) {
        throw new TaskError("the signing key isn't a minisign secret key");
    }
    const kdf = bytes.subarray(2, 4);
    const salt = bytes.subarray(6, 6 + SALT_BYTES);
    const opslimit = Number(bytes.readBigUInt64LE(38));
    const memlimit = Number(bytes.readBigUInt64LE(46));
    let keynum = bytes.subarray(54);
    if (kdf.equals(SCRYPT)) {
        keynum = xor(keynum, keyStream(password, salt, opslimit, memlimit));
    } else if (!kdf.equals(NO_KDF)) {
        throw new TaskError(`the signing key uses an unknown key derivation "${kdf.toString()}"`);
    }
    const keyId = keynum.subarray(0, KEY_ID_BYTES);
    const secret = keynum.subarray(KEY_ID_BYTES, KEY_ID_BYTES + SECRET_BYTES);
    if (!checksum(keyId, secret).equals(keynum.subarray(KEY_ID_BYTES + SECRET_BYTES))) {
        throw new TaskError("could not read the signing key: wrong password");
    }
    return { keyId, privateKey: privateKeyFromSeed(secret.subarray(0, 32)) };
}

/** A minisign signature of `data`, in the text format the app verifies. */
export function signData(data: Uint8Array, secretKey: string, password: string): string {
    const { keyId, privateKey } = readSecretKey(secretKey, password);
    const signature = sign(null, createHash("blake2b512").update(data).digest(), privateKey);
    const trusted = `timestamp:${Math.floor(Date.now() / 1000)}`;
    const globalSignature = sign(null, Buffer.concat([signature, Buffer.from(trusted)]), privateKey);
    const signatureLine = Buffer.concat([PREHASHED_ALGORITHM, keyId, signature]).toString("base64");
    return `untrusted comment: signature from minisign secret key\n${signatureLine}\ntrusted comment: ${trusted}\n${globalSignature.toString("base64")}\n`;
}

/** Checks a signature the way minisign-verify does, so a bad one is caught before it's published. */
export function verifySignature(data: Uint8Array, signature: string, publicKey: string): boolean {
    const key = boxPayload(publicKey, "public key");
    const lines = signature.split(/\r?\n/);
    const signatureBytes = Buffer.from(lines[1] ?? "", "base64");
    const trusted = (lines[2] ?? "").replace(/^trusted comment: /, "");
    const globalSignature = Buffer.from(lines[3] ?? "", "base64");
    if (key.length !== 42 || signatureBytes.length !== 74 || !signatureBytes.subarray(0, 2).equals(PREHASHED_ALGORITHM)) {
        return false;
    }
    if (!signatureBytes.subarray(2, 10).equals(key.subarray(2, 10))) {
        return false;
    }
    const signer = publicKeyObject(key.subarray(10));
    const raw = signatureBytes.subarray(10);
    return (
        verify(null, createHash("blake2b512").update(data).digest(), signer, raw) &&
        verify(null, Buffer.concat([raw, Buffer.from(trusted)]), signer, globalSignature)
    );
}
