import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { messageOf, root, run, TaskError, write } from "./util.mts";

const MPV_URL = "https://github.com/mpv-player/mpv/releases/download/v0.42.0/libmpv-v0.42.0-x86_64-w64-mingw32-gpl.zip";
// Pinned so a changed or tampered download fails the build instead of being bundled.
const MPV_SHA256 = "b99e0d183024ae479d1b36a93fa262c0297dfb025b99715b98da377ffb6c2913";

export async function fetchMpv(): Promise<void> {
    if (process.platform !== "win32") {
        throw new TaskError("fetch-mpv is for Windows, Linux builds use the system's libmpv");
    }
    console.log(`Downloading ${MPV_URL}`);
    const bytes = await download(MPV_URL);
    const hash = createHash("sha256").update(bytes).digest("hex");
    if (hash !== MPV_SHA256) {
        throw new TaskError(`the download's SHA-256 is ${hash}, expected ${MPV_SHA256}`);
    }

    const target = path.join(root, "vendor/mpv/windows-x86_64");
    mkdirSync(target, { recursive: true });
    const scratch = mkdtempSync(path.join(tmpdir(), "rona-mpv-"));
    try {
        const archive = path.join(scratch, "libmpv.zip");
        write(archive, bytes);
        // Windows' own tar reads zip files. Git's GNU tar, which can come first on PATH, doesn't.
        const tar = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe");
        run(tar, ["-xf", archive, "-C", target]);
    } finally {
        rmSync(scratch, { recursive: true, force: true });
    }
    if (!existsSync(path.join(target, "libmpv-2.dll"))) {
        throw new TaskError(`the archive has no libmpv-2.dll at its top level, check ${target}`);
    }
    console.log(`Extracted to ${target}`);
}

async function download(url: string): Promise<Buffer> {
    let response: Response;
    try {
        response = await fetch(url);
    } catch (err) {
        throw new TaskError(`could not download ${url}: ${messageOf(err)}`);
    }
    if (!response.ok) {
        throw new TaskError(`could not download ${url}: ${response.status} ${response.statusText}`);
    }
    return Buffer.from(await response.arrayBuffer());
}
