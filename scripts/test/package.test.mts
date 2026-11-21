import assert from "node:assert/strict";
import { test } from "node:test";

import { readExtensions } from "../lib/extensions.mts";
import { playerRegistration } from "../lib/package.mts";

function sections(): { install: string; uninstall: string } {
    const text = playerRegistration(readExtensions());
    const [install, uninstall] = text.split("Section un.");
    assert.ok(uninstall !== undefined, "an uninstall section");
    return { install, uninstall };
}

test("the installer registers the extensions the app plays", () => {
    const { video, audio } = readExtensions();
    assert.ok(video.includes("mkv") && video.includes("divx"));
    assert.ok(audio.includes("mp3") && audio.includes("dts"));
    const { install } = sections();
    assert.ok(install.includes('"Software\\Classes\\Rona.flac" "" "FLAC audio"'));
});

test("a miscounted extension list fails instead of registering the wrong set", () => {
    const source = 'pub const VIDEO_EXTENSIONS: [&str; 3] = ["mkv", "mp4"];\npub const AUDIO_EXTENSIONS: [&str; 1] = ["mp3"];';
    assert.throws(() => readExtensions(source), /VIDEO_EXTENSIONS/);
});

// An unquoted exe path breaks for a user name with a space, and an unquoted %1 for any file with one.
test("open commands quote the exe and the file", () => {
    const { install } = sections();
    const commands = install.split("\n").filter((line) => line.includes("\\shell\\open\\command"));
    assert.ok(commands.length > 0);
    for (const command of commands) {
        assert.ok(command.endsWith('"$\\"$INSTDIR\\${MAINBINARYNAME}.exe$\\" $\\"%1$\\""'), command);
    }
});

test("uninstalling removes every key and value the install adds", () => {
    const { install, uninstall } = sections();
    const undoLines = uninstall.split("\n").map((line) => line.trimStart());
    for (const line of install.split("\n").filter((line) => line.trimStart().startsWith("WriteRegStr"))) {
        const fields = line.split('"');
        const [key, value] = [fields[1], fields[3]];
        const removed = undoLines.some(
            (undo) =>
                (undo.startsWith("DeleteRegKey") && key.startsWith(undo.split('"')[1] ?? "\u0000")) ||
                (undo.startsWith("DeleteRegValue") && undo.includes(`"${key}" "${value}"`)),
        );
        assert.ok(removed, `nothing removes ${line}`);
    }
});

// Deleting a shared key would take other apps' entries with it.
test("uninstalling never deletes an extension key", () => {
    const { uninstall } = sections();
    for (const line of uninstall.split("\n").filter((line) => line.includes("DeleteRegKey"))) {
        assert.ok(!line.includes("Software\\Classes\\."), line);
    }
});
