// Publishes both plugin SDKs: the rona-plugin crate and the @rona-player/plugin npm package.
//
// Order: check both logins, build and verify both, and only then publish. Nothing is uploaded unless every step
// before it passed for both. A version that's already published is skipped, so it can run again after one of the
// two uploads failed.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";

import { read, root, TaskError } from "./util.mts";

const NPM_PUBLISH_ROLES = ["developer", "admin", "owner"];
const CRATES_IO_USER = "SalahaldinBilal";
const CRATES_IO = "https://crates.io/api/v1";
const USER_AGENT = "rona-sdk-publish-script";

const rustDir = path.join(root, "plugin-sdk/rust");
const tsDir = path.join(root, "plugin-sdk/ts");

type Captured = { ok: boolean, stdout: string, stderr: string };

function step(title: string): void {
    console.log(`\n== ${title}`);
}

// npm is a .cmd on Windows, which Node only starts through a shell. Its arguments here are fixed text.
function spawnArgs(program: string, args: string[]): [string, string[], boolean] {
    if (program === "npm" && process.platform === "win32") {
        return [`npm ${args.join(" ")}`, [], true];
    }
    return [program, args, false];
}

function capture(program: string, args: string[], cwd: string): Captured {
    const [command, commandArgs, shell] = spawnArgs(program, args);
    const result = spawnSync(command, commandArgs, { cwd, shell, encoding: "utf-8" });
    if (result.error) {
        throw new TaskError(`could not run ${program}: ${result.error.message}`);
    }
    return { ok: result.status === 0, stdout: result.stdout.trim(), stderr: result.stderr.trim() };
}

function run(program: string, args: string[], cwd: string): void {
    console.log(`> ${program} ${args.join(" ")}`);
    const [command, commandArgs, shell] = spawnArgs(program, args);
    const result = spawnSync(command, commandArgs, { cwd, shell, stdio: "inherit" });
    if (result.error) {
        throw new TaskError(`could not run ${program}: ${result.error.message}`);
    }
    if (result.status !== 0) {
        throw new TaskError(`${program} ${args[0]} failed. Nothing after this step was run.`);
    }
}

function parseJson<T>(text: string, what: string): T {
    try {
        return JSON.parse(text) as T;
    } catch {
        throw new TaskError(`npm gave an unexpected answer for ${what}: ${text}`);
    }
}

function checkNpmLogin(scope: string): void {
    const who = capture("npm", ["whoami"], tsDir);
    if (!who.ok || !who.stdout) {
        throw new TaskError("npm is not logged in. Run: npm login");
    }
    const user = who.stdout;

    const members = capture("npm", ["org", "ls", scope, user, "--json"], tsDir);
    if (!members.ok) {
        throw new TaskError(`npm user ${user} can't read the ${scope} org, so it isn't a member of it.\n${members.stderr}`);
    }
    const role = parseJson<Record<string, string>>(members.stdout, `the ${scope} org members`)[user];
    if (!role || !NPM_PUBLISH_ROLES.includes(role)) {
        throw new TaskError(`npm user ${user} can't publish in the ${scope} org (role: ${role ?? "none"})`);
    }
    console.log(`npm: logged in as ${user}, ${role} of the ${scope} org`);
    checkNpmTwoFactor(user);
}

// npm refuses to publish without two-factor auth, and only says so at the upload.
function checkNpmTwoFactor(user: string): void {
    const profile = capture("npm", ["profile", "get", "--json"], tsDir);
    // An access token can't read the profile. Such a token only publishes if it was made to bypass 2FA.
    if (!profile.ok) {
        console.log("npm: logged in with an access token, so two-factor auth couldn't be checked");
        return;
    }
    const tfa = parseJson<{ tfa?: unknown }>(profile.stdout, `the profile of ${user}`).tfa;
    const enabled = typeof tfa === "object" && tfa !== null && (tfa as { pending?: boolean }).pending !== true;
    if (!enabled) {
        throw new TaskError(
            `npm user ${user} has no two-factor auth, and npm won't publish without it. Turn it on at `
            + "https://www.npmjs.com/settings/~/profile (or run: npm profile enable-2fa auth-and-writes), then run this again.",
        );
    }
    console.log("npm: two-factor auth is on. npm asks for the code when it publishes.");
}

/** The crates.io token in a cargo credentials file, from `[registry]` or `[registries.crates-io]`. */
export function tokenFromCredentials(text: string): string | null {
    let inRegistry = false;
    for (const line of text.split(/\r?\n/)) {
        const section = /^\s*\[(.+)\]\s*$/.exec(line);
        if (section) {
            inRegistry = section[1] === "registry" || section[1] === "registries.crates-io";
            continue;
        }
        const token = inRegistry ? /^\s*token\s*=\s*"([^"]+)"/.exec(line) : null;
        if (token) {
            return token[1];
        }
    }
    return null;
}

function hasCargoToken(): boolean {
    if (process.env.CARGO_REGISTRY_TOKEN) {
        return true;
    }
    const cargoHome = process.env.CARGO_HOME ?? path.join(homedir(), ".cargo");
    return ["credentials.toml", "credentials"].some(name => {
        const file = path.join(cargoHome, name);
        return existsSync(file) && tokenFromCredentials(read(file)) !== null;
    });
}

async function cratesIo(route: string): Promise<Response> {
    return fetch(`${CRATES_IO}${route}`, { headers: { "User-Agent": USER_AGENT } });
}

// crates.io won't say whose a token is, so the account is checked through the crate's owners.
async function checkCargoLogin(crate: string): Promise<void> {
    if (!hasCargoToken()) {
        throw new TaskError("cargo has no crates.io token in CARGO_REGISTRY_TOKEN or ~/.cargo/credentials.toml. Run: cargo login");
    }
    const response = await cratesIo(`/crates/${crate}/owners`);
    if (response.status === 404) {
        await response.arrayBuffer();
        console.log(`cargo: a token is set. ${crate} isn't on crates.io yet, so whoever publishes it first owns it.`);
        return;
    }
    if (!response.ok) {
        throw new TaskError(`crates.io answered ${response.status} when asked who owns ${crate}`);
    }
    const body = await response.json() as { users?: Array<{ login?: string }> };
    const owners = (body.users ?? []).map(user => user.login ?? "");
    if (!owners.some(login => login.toLowerCase() === CRATES_IO_USER.toLowerCase())) {
        throw new TaskError(`${crate} on crates.io belongs to ${owners.join(", ") || "nobody"}, not to ${CRATES_IO_USER}`);
    }
    console.log(`cargo: a token is set, and ${crate} belongs to ${CRATES_IO_USER}`);
}

function cargoPackageField(field: string): string {
    const manifest = read(path.join(rustDir, "Cargo.toml"));
    const value = new RegExp(`^\\[package\\][\\s\\S]*?^${field}\\s*=\\s*"([^"]+)"`, "m").exec(manifest);
    if (!value) {
        throw new TaskError(`plugin-sdk/rust/Cargo.toml has no package ${field}`);
    }
    return value[1];
}

async function crateIsPublished(name: string, version: string): Promise<boolean> {
    const response = await cratesIo(`/crates/${name}/${version}`);
    await response.arrayBuffer();
    if (response.status === 404) {
        return false;
    }
    if (!response.ok) {
        throw new TaskError(`crates.io answered ${response.status} when asked about ${name} ${version}`);
    }
    return true;
}

function npmIsPublished(name: string, version: string): boolean {
    const view = capture("npm", ["view", `${name}@${version}`, "version"], tsDir);
    if (view.ok) {
        return view.stdout === version;
    }
    if (/E404|404 Not Found/.test(view.stderr)) {
        return false;
    }
    throw new TaskError(`npm couldn't say whether ${name}@${version} is published:\n${view.stderr}`);
}

export async function publishSdk(dryRun: boolean): Promise<void> {
    const crate = { name: cargoPackageField("name"), version: cargoPackageField("version") };
    const npmPackage = JSON.parse(read(path.join(tsDir, "package.json"))) as { name: string, version: string };
    const scope = /^@([^/]+)\//.exec(npmPackage.name)?.[1];
    if (!scope) {
        throw new TaskError(`${npmPackage.name} has no @scope, so there is no org to check`);
    }

    step("Checking logins");
    checkNpmLogin(scope);
    await checkCargoLogin(crate.name);

    step("Checking versions");
    const publishCrate = !await crateIsPublished(crate.name, crate.version);
    const publishNpm = !npmIsPublished(npmPackage.name, npmPackage.version);
    console.log(`${crate.name} ${crate.version}: ${publishCrate ? "to publish" : "already on crates.io, skipped"}`);
    console.log(`${npmPackage.name} ${npmPackage.version}: ${publishNpm ? "to publish" : "already on npm, skipped"}`);
    if (!publishCrate && !publishNpm) {
        console.log("\nBoth versions are already published. Raise the versions to publish again.");
        return;
    }

    step("Building and checking the Rust SDK");
    run("cargo", ["test"], rustDir);
    run("cargo", ["check", "--target", "wasm32-unknown-unknown", "--no-default-features"], rustDir);
    // Fails on uncommitted changes, so what gets published is what's in git.
    run("cargo", ["package"], rustDir);

    step("Building and checking the TypeScript SDK");
    run("npm", ["install", "--no-audit", "--no-fund"], tsDir);
    run("npm", ["test"], tsDir);
    run("npm", ["run", "build"], tsDir);
    run("npm", ["pack", "--dry-run"], tsDir);

    if (dryRun) {
        step("Dry run");
        if (publishCrate) {
            run("cargo", ["publish", "--dry-run"], rustDir);
        }
        if (publishNpm) {
            run("npm", ["publish", "--dry-run"], tsDir);
        }
        console.log("\nEverything passed. Nothing was published (--dry-run).");
        return;
    }

    step("Publishing");
    if (publishCrate) {
        run("cargo", ["publish"], rustDir);
    }
    // No --otp: a code would run out during the build, so npm asks for it here.
    if (publishNpm) {
        run("npm", ["publish", "--access", "public"], tsDir);
    }
    console.log("\nDone.");
}
