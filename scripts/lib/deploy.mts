import { copyFileSync, mkdirSync, readdirSync, readFileSync, rmSync } from "node:fs";
import path from "node:path";

import { releaseNotes } from "./changelog.mts";
import { PUBLIC_KEY_PATH, signData, verifySignature } from "./minisign.mts";
import { read, requireEnv, root, run, TaskError, write } from "./util.mts";
import { packageVersion } from "./version.mts";

export const BUCKET = "rona-files";
export const DOWNLOAD_DOMAIN = "rona.won.fyi";
const MANIFEST_PATH = "updater/latest.json";

interface Artifact {
    label: string;
    // Folder under bundles/ that the workflow downloads the build artifact into.
    bundle: string;
    suffix: string;
    publishedSuffix: string;
    // Key in latest.json, `<os>-<arch>` as the app builds it.
    platform: string;
}

const ARTIFACTS: Artifact[] = [
    {
        label: "Windows",
        bundle: "bundle-windows",
        suffix: "-setup.exe",
        publishedSuffix: "x64-setup.exe",
        platform: "windows-x86_64",
    },
    {
        label: "Linux (AppImage)",
        bundle: "bundle-linux",
        suffix: ".AppImage",
        publishedSuffix: "amd64.AppImage",
        platform: "linux-x86_64",
    },
];

interface Staged {
    artifact: Artifact;
    file: string;
    remotePath: string;
    signature: string;
}

export function deploy(): void {
    const version = packageVersion();
    const tag = `v${version}`;
    const notes = releaseNotes(read(path.join(root, "CHANGELOG.md")), version);
    const b2KeyId = requireEnv("B2_APPLICATION_KEY_ID");
    const b2Key = requireEnv("B2_APPLICATION_KEY");
    const signKey = requireEnv("RONA_SIGN_KEY");
    const signPassword = requireEnv("RONA_SIGN_PASSWORD");
    const publicKey = read(path.join(root, PUBLIC_KEY_PATH));

    const staging = path.join(root, "release-files");
    rmSync(staging, { recursive: true, force: true });
    mkdirSync(staging, { recursive: true });

    const staged: Staged[] = [];
    for (const artifact of ARTIFACTS) {
        const built = findBuiltFile(path.join(root, "bundles", artifact.bundle), artifact.suffix);
        const name = `rona_${version}_${artifact.publishedSuffix}`;
        const file = path.join(staging, name);
        copyFileSync(built, file);
        const bytes = readFileSync(file);
        const signature = signData(bytes, signKey, signPassword);
        // The app only accepts signatures from the key compiled into it, so a different key fails here, not there.
        if (!verifySignature(bytes, signature, publicKey)) {
            throw new TaskError(`the signature of ${name} doesn't verify with ${PUBLIC_KEY_PATH}`);
        }
        write(`${file}.minisig`, signature);
        staged.push({ artifact, file, remotePath: `installer/${version}/${name}`, signature });
    }

    run("b2", ["account", "authorize", b2KeyId, b2Key]);
    for (const file of staged) {
        upload(file.file, file.remotePath);
        upload(`${file.file}.minisig`, `${file.remotePath}.minisig`);
    }

    const downloads = staged.map((file) => `- ${file.artifact.label}: ${downloadUrl(file.remotePath)}`);
    const notesWithDownloads = `${notes}\n\n### Downloads\n\n${downloads.join("\n")}`;
    const attachments = staged.flatMap((file) => [file.file, `${file.file}.minisig`]);
    run("gh", ["release", "create", tag, "--title", tag, "--notes", notesWithDownloads, ...attachments]);

    for (const file of staged) {
        upload(file.file, `rona_latest_${file.artifact.publishedSuffix}`);
    }

    // Written last, so a client never sees a manifest that points at files that aren't uploaded yet.
    const manifestPath = path.join(staging, "latest.json");
    write(manifestPath, manifest(tag, notes, new Date().toISOString(), staged));
    upload(manifestPath, MANIFEST_PATH);

    console.log(`\nDeployed ${tag}.`);
}

function manifest(tag: string, notes: string, date: string, staged: Staged[]): string {
    const platforms = Object.fromEntries(
        staged.map((file) => [file.artifact.platform, { url: downloadUrl(file.remotePath), signature: file.signature }]),
    );
    return JSON.stringify({ version: tag, notes, pub_date: date, platforms }, null, 2);
}

function downloadUrl(remotePath: string): string {
    return `https://${DOWNLOAD_DOMAIN}/${remotePath}`;
}

function upload(local: string, remote: string): void {
    console.log(`Uploading ${local} -> b2://${BUCKET}/${remote}`);
    run("b2", ["file", "upload", BUCKET, local, remote]);
}

// The packagers name files after the product and version, so they are found by suffix anywhere in the bundle.
function findBuiltFile(dir: string, suffix: string): string {
    let entries: string[];
    try {
        entries = readdirSync(dir, { recursive: true, encoding: "utf8" });
    } catch {
        entries = [];
    }
    const matches = entries.filter((entry) => entry.endsWith(suffix)).map((entry) => path.join(dir, entry));
    if (matches.length === 0) {
        throw new TaskError(`no *${suffix} file in ${dir}`);
    }
    if (matches.length > 1) {
        throw new TaskError(`more than one *${suffix} file in ${dir}`);
    }
    return matches[0];
}
