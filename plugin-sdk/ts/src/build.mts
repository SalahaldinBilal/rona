// Bundles a plugin's src/index.ts and compiles it with extism-js into plugin.wasm next to plugin.json.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { delimiter, join, resolve } from "node:path";
import { build } from "esbuild";
import { ensureTools } from "./tools.mts";

// Name and number of arguments. Keep in step with FUNCTIONS in src/plugins/host.rs.
export const HOST_FUNCTIONS: Array<[name: string, parameters: number]> = [
  ["log", 1],
  ["kv_get", 1],
  ["kv_set", 2],
  ["kv_delete", 1],
  ["kv_keys", 1],
  ["setting_get", 1],
  ["player_state", 0],
  ["player_control", 1],
  ["media_info", 0],
  ["tracks_get", 0],
  ["tracks_select", 1],
  ["subtitle_add", 2],
  ["file_size", 0],
  ["file_read", 1],
  ["open_media", 1],
  ["playlist_get", 0],
  ["playlist_edit", 1],
  ["mpv_get", 1],
  ["mpv_observe", 1],
  ["mpv_set", 2],
  ["mpv_command", 1],
  ["osd_show", 1],
  ["overlay_set", 2],
  ["overlay_clear", 1],
  ["ui_update", 2],
  ["ui_patch", 2],
  ["ui_image", 2],
  ["ui_open", 1],
  ["controls_set", 2],
  ["menu_set", 2],
  ["clipboard_read", 0],
  ["clipboard_write", 1],
  ["http_request", 2],
  ["http_response_body", 0],
  ["timer_set", 1],
  ["timer_clear", 1],
  ["position_events", 1],
  ["ipc_send", 2],
  ["ipc_close", 1],
  ["pipe_connect", 1],
  ["pipe_write", 2],
  ["pipe_read", 1],
  ["pipe_close", 1],
  ["process_run", 2],
  ["process_stdout", 0],
  ["process_start", 1],
  ["process_write", 2],
  ["process_read", 2],
  ["process_end_input", 1],
  ["process_close", 1],
];

const ENTRIES = ["src/index.ts", "src/index.js"];

// What extism-js reads to know the plugin's exports and the host functions it imports.
function interfaceFile(hooks: string[]): string {
  const exports = hooks.map(hook => `  export function ${hook}(): I32;`).join("\n");
  const imports = HOST_FUNCTIONS
    .map(([name, count]) => {
      const params = Array.from({ length: count }, (_, index) => `block${index}: I64`).join(", ");
      return `    ${name}(${params}): I64;`;
    })
    .join("\n");
  return `declare module "main" {\n${exports}\n}\n\ndeclare module "extism:host" {\n  interface user {\n${imports}\n  }\n}\n`;
}

// Throws with the reason when the plugin can't be built. Returns the path of plugin.wasm.
export async function buildPlugin(pluginFolder: string): Promise<string> {
  const folder = resolve(pluginFolder);
  if (!existsSync(join(folder, "plugin.json"))) throw new Error(`${folder} has no plugin.json`);
  const entry = ENTRIES.find(path => existsSync(join(folder, path)));
  if (!entry) throw new Error(`${folder} has no src/index.ts`);

  const manifest: { hooks?: unknown } = JSON.parse(readFileSync(join(folder, "plugin.json"), "utf-8"));
  const hooks = Array.isArray(manifest.hooks) ? manifest.hooks.filter((hook): hook is string => typeof hook === "string") : [];
  if (hooks.length === 0) throw new Error("plugin.json lists no hooks");

  const tools = await ensureTools();

  mkdirSync(join(folder, "dist"), { recursive: true });
  await build({
    stdin: {
      contents: `module.exports = require("./${entry}").default;`,
      resolveDir: folder,
      sourcefile: "entry.js",
    },
    outfile: join(folder, "dist/plugin.js"),
    bundle: true,
    format: "cjs",
    target: ["es2020"],
    minify: false,
    logLevel: "warning",
  });
  writeFileSync(join(folder, "dist/plugin.d.ts"), interfaceFile(hooks));

  const output = join(folder, "plugin.wasm");
  const compiled = spawnSync(tools.extismJs, ["dist/plugin.js", "-i", "dist/plugin.d.ts", "-o", "plugin.wasm"], {
    cwd: folder,
    stdio: "inherit",
    env: { ...process.env, PATH: `${tools.binaryenBin}${delimiter}${process.env.PATH ?? ""}` },
  });
  if (compiled.error) throw new Error(`Couldn't run extism-js: ${compiled.error.message}`);
  if (compiled.status !== 0) throw new Error("extism-js couldn't compile the plugin");
  return output;
}
