import path from "node:path";

import { deploy } from "./lib/deploy.mts";
import { generateKeyPair, PUBLIC_KEY_PATH } from "./lib/minisign.mts";
import { fetchMpv } from "./lib/mpv.mts";
import { packageApp } from "./lib/package.mts";
import { root, TaskError, write } from "./lib/util.mts";
import { bump } from "./lib/version.mts";

const USAGE = `Usage: node scripts/tasks.mts <command>

Commands:
  fetch-mpv          Download libmpv for Windows into vendor/mpv/windows-x86_64/
  bump <version>     Set the version in Cargo.toml and date the Unreleased section of CHANGELOG.md
  keygen <password>  Create the update signing key pair, encrypted with this password
  package            Build in release mode and package for this platform (NSIS on Windows, AppImage on Linux)
  deploy             Upload a release from bundles/ (run by the release workflow)`;

function keygen(password: string): void {
    if (password === "") {
        throw new TaskError("the password can't be empty");
    }
    const { publicKey, secretKey } = generateKeyPair(password);
    write(path.join(root, PUBLIC_KEY_PATH), publicKey);
    console.log(`Wrote the public key to ${PUBLIC_KEY_PATH}, commit it.\n`);
    console.log("Add these as repository secrets for the release workflow:");
    console.log("  RONA_SIGN_PASSWORD: the password you used");
    console.log("  RONA_SIGN_KEY: everything between the lines below\n");
    console.log(`-----\n${secretKey}-----`);
    console.log("\nKeep the secret key somewhere safe too. Without it, installed copies can't be updated.");
}

async function main(args: string[]): Promise<void> {
    const [command, ...rest] = args;
    if (command === "fetch-mpv" && rest.length === 0) {
        await fetchMpv();
    } else if (command === "bump" && rest.length === 1) {
        bump(rest[0]);
    } else if (command === "keygen" && rest.length === 1) {
        keygen(rest[0]);
    } else if (command === "package" && rest.length === 0) {
        packageApp();
    } else if (command === "deploy" && rest.length === 0) {
        deploy();
    } else {
        throw new TaskError(USAGE);
    }
}

try {
    await main(process.argv.slice(2));
} catch (err) {
    console.error(err instanceof TaskError ? err.message : err);
    process.exitCode = 1;
}
