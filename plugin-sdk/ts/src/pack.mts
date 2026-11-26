// Checks <folder>/plugin.json against the rules Rona applies on install and writes <id>-<version>.rp from
// plugin.json, plugin.wasm and icon.png (if there is one).
//
// The manifest rules mirror plugin-sdk/rust/src/manifest.rs. Keep them in step.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { crc32, deflateRawSync } from "node:zlib";

import { validateTree } from "./tree.mts";

const API_VERSION = 1;
const WASM_MAGIC = Buffer.from([0x00, 0x61, 0x73, 0x6d]);
const HOOKS = ["on_event", "ui_render", "ui_event", "resolve_link", "ipc_accept"];
const PERMISSIONS = [
  "playback.read", "playback.control", "media.metadata", "media.path", "media.tracks", "media.subtitles", "media.file",
  "open.media", "open.resolve", "playlist.read", "playlist.edit", "mpv.read", "mpv.write", "mpv.command", "ui.osd",
  "ui.overlay", "ui.panel", "ui.controls", "ui.menu", "input.actions", "clipboard.read", "clipboard.write", "network",
  "ipc.serve", "ipc.connect", "system.run",
];
const TONES = ["", "normal", "muted", "accent", "success", "danger"];
const MAX_CONTROLS = 3;
const MAX_MENU_ENTRIES = 8;
const MAX_ACTIONS = 16;
const MAX_DIALOGS = 8;
const MAX_LABEL = 40;
const MAX_REASON = 200;
const LIMITS = { "plugin.json": 256 * 1024, "plugin.wasm": 64 * 1024 * 1024, "icon.png": 256 * 1024 };

const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/;
const PLUGIN_ID = /^[a-z0-9.-]{3,64}$/;
const ITEM_ID = /^[A-Za-z0-9_-]{1,64}$/;
const HOST_LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
const IPV4 = /^\d{1,3}(\.\d{1,3}){3}$/;
const PIPE = /^[A-Za-z0-9_][A-Za-z0-9._-]{0,63}$/;
const PROGRAM = /^[A-Za-z0-9_+][A-Za-z0-9._+-]{0,63}$/;
const PATH_DATA =/^[MmLlHhVvCcSsQqTtAaZz0-9 ,.\-\n]*$/;

type Json = Record<string, unknown>;

function fail(message: string): never {
  throw new Error(message);
}

function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function list(value: unknown, what: string): unknown[] {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) fail(`${what} must be a list`);
  return value;
}

function label(value: unknown, what: string): void {
  const length = typeof value === "string" ? [...value.trim()].length : 0;
  if (length < 1 || length > MAX_LABEL) fail(`${what} must be 1 to ${MAX_LABEL} characters`);
}

function itemId(value: unknown, what: string, seen: Set<string>): void {
  if (typeof value !== "string" || !ITEM_ID.test(value)) fail(`${what} id "${String(value)}" is not valid. Use letters, digits, dashes and underscores`);
  if (seen.has(value)) fail(`The id "${value}" is used twice`);
  seen.add(value);
}

function pathData(value: unknown): void {
  if (value === undefined) return;
  if (typeof value !== "string" || value.length > 2000 || !PATH_DATA.test(value)) fail("Icon path data may only hold path commands and numbers");
}

function checkHost(entry: unknown): void {
  const host = typeof entry === "string" ? entry.replace(/^\*\./, "") : "";
  const valid = host.length > 0 && host.length <= 253 && host.includes(".")
    && host.split(".").every(part => HOST_LABEL.test(part)) && !IPV4.test(host);
  if (!valid) fail(`Network host "${String(entry)}" is not valid. Use a lowercase hostname like api.example.com or *.example.com`);
}

function checkPipe(entry: unknown): void {
  if (typeof entry !== "string" || !PIPE.test(entry) || entry.toLowerCase().startsWith("rona")) {
    fail(`Pipe "${String(entry)}" is not valid. Use up to 64 letters, digits, dots, dashes and underscores`);
  }
}

export function checkManifest(manifest: unknown): { id: string, version: string } {
  if (!isObject(manifest)) fail("plugin.json must hold an object");
  if (manifest.apiVersion !== API_VERSION) fail(`apiVersion must be ${API_VERSION}`);

  const id = manifest.id;
  const idValid = typeof id === "string" && PLUGIN_ID.test(id) && !/^[.-]|[.-]$/.test(id) && !id.includes("..");
  if (!idValid) fail(`Plugin id "${String(id)}" is not valid. Use 3 to 64 lowercase letters, digits, dots and dashes`);
  const name = typeof manifest.name === "string" ? [...manifest.name.trim()].length : 0;
  if (name < 1 || name > 64) fail("The plugin name must be 1 to 64 characters");
  const version = manifest.version;
  if (typeof version !== "string" || !SEMVER.test(version)) fail(`Plugin version "${String(version)}" is not a valid version`);
  if (manifest.minAppVersion !== undefined && manifest.minAppVersion !== null
    && (typeof manifest.minAppVersion !== "string" || !SEMVER.test(manifest.minAppVersion))) {
    fail(`minAppVersion "${String(manifest.minAppVersion)}" is not a valid version`);
  }
  if (manifest.homepage !== undefined && manifest.homepage !== null
    && (typeof manifest.homepage !== "string" || !manifest.homepage.startsWith("https://"))) {
    fail("homepage must be an https link");
  }

  const hooks = list(manifest.hooks, "hooks");
  for (const hook of hooks) {
    if (typeof hook !== "string" || !HOOKS.includes(hook)) fail(`Unknown hook "${String(hook)}"`);
  }
  const has = (hook: string) => hooks.includes(hook);

  const permissions = manifest.permissions ?? {};
  if (!isObject(permissions)) fail("permissions must be an object");
  for (const [permission, request] of Object.entries(permissions)) {
    if (!PERMISSIONS.includes(permission)) fail(`Unknown permission "${permission}"`);
    const detailed = isObject(request) ? request : null;
    if (!detailed && typeof request !== "string") fail(`${permission} takes a reason, or an object with one`);
    const reason = detailed ? detailed.reason ?? "" : request;
    if (typeof reason !== "string" || [...reason].length > MAX_REASON) fail(`The reason for ${permission} is longer than ${MAX_REASON} characters`);
    const lists: Array<[string, string, unknown[]]> = [
      ["network", "hosts", list(detailed?.hosts, `${permission} hosts`)],
      ["ipc.connect", "pipes", list(detailed?.pipes, `${permission} pipes`)],
      ["system.run", "programs", list(detailed?.programs, `${permission} programs`)],
    ];
    for (const [owner, what, entries] of lists) {
      if (permission === owner && entries.length === 0) fail(`${permission} needs a list of ${what}`);
      if (permission !== owner && entries.length > 0) fail(`Only ${owner} takes that list, not ${permission}`);
    }
    lists[0][2].forEach(checkHost);
    lists[1][2].forEach(checkPipe);
    for (const program of lists[2][2]) {
      if (typeof program !== "string" || !PROGRAM.test(program)) {
        fail(`Program "${String(program)}" is not valid. Use the name of a program on PATH, like yt-dlp, without a folder`);
      }
    }
  }
  const asks = (permission: string) => permission in permissions;
  if (asks("open.resolve") !== has("resolve_link")) fail("open.resolve and the resolve_link hook go together");
  if (has("ipc_accept") && !asks("ipc.serve")) fail("The ipc_accept hook needs the ipc.serve permission");
  if ((asks("ipc.serve") || asks("ipc.connect")) && !has("on_event")) fail("A plugin that talks to other programs needs the on_event hook");
  if (has("ui_render") !== has("ui_event")) fail("ui_render and ui_event go together");

  const ui = isObject(manifest.ui) ? manifest.ui : {};
  const ids = new Set<string>();
  if (ui.panel !== undefined && ui.panel !== null) {
    if (!isObject(ui.panel)) fail("ui.panel must be an object");
    if (!asks("ui.panel")) fail("A panel needs the ui.panel permission");
    if (!has("ui_render")) fail("A panel needs the ui_render hook");
    label(ui.panel.title, "The panel title");
    pathData(ui.panel.fill);
    pathData(ui.panel.stroke);
  } else if (asks("ui.panel")) {
    fail("ui.panel is asked for but ui.panel is not described");
  }
  const controls = list(ui.controls, "ui.controls");
  if (controls.length > 0 && !asks("ui.controls")) fail("Controls need the ui.controls permission");
  if (controls.length + (ui.panel ? 1 : 0) > MAX_CONTROLS) fail(`A plugin can have at most ${MAX_CONTROLS} controls, counting the panel button`);
  for (const control of controls) {
    if (!isObject(control)) fail("Every control must be an object");
    itemId(control.id, "Control", ids);
    label(control.label, "A control label");
    pathData(control.fill);
    pathData(control.stroke);
    const kind = control.kind ?? "button";
    if (!["button", "text", "badge"].includes(String(kind))) fail(`Unknown control kind "${String(kind)}"`);
    if (control.tone !== undefined && !TONES.includes(String(control.tone))) fail(`Unknown tone "${String(control.tone)}"`);
    if (typeof control.text === "string" && [...control.text].length > 8) fail("Control text can be at most 8 characters");
    if (kind === "button" && !control.fill && !control.stroke) fail(`The button "${String(control.id)}" has no icon (fill or stroke)`);
    if (control.popover === true && kind === "badge") fail("A badge can't open a popover");
    if (control.popover === true && !has("ui_render")) fail("A control with a popover needs the ui_render hook");
  }
  const menu = list(ui.menu, "ui.menu");
  const rowMenu = list(ui.rowMenu, "ui.rowMenu");
  if (menu.length + rowMenu.length > 0 && !asks("ui.menu")) fail("Menu entries need the ui.menu permission");
  if (menu.length > MAX_MENU_ENTRIES || rowMenu.length > MAX_MENU_ENTRIES) fail(`A menu can have at most ${MAX_MENU_ENTRIES} entries`);
  for (const entry of [...menu, ...rowMenu]) {
    if (!isObject(entry)) fail("Every menu entry must be an object");
    itemId(entry.id, "Menu entry", ids);
    label(entry.label, "A menu label");
  }
  const actions = list(manifest.actions, "actions");
  if (actions.length > 0 && !asks("input.actions")) fail("Actions need the input.actions permission");
  if (actions.length > MAX_ACTIONS) fail(`A plugin can have at most ${MAX_ACTIONS} actions`);
  for (const action of actions) {
    if (!isObject(action)) fail("Every action must be an object");
    itemId(action.id, "Action", ids);
    label(action.label, "An action label");
  }
  const dialogs = list(ui.dialogs, "ui.dialogs");
  if (dialogs.length > MAX_DIALOGS) fail(`A plugin can have at most ${MAX_DIALOGS} dialogs`);
  if (dialogs.length > 0 && !has("ui_render")) fail("Dialogs need the ui_render hook");
  for (const dialog of dialogs) {
    if (!isObject(dialog)) fail("Every dialog must be an object");
    itemId(dialog.id, "Dialog", ids);
    label(dialog.title, "A dialog title");
  }
  if ((controls.length > 0 || menu.length + rowMenu.length > 0 || actions.length > 0) && !has("on_event")) {
    fail("Controls, menu entries and actions need the on_event hook");
  }
  try {
    validateTree(list(manifest.settings, "settings"), "settings");
  } catch (error) {
    fail(`settings: ${error instanceof Error ? error.message : String(error)}`);
  }
  return { id: id as string, version };
}

type Entry = { name: string, data: Buffer };

function zip(entries: Entry[]): Buffer {
  const parts: Buffer[] = [];
  const directory: Buffer[] = [];
  let offset = 0;
  for (const entry of entries) {
    const name = Buffer.from(entry.name, "utf-8");
    const packed = deflateRawSync(entry.data);
    const checksum = crc32(entry.data);

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(8, 8);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(packed.length, 18);
    local.writeUInt32LE(entry.data.length, 22);
    local.writeUInt16LE(name.length, 26);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(8, 10);
    central.writeUInt32LE(checksum, 16);
    central.writeUInt32LE(packed.length, 20);
    central.writeUInt32LE(entry.data.length, 24);
    central.writeUInt16LE(name.length, 28);
    central.writeUInt32LE(offset, 42);

    parts.push(local, name, packed);
    directory.push(central, name);
    offset += local.length + name.length + packed.length;
  }
  const directorySize = directory.reduce((sum, part) => sum + part.length, 0);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directorySize, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...parts, ...directory, end]);
}

function readEntry(folder: string, name: keyof typeof LIMITS, required: boolean): Entry | null {
  const path = join(folder, name);
  if (!existsSync(path)) {
    if (required) fail(`${path} is missing`);
    return null;
  }
  const data = readFileSync(path);
  if (data.length > LIMITS[name]) fail(`${name} is larger than ${Math.floor(LIMITS[name] / 1024)} KB`);
  return { name, data };
}

// Throws with the reason when the plugin can't be packed. Returns the path of the written file.
export function packPlugin(pluginFolder: string, outputFile?: string): string {
  const folder = resolve(pluginFolder);
  const manifestEntry = readEntry(folder, "plugin.json", true)!;
  const wasmEntry = readEntry(folder, "plugin.wasm", true)!;
  const iconEntry = readEntry(folder, "icon.png", false);
  if (!wasmEntry.data.subarray(0, 4).equals(WASM_MAGIC)) fail("plugin.wasm is not a WebAssembly module");

  let manifest: unknown;
  try {
    manifest = JSON.parse(manifestEntry.data.toString("utf-8"));
  } catch (error) {
    fail(`plugin.json is not valid JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
  const { id, version } = checkManifest(manifest);

  const entries = [manifestEntry, wasmEntry, ...(iconEntry ? [iconEntry] : [])];
  const output = resolve(outputFile ?? join(folder, `${id}-${version}.rp`));
  writeFileSync(output, zip(entries));
  return output;
}
