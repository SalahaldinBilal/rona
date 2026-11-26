import path from "node:path";

import { readExtensions } from "./extensions.mts";
import type { Extensions } from "./extensions.mts";
import { root, run, write } from "./util.mts";
import { packageVersion } from "./version.mts";

export function packageApp(): void {
    const version = packageVersion();
    run("cargo", ["build", "--release"]);
    const configPath = path.join(root, "target/packager.toml");
    write(configPath, config(slashPath(root), version, process.platform));
    run("cargo", ["packager", "--release", "--config", configPath]);
    console.log(`\nPackaged Rona ${version} into target/packager/`);
}

// TOML strings treat backslashes as escapes, and every tool here accepts forward slashes on Windows.
function slashPath(file: string): string {
    return file.replaceAll("\\", "/");
}

export function config(root: string, version: string, platform: NodeJS.Platform): string {
    const common = `name = "rona"
product-name = "Rona"
version = "${version}"
identifier = "fyi.won.rona"
description = "Video player"
out-dir = "${root}/target/packager"
binaries-dir = "${root}/target/release"
`;
    return common + (platform === "win32" ? windowsConfig(root) : linuxConfig(root));
}

// build.rs puts libmpv-2.dll and rona.ico next to the release exe.
function windowsConfig(root: string): string {
    return `formats = ["nsis"]
icons = ["${root}/target/release/rona.ico"]
resources = ["${root}/target/release/libmpv-2.dll"]

[[binaries]]
path = "rona"
main = true

[nsis]
installer-mode = "currentUser"
installer-icon = "${root}/target/release/rona.ico"
preinstall-section = '''
${playerRegistration(readExtensions())}'''
`;
}

// build.rs writes the hicolor icons under target/release/share. libmpv comes from the build machine's packages.
function linuxConfig(root: string): string {
    return `formats = ["appimage"]
icons = ["${root}/target/release/share/icons/hicolor/*/apps/rona.png"]

[[binaries]]
path = "rona"
main = true

[appimage]
libs = ["libmpv.so.2"]
`;
}

// NSIS variables, kept in plain strings so a template literal doesn't read them as its own placeholders.
const MAIN_BINARY = "${MAINBINARYNAME}";
const PRODUCT = "${PRODUCTNAME}";

// cargo-packager's own file-associations take over each extension's default, leave the exe path in the open
// command unquoted and never remove anything on uninstall, so the registration is written here instead.
// SHCTX is HKCU in a per-user install. A `un.` section runs in the uninstaller, before cargo-packager's own.
export function playerRegistration(extensions: Extensions): string {
    const exe = `$\\"$INSTDIR\\${MAIN_BINARY}.exe$\\"`;
    const application = `Software\\Classes\\Applications\\${MAIN_BINARY}.exe`;
    const capabilities = "Software\\Clients\\Media\\Rona\\Capabilities";
    const icon = `${exe},0`;
    const command = `${exe} $\\"%1$\\"`;
    const kinds = [
        ...extensions.video.map((extension) => [extension, "video"]),
        ...extensions.audio.map((extension) => [extension, "audio"]),
        ...extensions.playlist.map((extension) => [extension, "playlist"]),
        ...extensions.plugin.map((extension) => [extension, "plugin"]),
    ];

    const install = [
        `WriteRegStr SHCTX "${application}" "FriendlyAppName" "${PRODUCT}"`,
        `WriteRegStr SHCTX "${application}\\DefaultIcon" "" "${icon}"`,
        `WriteRegStr SHCTX "${application}\\shell\\open\\command" "" "${command}"`,
        `WriteRegStr SHCTX "${capabilities}" "ApplicationName" "${PRODUCT}"`,
        `WriteRegStr SHCTX "${capabilities}" "ApplicationDescription" "Video and audio player"`,
        `WriteRegStr SHCTX "${capabilities}" "ApplicationIcon" "${icon}"`,
        `WriteRegStr SHCTX "Software\\RegisteredApplications" "${PRODUCT}" "${capabilities}"`,
    ];
    const uninstall: string[] = [];
    // Rona's own playlist and plugin formats have no other app to take them from, so Rona is their default. The
    // uninstaller only clears that default while it still points at Rona.
    for (const extension of [...extensions.playlist, ...extensions.plugin]) {
        const key = `Software\\Classes\\.${extension}`;
        const label = `keep_default_${extension}`;
        install.push(`WriteRegStr SHCTX "${key}" "" "Rona.${extension}"`);
        uninstall.push(
            `ReadRegStr $0 SHCTX "${key}" ""`,
            `StrCmp $0 "Rona.${extension}" 0 ${label}`,
            `DeleteRegValue SHCTX "${key}" ""`,
            `${label}:`,
        );
    }
    for (const [extension, kind] of kinds) {
        const progId = `Rona.${extension}`;
        const progClass = `Software\\Classes\\${progId}`;
        install.push(
            `WriteRegStr SHCTX "${progClass}" "" "${extension.toUpperCase()} ${kind}"`,
            `WriteRegStr SHCTX "${progClass}\\DefaultIcon" "" "${icon}"`,
            `WriteRegStr SHCTX "${progClass}\\shell\\open\\command" "" "${command}"`,
            // Offers Rona under Open with without taking the default from the app that has it.
            `WriteRegStr SHCTX "Software\\Classes\\.${extension}\\OpenWithProgids" "${progId}" ""`,
            `WriteRegStr SHCTX "${application}\\SupportedTypes" ".${extension}" ""`,
            `WriteRegStr SHCTX "${capabilities}\\FileAssociations" ".${extension}" "${progId}"`,
        );
        uninstall.push(
            `DeleteRegKey SHCTX "${progClass}"`,
            `DeleteRegValue SHCTX "Software\\Classes\\.${extension}\\OpenWithProgids" "${progId}"`,
        );
    }
    uninstall.push(
        `DeleteRegKey SHCTX "${application}"`,
        `DeleteRegKey SHCTX "Software\\Clients\\Media\\Rona"`,
        `DeleteRegValue SHCTX "Software\\RegisteredApplications" "${PRODUCT}"`,
    );

    const section = (name: string, lines: string[]) =>
        `Section ${name}\n${lines.map((line) => `  ${line}\n`).join("")}  !insertmacro UPDATEFILEASSOC\nSectionEnd\n`;
    return `${section("RegisterPlayer", install)}\n${section("un.UnregisterPlayer", uninstall)}`;
}
