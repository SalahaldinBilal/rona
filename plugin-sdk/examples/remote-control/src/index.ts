import { definePlugin, host, Json, ViewNode } from "@rona-player/plugin";

// Connected programs, kept in storage since the plugin instance can be rebuilt between calls.
type Client = { connection: number, program: string };

function clients(): Client[] {
  return JSON.parse(host.storage.getText("clients") ?? "[]");
}

function saveClients(list: Client[]): void {
  host.storage.set("clients", JSON.stringify(list));
  // The panel may not be open, and without ui.panel there is none. Both are fine.
  try {
    host.ui.update("panel", panel());
  } catch {
    // Nothing to update.
  }
}

function programName(path: string): string {
  return path.split(/[\\/]/).pop() || "A program";
}

function panel(): ViewNode[] {
  const connected = clients();
  if (connected.length === 0) return [{ type: "empty", text: "No program is connected." }];
  return [{
    type: "group",
    title: "Connected",
    children: [{
      type: "list",
      id: "clients",
      entries: connected.map(client => ({
        id: `c${client.connection}`,
        title: programName(client.program),
        subtitle: client.program,
        buttons: [{ id: "drop", label: "Disconnect" }],
      })),
    }],
  }];
}

function state(): Json {
  const playback = host.playback?.state();
  if (!playback) return { error: "Reading playback is turned off" };
  return { state: playback.state, position: playback.position, duration: playback.duration, title: playback.title ?? null };
}

function run(message: Json): Json {
  if (typeof message !== "object" || message === null || Array.isArray(message)) return { error: "Send an object like {\"cmd\": \"toggle\"}" };
  const command = message.cmd;
  if (command === "state") return state();
  const control = host.playbackControl;
  if (!control) return { error: "Controlling playback is turned off" };
  switch (command) {
    case "play": control.play(); break;
    case "pause": control.pause(); break;
    case "toggle": control.togglePause(); break;
    case "next": control.next(); break;
    case "previous": control.previous(); break;
    case "seek":
      if (typeof message.to === "number") control.seekTo(message.to);
      else if (typeof message.by === "number") control.seekBy(message.by);
      else return { error: "seek needs to or by" };
      break;
    default: return { error: `Unknown command ${String(command)}` };
  }
  return { ok: true };
}

export default definePlugin({
  // Only programs of the user running Rona can reach the pipe at all. The program path comes from the system,
  // the channel from the program itself, so only the path can be trusted.
  ipc_accept(client) {
    host.log("info", `${client.program || "A program"} connected (${client.channel || "no channel"})`);
    return true;
  },

  on_event(event) {
    switch (event.type) {
      case "started":
        host.storage.set("clients", "[]");
        break;
      case "ipc-connected":
        saveClients([...clients(), { connection: event.connection, program: event.client.program }]);
        break;
      case "ipc-closed":
        saveClients(clients().filter(client => client.connection !== event.connection));
        break;
      case "ipc-message":
        host.ipcServe?.send(event.connection, run(event.message));
        break;
      case "permissions-changed":
        if (!event.granted.includes("ipc.serve")) saveClients([]);
        break;
    }
  },

  ui_render({ view }) {
    return { tree: view === "panel" ? panel() : [] };
  },

  ui_event({ id, entry }) {
    if (id === "drop" && entry) {
      const connection = Number(entry.slice(1));
      host.ipcServe?.close(connection);
      saveClients(clients().filter(client => client.connection !== connection));
    }
    return { tree: panel() };
  },
});
