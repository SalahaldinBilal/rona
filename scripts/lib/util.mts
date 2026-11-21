import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

/** A failure with a message meant for the person running the task, printed without a stack trace. */
export class TaskError extends Error {}

export function requireEnv(name: string): string {
    const value = process.env[name];
    if (!value) {
        throw new TaskError(`${name} is not set`);
    }
    return value;
}

export function read(file: string): string {
    try {
        return readFileSync(file, "utf8");
    } catch (err) {
        throw new TaskError(`could not read ${file}: ${messageOf(err)}`);
    }
}

export function write(file: string, contents: string | Uint8Array): void {
    try {
        writeFileSync(file, contents);
    } catch (err) {
        throw new TaskError(`could not write ${file}: ${messageOf(err)}`);
    }
}

// No shell, so arguments such as multi-line release notes reach the program as they are.
export function run(program: string, args: string[], cwd: string = root): void {
    const result = spawnSync(program, args, { cwd, stdio: "inherit" });
    if (result.error) {
        throw new TaskError(`could not run ${program}: ${result.error.message}`);
    }
    if (result.status !== 0) {
        throw new TaskError(`${program} ${args.slice(0, 2).join(" ")} failed with exit code ${result.status}`);
    }
}

export function messageOf(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
}
