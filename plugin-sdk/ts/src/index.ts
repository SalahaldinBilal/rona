// SDK for Rona plugins in TypeScript, plugin API 1.
//
// Every part of `host` that needs a permission is `undefined` while the user has that permission turned off,
// and the types mark each one optional, so the compiler makes a plugin check: `host.playback?.state()`.
// The user can turn permissions off at any time, so check on each use instead of keeping the object.

// The parts of the Extism JS PDK runtime this SDK uses. They exist as globals inside a plugin.
type ExtismOffset = number | bigint;

interface ExtismMemoryHandle {
  offset: ExtismOffset;
  readBytes(): ArrayBuffer;
  free(): void;
}

declare const Host: {
  inputString(): string;
  outputString(output: string): void;
  getFunctions(): Record<string, (...blocks: ExtismOffset[]) => ExtismOffset>;
};

declare const Memory: {
  fromString(text: string): ExtismMemoryHandle;
  fromBuffer(buffer: ArrayBufferLike): ExtismMemoryHandle;
  find(offset: ExtismOffset): ExtismMemoryHandle | null;
};

declare class TextEncoder {
  encode(text: string): Uint8Array;
}

declare class TextDecoder {
  decode(bytes: Uint8Array): string;
}

// The plugin API this SDK has, for apiVersion in plugin.json.
export const API_VERSION = "1.0";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export type PlaylistEntry = { index: number, title: string, path?: string, current: boolean };

export type IpcClient = { connection: number, pid: number, program: string, channel: string };

// Everything that happens, for the on_event hook. Events that need a permission only arrive while it is on.
export type RonaEvent =
  | { type: "started" }
  | { type: "stopping" }
  | { type: "permissions-changed", granted: string[] }
  | { type: "file-started", path: string | null }
  | { type: "file-loaded", path: string | null, title: string | null, duration: number }
  | { type: "file-ended", reason: "eof" | "stop" | "quit" | "error" | "other" }
  | { type: "idle" }
  | { type: "pause", paused: boolean }
  | { type: "seek", position: number }
  | { type: "position", position: number, duration: number }
  | { type: "volume", volume: number, muted: boolean }
  | { type: "speed", speed: number }
  | { type: "playlist-changed" }
  | { type: "tracks-changed" }
  | { type: "chapter-changed", chapter: number, title: string | null }
  | { type: "action", id: string }
  | { type: "menu", id: string, entry?: PlaylistEntry }
  | { type: "control", id: string, button: "left" | "right" }
  | { type: "timer", id: string }
  | { type: "ipc-connected", connection: number, client: IpcClient }
  | { type: "ipc-message", connection: number, message: Json }
  | { type: "ipc-closed", connection: number }
  | { type: "pipe-data", connection: number }
  | { type: "pipe-closed", connection: number }
  // A program from host.programs.start printed something. Read both of its streams.
  | { type: "process-output", process: number }
  // It ended. status is null when it was stopped. What it printed last can still be read.
  | { type: "process-exited", process: number, status: number | null }
  | { type: "property", name: string, value: Json };

// A node of a view: groups hold rows, rows hold up to 4 items. See the node table in the SDK README.
export type ViewNode = {
  type: "group" | "row" | "label" | "badge" | "button" | "toggle" | "select" | "segmented" | "slider" | "text"
    | "color" | "progress" | "image" | "list" | "link" | "separator" | "empty",
  id?: string,
  children?: ViewNode[],
  [prop: string]: unknown,
};

export type DialogAction = { id: string, label: string, primary?: boolean, danger?: boolean };

export type UiResponse = {
  tree?: ViewNode[],
  // Buttons at the bottom of a dialog, after Rona's own Close.
  actions?: DialogAction[],
  // An OSD message, dropped without ui.osd.
  osd?: string,
  // Closes the dialog or popover the event came from.
  close?: boolean,
  // Opens one of the manifest's dialogs. Only works in answer to something the user did.
  open?: string,
  // Asks for ui_render again after this long while the view stays open. 500 ms at least.
  refreshAfterMs?: number,
};

export type UiEvent = {
  // "settings", "panel", "popover:<control id>" or "dialog:<dialog id>".
  view: string,
  id: string,
  type: "click" | "change" | "submit",
  value?: Json,
  // The list entry that was clicked, for list nodes.
  entry?: string | null,
};

export type Resolved = { url: string, headers?: Record<string, string>, title?: string };

// Hooks run to completion with no event loop, so they can't be async.
export type Hooks = {
  on_event?: (event: RonaEvent) => void,
  ui_render?: (input: { view: string }) => UiResponse,
  ui_event?: (input: UiEvent) => UiResponse | void,
  resolve_link?: (input: { url: string }) => Resolved | null,
  ipc_accept?: (client: IpcClient) => boolean,
};

let granted = new Set<string>();

type Handler = (input: never) => unknown;

function exported(name: string, handler: Handler): () => number {
  return function hook(): number {
    const input = JSON.parse(Host.inputString());
    granted = new Set(Array.isArray(input.granted) ? input.granted : []);
    const payload = name === "on_event" ? input.event : name === "ipc_accept" ? input.client : input;
    const output = (handler as (input: unknown) => unknown)(payload);
    Host.outputString(JSON.stringify(output ?? null));
    return 0;
  };
}

// The default export of a plugin's entry file. Every hook also has to be listed in plugin.json.
export function definePlugin(hooks: Hooks): Record<string, () => number> {
  const exports: Record<string, () => number> = {};
  for (const [name, handler] of Object.entries(hooks)) {
    if (handler) exports[name] = exported(name, handler as Handler);
  }
  return exports;
}

// The user turned this permission off during the hook.
export class PermissionDenied extends Error {
  constructor(readonly permission: string) {
    super(`The ${permission} permission is turned off`);
  }
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

// Offset 0 stands for an empty block in both directions.
function block(data: string | Uint8Array): ExtismMemoryHandle | null {
  if (data.length === 0) return null;
  if (typeof data === "string") return Memory.fromString(data);
  return Memory.fromBuffer(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
}

// Every reply starts with a status byte: 0 ok, 1 denied (the permission follows), 2 failed (the message follows).
function call(name: string, ...data: Array<string | Uint8Array>): Uint8Array {
  const blocks = data.map(block);
  const offset = Host.getFunctions()[name](...blocks.map(memory => memory ? memory.offset : 0));
  for (const memory of blocks) memory?.free();
  const memory = Number(offset) === 0 ? null : Memory.find(offset);
  const reply = memory ? new Uint8Array(memory.readBytes()) : new Uint8Array(0);
  memory?.free();
  if (reply.length === 0) throw new Error("Rona sent no reply");
  const payload = reply.subarray(1);
  if (reply[0] === 1) throw new PermissionDenied(decoder.decode(payload));
  if (reply[0] !== 0) throw new Error(decoder.decode(payload));
  return payload;
}

function json<T>(bytes: Uint8Array): T {
  return JSON.parse(decoder.decode(bytes)) as T;
}

function text(bytes: Uint8Array): string {
  return decoder.decode(bytes);
}

const when = <T>(permission: string, value: T): T | undefined => granted.has(permission) ? value : undefined;

export type PlayerState = {
  state: "idle" | "opening" | "playing" | "paused",
  position: number,
  duration: number,
  speed: number,
  volume: number,
  muted: boolean,
  // Only with media.metadata.
  title?: string,
  // Only with media.path.
  path?: string,
};

export type MediaInfo = {
  title: string,
  artist?: string | null,
  album?: string | null,
  videoCodec?: string | null,
  audioCodec?: string | null,
  width?: number | null,
  height?: number | null,
  audioOnly: boolean,
  chapters: Array<{ time: number, title: string }>,
  tags: Record<string, string>,
};

export type Track = {
  id: number,
  kind: "audio" | "sub" | "video",
  title?: string | null,
  language?: string | null,
  codec?: string | null,
  selected: boolean,
  external: boolean,
};

export type ControlState = Partial<{
  fill: string, stroke: string, text: string, tone: string, active: boolean, dot: boolean, highlight: boolean,
  visible: boolean, enabled: boolean,
}>;

export type HttpRequest = {
  method: string,
  url: string,
  headers?: Array<[string, string]>,
  body?: { type: "none" } | { type: "text", text: string } | { type: "json", json: Json } | { type: "bytes" },
  timeoutMs?: number,
  // Sent as the body for `{ type: "bytes" }`.
  bytes?: Uint8Array,
};

export type HttpResponse = {
  status: number,
  ok: boolean,
  headers: Array<[string, string]>,
  body: Uint8Array,
  text(): string,
  json<T>(): T,
  header(name: string): string | null,
};

const playback = {
  state: (): PlayerState => json(call("player_state")),
  // Sends position events at most this often while something plays. 0 stops them, the least is 250 ms.
  positionEvents: (intervalMs: number): void => void call("position_events", JSON.stringify({ intervalMs })),
};

const control = (args: Json) => void call("player_control", JSON.stringify(args));
const playbackControl = {
  play: () => control({ action: "play" }),
  pause: () => control({ action: "pause" }),
  togglePause: () => control({ action: "toggle" }),
  seekTo: (seconds: number) => control({ action: "seek", to: seconds }),
  seekBy: (seconds: number) => control({ action: "seek", by: seconds }),
  setSpeed: (speed: number) => control({ action: "speed", value: speed }),
  // 0 to 1, or more when the user allows a louder maximum.
  setVolume: (volume: number) => control({ action: "volume", value: volume }),
  setMuted: (muted: boolean) => control({ action: "mute", value: muted }),
  next: () => control({ action: "next" }),
  previous: () => control({ action: "previous" }),
  stop: () => control({ action: "stop" }),
};

const media = { info: (): MediaInfo => json(call("media_info")) };

const tracks = {
  list: (): Track[] => json(call("tracks_get")),
  // null turns that kind off.
  select: (kind: "audio" | "sub", id: number | null): void => void call("tracks_select", JSON.stringify({ kind, id })),
};

const subtitles = {
  // Adds a subtitle file's contents to what plays and selects it. `extension` is like "srt" or "ass".
  add: (name: string, language: string | null, extension: string, bytes: Uint8Array): void =>
    void call("subtitle_add", JSON.stringify({ name, language, extension }), bytes),
};

const currentFile = {
  size: (): number => json(call("file_size")),
  // Up to 4 MiB from offset. Shorter at the end of the file.
  read: (offset: number, length: number): Uint8Array => call("file_read", JSON.stringify({ offset, length })),
};

const opener = {
  open: (sources: string[]): void => void call("open_media", JSON.stringify({ sources, append: false })),
  append: (sources: string[]): void => void call("open_media", JSON.stringify({ sources, append: true })),
};

const playlist = { entries: (): PlaylistEntry[] => json(call("playlist_get")) };

const edit = (op: Json) => void call("playlist_edit", JSON.stringify(op));
const playlistEdit = {
  remove: (index: number) => edit({ op: "remove", index }),
  // Moves the entry so it lands before the one at `before`, or at the end when that is the length.
  move: (index: number, before: number) => edit({ op: "move", index, before }),
  play: (index: number) => edit({ op: "play", index }),
};

const mpvRead = {
  get: (name: string): Json => json(call("mpv_get", name)),
  // Sends a property event each time it changes, and once right away.
  observe: (name: string): void => void call("mpv_observe", name),
};

const mpvWrite = { set: (name: string, value: string): void => void call("mpv_set", name, value) };

const mpvCommand = { run: (...args: string[]): void => void call("mpv_command", JSON.stringify(args)) };

const osd = { show: (message: string): void => void call("osd_show", message) };

const overlay = {
  // ASS events drawn on the video, on a 1280 by 720 canvas scaled to the picture. Ids are 1 to 16.
  set: (id: number, ass: string): void => void call("overlay_set", JSON.stringify({ id }), ass),
  clear: (id: number): void => void call("overlay_clear", JSON.stringify({ id })),
};

const controls = { set: (id: string, state: ControlState): void => void call("controls_set", id, JSON.stringify(state)) };

const menu = {
  set: (id: string, state: { label?: string, checked?: boolean, visible?: boolean }): void =>
    void call("menu_set", id, JSON.stringify(state)),
};

const clipboardRead = { text: (): string => text(call("clipboard_read")) };
const clipboardWrite = { text: (value: string): void => void call("clipboard_write", value) };

const network = {
  // Throws on a network failure or a host that is off. An HTTP error status is still a response.
  send(request: HttpRequest): HttpResponse {
    const { bytes, ...rest } = request;
    const reply = json<{ status: number, headers: Array<[string, string]> }>(
      call("http_request", JSON.stringify({ headers: [], body: { type: "none" }, ...rest }), bytes ?? new Uint8Array(0)),
    );
    const body = call("http_response_body");
    return {
      status: reply.status,
      ok: reply.status >= 200 && reply.status < 300,
      headers: reply.headers,
      body,
      text: () => decoder.decode(body),
      json: <T>() => JSON.parse(decoder.decode(body)) as T,
      header: (name: string) => reply.headers.find(([key]) => key.toLowerCase() === name.toLowerCase())?.[1] ?? null,
    };
  },
};

const ipcServe = {
  send: (connection: number, message: Json): void => void call("ipc_send", String(connection), JSON.stringify(message)),
  close: (connection: number): void => void call("ipc_close", String(connection)),
};

const pipes = {
  // Opens \\.\pipe\<name> on Windows, or <name> in the runtime folder elsewhere. Arriving bytes send pipe-data events.
  connect: (name: string): number => json(call("pipe_connect", name)),
  write: (connection: number, bytes: Uint8Array): void => void call("pipe_write", String(connection), bytes),
  // Everything that arrived since the last read.
  read: (connection: number): Uint8Array => call("pipe_read", String(connection)),
  close: (connection: number): void => void call("pipe_close", String(connection)),
};

export type RunOutput = {
  // null when the program was stopped, by the timeout or otherwise.
  status: number | null,
  timedOut: boolean,
  stdout: Uint8Array,
  stderr: string,
  ok: boolean,
  text(): string,
};

const programs = {
  // Runs one of the manifest's programs, found on PATH, without a shell, and waits for it. 60 s by default,
  // 10 minutes at most. Output is kept up to 32 MB each.
  run(program: string, args: string[] = [], options: { stdin?: Uint8Array | string, timeoutMs?: number } = {}): RunOutput {
    const stdin = typeof options.stdin === "string" ? encoder.encode(options.stdin) : options.stdin ?? new Uint8Array(0);
    const reply = json<{ status: number | null, timedOut: boolean, stderr: string }>(
      call("process_run", JSON.stringify({ program, args, timeoutMs: options.timeoutMs }), stdin),
    );
    const stdout = call("process_stdout");
    return { ...reply, stdout, ok: reply.status === 0, text: () => decoder.decode(stdout) };
  },
  // Starts one of the manifest's programs and returns at once. It runs until it exits, is closed or the plugin
  // stops, with no time limit. Output sends process-output events and the end process-exited. At most 4 at a
  // time, and ones that ended count until closed.
  start: (program: string, args: string[] = []): number => json(call("process_start", JSON.stringify({ program, args }))),
  write: (process: number, input: Uint8Array | string): void =>
    void call("process_write", String(process), typeof input === "string" ? encoder.encode(input) : input),
  // Everything it printed since the last read.
  readStdout: (process: number): Uint8Array => call("process_read", String(process), "stdout"),
  readStderr: (process: number): Uint8Array => call("process_read", String(process), "stderr"),
  // Closes its input, which tells many programs to finish.
  endInput: (process: number): void => void call("process_end_input", String(process)),
  // Stops it if it still runs, along with what it started, and forgets it.
  close: (process: number): void => void call("process_close", String(process)),
};

export type RonaHost = {
  // Whether the user has this permission (or network:<host>, ipc.connect:<pipe>) on right now.
  granted(permission: string): boolean,
  log(level: "debug" | "info" | "warn" | "error", message: string): void,
  // The plugin's own storage. Private to it, so it needs no permission.
  storage: {
    get(key: string): Uint8Array | null,
    getText(key: string): string | null,
    set(key: string, value: string | Uint8Array): void,
    delete(key: string): void,
    keys(prefix?: string): string[],
  },
  // The value of a static setting from plugin.json, or its declared default.
  setting<T = Json>(id: string): T | null,
  // Timers send timer events, since a plugin has no thread of its own. At least 250 ms.
  timers: { set(id: string, ms: number, repeat?: boolean): void, clear(id: string): void },
  // The plugin's own views. They only exist when the manifest describes them.
  ui: {
    update(view: string, tree: ViewNode[]): void,
    patch(view: string, patches: Array<{ id: string, [prop: string]: unknown }>): void,
    image(id: string, bytes: Uint8Array): void,
    // Only while handling a control, menu or action event. From ui_event, return { open } instead.
    openDialog(dialog: string): void,
  },
  readonly playback?: typeof playback,
  readonly playbackControl?: typeof playbackControl,
  readonly media?: typeof media,
  readonly tracks?: typeof tracks,
  readonly subtitles?: typeof subtitles,
  readonly currentFile?: typeof currentFile,
  readonly opener?: typeof opener,
  readonly playlist?: typeof playlist,
  readonly playlistEdit?: typeof playlistEdit,
  readonly mpvRead?: typeof mpvRead,
  readonly mpvWrite?: typeof mpvWrite,
  readonly mpvCommand?: typeof mpvCommand,
  readonly osd?: typeof osd,
  readonly overlay?: typeof overlay,
  readonly controls?: typeof controls,
  readonly menu?: typeof menu,
  readonly clipboardRead?: typeof clipboardRead,
  readonly clipboardWrite?: typeof clipboardWrite,
  // Any network:<host> on. Each request is checked against the hosts that are on.
  readonly network?: typeof network,
  readonly ipcServe?: typeof ipcServe,
  // Any ipc.connect:<pipe> on. Each connect is checked against the pipes that are on.
  readonly pipes?: typeof pipes,
  // Any system.run:<program> on. Each run is checked against the programs that are on.
  readonly programs?: typeof programs,
};

const anyGranted = (prefix: string) => [...granted].some(grant => grant.startsWith(prefix));

export const host: RonaHost = {
  granted: (permission: string) => granted.has(permission),
  log: (level, message) => void call("log", JSON.stringify({ level, text: message })),
  storage: {
    get(key) {
      const reply = call("kv_get", key);
      return reply[0] === 1 ? reply.slice(1) : null;
    },
    getText(key) {
      const value = host.storage.get(key);
      return value === null ? null : decoder.decode(value);
    },
    set: (key, value) => void call("kv_set", key, typeof value === "string" ? encoder.encode(value) : value),
    delete: key => void call("kv_delete", key),
    keys: (prefix = "") => json(call("kv_keys", prefix)),
  },
  setting: <T>(id: string) => json<T | null>(call("setting_get", id)),
  timers: {
    set: (id, ms, repeat = false) => void call("timer_set", JSON.stringify({ id, ms, repeat })),
    clear: id => void call("timer_clear", id),
  },
  ui: {
    update: (view, tree) => void call("ui_update", view, JSON.stringify(tree)),
    patch: (view, patches) => void call("ui_patch", view, JSON.stringify(patches)),
    image: (id, bytes) => void call("ui_image", JSON.stringify({ id }), bytes),
    openDialog: dialog => void call("ui_open", dialog),
  },
  get playback() { return when("playback.read", playback); },
  get playbackControl() { return when("playback.control", playbackControl); },
  get media() { return when("media.metadata", media); },
  get tracks() { return when("media.tracks", tracks); },
  get subtitles() { return when("media.subtitles", subtitles); },
  get currentFile() { return when("media.file", currentFile); },
  get opener() { return when("open.media", opener); },
  get playlist() { return when("playlist.read", playlist); },
  get playlistEdit() { return when("playlist.edit", playlistEdit); },
  get mpvRead() { return when("mpv.read", mpvRead); },
  get mpvWrite() { return when("mpv.write", mpvWrite); },
  get mpvCommand() { return when("mpv.command", mpvCommand); },
  get osd() { return when("ui.osd", osd); },
  get overlay() { return when("ui.overlay", overlay); },
  get controls() { return when("ui.controls", controls); },
  get menu() { return when("ui.menu", menu); },
  get clipboardRead() { return when("clipboard.read", clipboardRead); },
  get clipboardWrite() { return when("clipboard.write", clipboardWrite); },
  get network() { return anyGranted("network:") ? network : undefined; },
  get ipcServe() { return when("ipc.serve", ipcServe); },
  get pipes() { return anyGranted("ipc.connect:") ? pipes : undefined; },
  get programs() { return anyGranted("system.run:") ? programs : undefined; },
};
