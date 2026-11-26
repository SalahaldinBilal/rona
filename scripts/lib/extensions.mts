import path from "node:path";

import { read, root, TaskError } from "./util.mts";

export interface Extensions {
    video: string[];
    audio: string[];
    playlist: string[];
    plugin: string[];
}

/** The app's extension lists, read from src/extensions.rs so the installer registers exactly what it plays. */
export function readExtensions(source = read(path.join(root, "src/extensions.rs"))): Extensions {
    return {
        video: list(source, "VIDEO_EXTENSIONS"),
        audio: list(source, "AUDIO_EXTENSIONS"),
        playlist: list(source, "PLAYLIST_EXTENSIONS"),
        plugin: list(source, "PLUGIN_EXTENSIONS"),
    };
}

function list(source: string, name: string): string[] {
    const match = new RegExp(`${name}: \\[&str; (\\d+)\\] = \\[([^\\]]*)\\]`).exec(source);
    if (!match) {
        throw new TaskError(`no ${name} array in src/extensions.rs`);
    }
    const items = [...match[2].matchAll(/"([^"]*)"/g)].map((item) => item[1]);
    if (items.length !== Number(match[1])) {
        throw new TaskError(`${name} in src/extensions.rs has ${items.length} entries but declares ${match[1]}`);
    }
    return items;
}
