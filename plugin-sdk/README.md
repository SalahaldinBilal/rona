# Rona plugins

Plugins add things to Rona: buttons in the control bar, a side panel, menu entries, keys, text on the video, ways to open links, and ways for other programs on the computer to control playback. A plugin is a WebAssembly module, written in Rust or TypeScript, that Rona runs in a sandbox. It can't read files, start programs or reach the network on its own, only the ones its manifest names and the user allows. Everything goes through Rona, which checks the user's permission switches on every call.

This folder holds:

- `rust/`: the Rust SDK (crate `rona-plugin`) and its packer, `rona-plugin-pack`.
- `ts/`: the TypeScript SDK (package `@rona-player/plugin`) and the same packer.
- `examples/ab-loop/`: a Rust plugin with a control bar button, a key action and a setting.
- `examples/remote-control/`: a TypeScript plugin that takes commands from other programs and shows them in a side panel.

## Contents

- [Quick start](#quick-start)
- [The package](#the-package)
- [API versions](#api-versions)
- [plugin.json](#pluginjson)
- [Permissions](#permissions)
- [Hooks](#hooks)
- [Events](#events)
- [What a plugin can call](#what-a-plugin-can-call)
- [Views](#views)
- [Control bar, menus and keys](#control-bar-menus-and-keys)
- [Talking to other programs](#talking-to-other-programs)
- [Asking Rona to install a plugin](#asking-rona-to-install-a-plugin)
- [Limits](#limits)

## Quick start

### Rust

```sh
rustup target add wasm32-unknown-unknown
cargo install --path plugin-sdk/rust   # or from crates.io once published
```

`Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
rona-plugin = { version = "1", default-features = false }
```

`src/lib.rs`:

```rust
use rona_plugin::{Event, HookResult, export_hooks, host};

fn event(event: Event) -> HookResult<()> {
    if let Event::Pause { paused } = event {
        // None while the user has ui.osd turned off. The plugin goes on without it.
        if let Some(osd) = host::osd() {
            osd.show(if paused { "Paused" } else { "Playing" })?;
        }
    }
    Ok(())
}

export_hooks! {
    on_event => event,
}
```

Then, in the plugin's folder, `rona-plugin-pack` builds `plugin.wasm` and writes `<id>-<version>.rp`.

### TypeScript

```sh
npm install @rona-player/plugin
```

`src/index.ts`:

```ts
import { definePlugin, host } from "@rona-player/plugin";

export default definePlugin({
  on_event(event) {
    if (event.type === "pause") {
      // undefined while the user has ui.osd turned off.
      host.osd?.show(event.paused ? "Paused" : "Playing");
    }
  },
});
```

Then `npx rona-plugin-pack`. The first build downloads `extism-js` and Binaryen into a cache folder and checks them against pinned hashes.

### Trying it

In Rona, Settings > Plugins > Load from folder runs the plugin straight from its folder. "Reload from folder" on its page picks up a new build. The page also shows the plugin's log, and the same lines go to Rona's log file as `plugin/<id>`.

## The package

A `.rp` file is a zip holding `plugin.json`, `plugin.wasm` and, if you like, an icon: `icon.webp`, or `icon.png`, `icon.jpg` or `icon.jpeg` if you have one of those already. It's at most 256 KB, square, and shown at up to 40 px, so 64 by 64 is plenty. The packer refuses a folder with more than one icon, or an icon whose content doesn't match its name. Other files in the package are ignored. Opening a `.rp` file, dropping it on Rona's window or choosing Settings > Plugins > Install from file shows the install dialog. Updating means installing a newer `.rp` with the same id. The user's switches carry over, and permissions the new version adds are marked as new.

Rona hashes `plugin.wasm` when it installs a plugin and checks the hash every time it loads it. A changed file leaves the plugin off. Plugins loaded from a folder skip this check.

## API versions

The plugin API has a major and a minor version, now `1.0`. A minor version only adds things: host functions, events, view nodes. A major version is for changes that break plugins written for the one before.

A plugin names the version it's written for in `apiVersion`. Rona runs it when the major is the same as its own and the minor is the same or lower, so a `1.0` plugin runs on a Rona with API `1.3`, and a `1.3` plugin doesn't run on a Rona with `1.0`. That Rona says it needs an update. Set `apiVersion` to the lowest version that has everything the plugin uses, so it runs on as many versions of Rona as it can.

Each SDK release has the plugin API of its own major and minor version: SDK `1.2.x` has API `1.2`. The packer refuses an `apiVersion` newer than its SDK's. A bare `1`, which SDK 1.0.0 wrote, counts as `1.0`.

## plugin.json

```json
{
  "apiVersion": "1.0",
  "id": "com.example.lyrics",
  "name": "Lyrics",
  "version": "1.2.0",
  "author": "Someone",
  "description": "Shows the lyrics of what plays.",
  "homepage": "https://example.com/lyrics",
  "minAppVersion": "0.2.0",
  "permissions": {
    "playback.read": "Follows the song to scroll the lyrics",
    "media.metadata": "Looks lyrics up by title and artist",
    "ui.panel": "Shows the lyrics",
    "network": { "reason": "Downloads the lyrics", "hosts": ["api.lyrics.example", "*.cdn.lyrics.example"] }
  },
  "hooks": ["on_event", "ui_render", "ui_event"],
  "ui": {
    "panel": { "title": "Lyrics", "stroke": "M 3 4 H 15 M 3 8 H 15 M 3 12 H 11" },
    "controls": [],
    "menu": [],
    "rowMenu": [],
    "dialogs": [{ "id": "search", "title": "Find lyrics" }]
  },
  "actions": [],
  "settings": []
}
```

| Key | |
|---|---|
| `apiVersion` | The plugin API the plugin is written for, like `"1.0"`. See [API versions](#api-versions). |
| `id` | 3 to 64 lowercase letters, digits, dots and dashes, like `com.example.lyrics`. It names the plugin's folder and storage, so it never changes. |
| `name`, `version` | Up to 64 characters, and a semver version. |
| `author`, `description`, `homepage` | Shown in the install dialog and on the plugin's page. `homepage` must be https. |
| `minAppVersion` | The oldest Rona the plugin runs on. |
| `permissions` | See [Permissions](#permissions). Each one has a reason the user reads, up to 200 characters. |
| `hooks` | The exports Rona may call. See [Hooks](#hooks). |
| `ui` | The panel, control bar controls, menu entries and dialogs. See [Views](#views) and [Control bar, menus and keys](#control-bar-menus-and-keys). |
| `actions` | Up to 16 named actions the user can bind to keys on the Shortcuts page. |
| `settings` | Settings Rona shows and stores itself, without calling the plugin. A view tree of groups and rows holding `toggle`, `select`, `segmented`, `slider`, `text` and `color` items, each with an `id` and a `value` as its default. Read them with `host::setting` / `host.setting`. |

## Permissions

Every permission is a switch. The install dialog shows them all on, the user turns off what they don't want, and they can change them any time in Settings > Plugins. Plan for any of them to be off:

- In TypeScript, a part of `host` is `undefined` while its permission is off, and the types mark it optional: `host.playback?.state()`.
- In Rust, it comes from a function returning `Option`: `if let Some(playback) = host::playback() { ... }`.
- Each hook call sees the switches as they are right then. Don't keep the object between calls.
- A call made after its permission was turned off (during the same hook) fails with `HostError::Denied` / `PermissionDenied` instead of ending the hook.
- Events that need a permission stop arriving while it's off.
- Turning a switch on or off sends a `permissions-changed` event with the new list.

| Permission | Category | Lets the plugin |
|---|---|---|
| `playback.read` | Playback | Read state, position, duration, speed and volume, and get playback events. |
| `playback.control` | Playback | Play, pause, seek, change speed and volume, mute, go to the next or previous file, stop. |
| `media.metadata` | Media | Read the title, tags, codecs, size and chapters. Adds titles to events. |
| `media.path` | Media | See file paths and links of what plays and of the playlist. Kept apart since paths say a lot about the user. |
| `media.tracks` | Media | List and switch audio and subtitle tracks. |
| `media.subtitles` | Media | Add a subtitle from bytes the plugin hands over. |
| `media.file` | Media | Read parts of the playing file, up to 4 MiB a call. Local files only. |
| `open.media` | Opening | Open paths or links, replacing what plays or adding to the playlist. |
| `open.resolve` | Opening | Get the `resolve_link` call for links Rona can't tell are media, and return a playable link. |
| `playlist.read` | Playlist | Read the playlist as the user sees it. |
| `playlist.edit` | Playlist | Remove, move and play entries. Removals can be undone by the user like any other. |
| `mpv.read` | mpv | Read and follow mpv properties from an allowlist. Properties holding paths also need `media.path`. |
| `mpv.write` | mpv | Set mpv properties from an allowlist. |
| `mpv.command` | mpv | Run mpv commands from an allowlist. |
| `ui.osd` | Interface | Show OSD messages. |
| `ui.overlay` | Interface | Draw ASS text on the video. |
| `ui.panel` | Interface | Have a side panel. |
| `ui.controls` | Interface | Add controls to the control bar. |
| `ui.menu` | Interface | Add entries to the Plugins menu and to the playlist's right-click menu. |
| `input.actions` | Interface | Have actions the user can bind to keys. |
| `clipboard.read`, `clipboard.write` | Clipboard | Read or write text. |
| `network` with `hosts` | Network | https to those hosts. Each host is its own switch. `*.example.com` covers subdomains, not `example.com` itself. |
| `ipc.serve` | Other programs | Take connections from programs on this computer. See [Talking to other programs](#talking-to-other-programs). |
| `ipc.connect` with `pipes` | Other programs | Connect to those named pipes or sockets, like Discord's `discord-ipc-0`. Each pipe is its own switch. |
| `system.run` with `programs` | Other programs | Run those programs, like `yt-dlp` or `ffprobe`, and read what they print. Each program is its own switch. |

`system.run` programs are bare names, looked up on PATH the way a terminal would, so a plugin can't run a file it wrote somewhere itself. They run without a shell, so arguments are passed as they are and nothing in them is read as shell syntax. There's no console window. The working folder is the temp folder. The program is still a normal program with the user's rights, so it's the most powerful permission a plugin can ask for, and the install dialog shows it like any other.

A program can be run to its end (`run`) or started and talked to while it runs (`start`). Anything a program starts itself ends with it: when it exits, is stopped or is closed, Rona ends its whole job on Windows or its process group elsewhere. A started program is stopped when the plugin stops or the user turns its switch off.

The plugin's own storage and its settings aren't permissions. They're private to the plugin.

Some mpv commands and options reach outside the player (`run`, `subprocess`, `load-script`, `input-ipc-server`, `screenshot-to-file`, `af` with lavfi's `amovie`, and others), so `mpv.write` and `mpv.command` only take what is on an allowlist in Rona's `src/plugins/mpv_allow.rs`. Commands: `seek`, `revert-seek`, `frame-step`, `frame-back-step`, `ab-loop`, `sub-seek`, `sub-step`, `show-text`, `show-progress`, and `set`, `add`, `cycle`, `cycle-values`, `multiply` on writable properties. Writable properties include `pause`, `speed`, `volume`, `mute`, `aid`, `sid`, `sub-delay`, `audio-delay`, `sub-pos`, `sub-scale`, `ab-loop-a`, `ab-loop-b`, `time-pos`, `chapter`, `video-zoom`, `video-pan-x`, `video-pan-y`, `video-rotate`, `video-aspect-override`, the picture adjustments and `loop-file`.

## Hooks

Exports Rona calls, each listed in `hooks`. A hook has 2 seconds (20 for `resolve_link`), not counting time spent waiting on the network, opening a pipe or running a program. A plugin that runs out of time 3 times in a row is paused until the user turns it off and on.

| Hook | Input | Returns |
|---|---|---|
| `on_event` | An [event](#events) | Nothing |
| `ui_render` | `{ view }` | A view response |
| `ui_event` | `{ view, id, type, value, entry }` | A view response |
| `resolve_link` | `{ url }` | `{ url, headers, title }` or `null`. Needs `open.resolve`. |
| `ipc_accept` | `{ connection, pid, program, channel }` | `true` to take the connection. Needs `ipc.serve`. Without this hook every connection is taken. |

A plugin has no thread of its own and runs only during hooks. Use timers for anything that repeats.

`ui_render` and `ui_event` go together. A view response:

```json
{
  "tree": [ ... ],
  "actions": [{ "id": "save", "label": "Save", "primary": true }],
  "osd": "Saved",
  "close": true,
  "open": "search",
  "refreshAfterMs": 1000
}
```

All fields are optional. `actions` are a dialog's buttons, after Rona's own Close (up to 3). `osd` needs `ui.osd`. `close` closes the dialog or popover. `open` opens one of the manifest's dialogs. `refreshAfterMs` asks for `ui_render` again while the view stays open, at least 500 ms later.

`resolve_link` runs on links without a media file extension, before Rona checks them itself. The first plugin that returns a link wins. The link must be http or https. `headers` are sent with every request for it (like `Referer` or `Cookie`), and `title` becomes the playlist entry's title.

## Events

`on_event` gets one object with a `type`:

| Type | Fields | Needs |
|---|---|---|
| `started` | | |
| `stopping` | | |
| `permissions-changed` | `granted` | |
| `file-started` | `path` (null without `media.path`) | `playback.read` |
| `file-loaded` | `path`, `title` (null without `media.metadata`), `duration` | `playback.read` |
| `file-ended` | `reason`: `eof`, `stop`, `quit`, `error` or `other` | `playback.read` |
| `idle` | | `playback.read` |
| `pause` | `paused` | `playback.read` |
| `seek` | `position` | `playback.read` |
| `position` | `position`, `duration` | `playback.read`, and asked for with `position_events` |
| `volume` | `volume` (0 to 1), `muted` | `playback.read` |
| `speed` | `speed` | `playback.read` |
| `playlist-changed` | | `playlist.read` |
| `tracks-changed` | | `media.tracks` |
| `chapter-changed` | `chapter`, `title` | `media.metadata` |
| `action` | `id` | `input.actions` |
| `menu` | `id`, `entry` (for playlist row entries: `index`, `title`, `current`, `path` with `media.path`) | `ui.menu` |
| `control` | `id`, `button`: `left` or `right` | `ui.controls` |
| `timer` | `id` | |
| `property` | `name`, `value` | `mpv.read`, for properties followed with `observe` |
| `ipc-connected` | `connection`, `client` | `ipc.serve` |
| `ipc-message` | `connection`, `message` | `ipc.serve` |
| `ipc-closed` | `connection` | `ipc.serve` |
| `pipe-data` | `connection` | `ipc.connect` |
| `pipe-closed` | `connection` | `ipc.connect` |
| `process-output` | `process` | `system.run` |
| `process-exited` | `process`, `status` (null when it was stopped) | `system.run` |

`pipe-data` and `process-output` come once until the plugin reads, however much arrives in between. If one is dropped because the queue is full, the next bytes send another.

Events queue up while the plugin is busy, up to 64. A full queue drops new events and logs a line, except `position` events, which are dropped without one.

`action`, `menu` and `control` events count as something the user did, so a plugin may open a dialog while handling them.

## What a plugin can call

Rust names are in `rona_plugin::host`, TypeScript names are on `host`. Calls that need a permission come from a function that returns `None` / `undefined` while it's off.

| Rust | TypeScript | |
|---|---|---|
| `log(level, text)` | `log(level, text)` | |
| `storage::{get, get_text, set, delete, keys}` | `storage.{get, getText, set, delete, keys}` | The plugin's own storage, 256 MB in all, 64 MB per value. |
| `setting(id)` | `setting(id)` | A static setting's value, or its default. |
| `timers::{set, clear}` | `timers.{set, clear}` | At least 250 ms, up to 16 timers. |
| `ui::{update, patch, image, open_dialog}` | `ui.{update, patch, image, openDialog}` | See [Views](#views). |
| `playback()?.{state, position_events}` | `playback?.{state, positionEvents}` | |
| `playback_control()?` | `playbackControl?` | `play`, `pause`, `toggle_pause`, `seek_to`, `seek_by`, `set_speed`, `set_volume`, `set_muted`, `next`, `previous`, `stop`. |
| `media()?.info()` | `media?.info()` | Title, artist, album, codecs, size, chapters, all tags. |
| `tracks()?.{list, select}` | `tracks?.{list, select}` | |
| `subtitles()?.add(name, language, extension, bytes)` | `subtitles?.add(...)` | `srt`, `ass`, `ssa`, `vtt`, `sub`, `sup`, `idx`, `smi` or `lrc`, up to 16 MB. |
| `current_file()?.{size, read}` | `currentFile?.{size, read}` | |
| `opener()?.{open, append}` | `opener?.{open, append}` | |
| `playlist()?.entries()` | `playlist?.entries()` | |
| `playlist_edit()?.{remove, move_entry, play}` | `playlistEdit?.{remove, move, play}` | |
| `mpv_read()?.{get, observe}` | `mpvRead?.{get, observe}` | Up to 32 followed properties. |
| `mpv_write()?.set` | `mpvWrite?.set` | |
| `mpv_command()?.run` | `mpvCommand?.run` | |
| `osd()?.show` | `osd?.show` | At most one a second, 200 characters. |
| `overlay()?.{set, clear}` | `overlay?.{set, clear}` | ASS events on a 1280 by 720 canvas. Ids 1 to 16. Cleared when the plugin stops or loses the permission. |
| `controls()?.set(id, state)` | `controls?.set(id, state)` | See [Control bar](#control-bar-menus-and-keys). |
| `menu()?.set(id, state)` | `menu?.set(id, state)` | `label`, `checked`, `visible`. |
| `clipboard_read()?.text()`, `clipboard_write()?.text(...)` | `clipboardRead?.text()`, `clipboardWrite?.text(...)` | |
| `network()?.send(request)` | `network?.send(request)` | https only, to hosts that are on, no private addresses. Every redirect is checked again. 32 MB per response, 30 s by default, 5 minutes at most. |
| `ipc_serve()?.{send, close}` | `ipcServe?.{send, close}` | |
| `pipes()?.{connect, write, read, close}` | `pipes?.{connect, write, read, close}` | Up to 4 open. |
| `programs()?.run(program, args, stdin, timeout_ms)` | `programs?.run(program, args, { stdin, timeoutMs })` | Waits for the program and returns its exit code, stdout and stderr. 60 s by default, 10 minutes at most, then it's killed. Up to 32 MB of each output is kept, and up to 256 arguments. |
| `programs()?.{start, write, read_stdout, read_stderr, end_input, close}` | `programs?.{start, write, readStdout, readStderr, endInput, close}` | Starts a program and returns its id at once. It runs with no time limit until it exits or is closed. Needs `on_event`, which gets its output events. Up to 4 at a time, and ones that ended count until closed (or until a new start needs the room). A program that prints 4 MB the plugin hasn't read is stopped. |

Calls that fail for another reason (a bad argument, a full storage, a network error) return an error the plugin can handle.

## Views

A plugin never draws. It returns a tree of nodes, and Rona draws it with its own controls in the user's theme. A plugin can't set colors, fonts or sizes, only a `tone`: `normal`, `muted`, `accent`, `success` or `danger`.

Views:

| View | Where | Needs |
|---|---|---|
| `settings` | The plugin's page in Settings > Plugins, under the static settings. | `ui_render` |
| `panel` | A side panel, 320 px wide, sharing the right side with the playlist and the recorder. Opened from a button the panel adds to the control bar. | `ui.panel`, `ui.panel` in the manifest |
| `popover:<control id>` | A small view above a control bar control that has `"popover": true`. | `ui.controls` |
| `dialog:<dialog id>` | A dialog over the video, for a dialog listed in `ui.dialogs`. Only opens in answer to something the user did. | |

A tree is groups of rows, and rows hold up to 4 items:

```json
[
  { "type": "group", "title": "Account", "description": "Sign in to see your lists.", "children": [
    { "type": "row", "label": "Token", "children": [{ "type": "text", "id": "token", "secret": true }] },
    { "type": "row", "children": [
      { "type": "button", "id": "sign-in", "label": "Sign in", "primary": true },
      { "type": "link", "label": "Get a token", "url": "https://example.com/token" }
    ]}
  ]},
  { "type": "list", "id": "results", "entries": [
    { "id": "r1", "title": "Song", "subtitle": "Artist", "image": "cover1", "buttons": [{ "id": "use", "label": "Use" }] }
  ]}
]
```

At the top: `group`, `list`, `label`, `separator`, `empty`. In a group: `row`, `label`, `separator`, `list`, and one more level of `group`. In a row: up to 4 items.

| Node | Props | Events |
|---|---|---|
| `group` | `title`, `description`, `children` | |
| `row` | `label` (left, 150 px), `description` (under it), `children` | |
| `label` | `text`, `tone` | |
| `separator` | | |
| `empty` | `text` | |
| `badge` | `text`, `highlight` | |
| `button` | `id`, `label`, `primary`, `danger`, `ghost`, `enabled` | `click` |
| `toggle` | `id`, `label`, `value` | `change`, true or false |
| `select` | `id`, `value`, `items: [{ value, label }]` (up to 200), `enabled` | `change`, the chosen item's `value` |
| `segmented` | Like `select`, up to 6 items | `change` |
| `slider` | `id`, `value`, `min`, `max`, `step`, `unit` | `change` when the drag ends |
| `text` | `id`, `value`, `placeholder`, `secret`, `numeric`, `live`, `enabled` | `change` when Enter is pressed or the field loses focus, `submit` on Enter. With `live`, `change` 300 ms after the last key instead. |
| `color` | `id`, `value` (`#rrggbb` or `#rrggbbaa`) | `change` 300 ms after the last change |
| `progress` | `value` from 0 to 1, or none for a spinner | |
| `image` | `id`, `image` (an id given to `ui.image`), `width`, `height` (8 to 480) | `click` |
| `link` | `label`, `url` (https) | Opens the browser. No event. |
| `list` | `id`, `entries: [{ id, title, subtitle, image, buttons: [{ id, label }] }]` (up to 500, 2 buttons each) | `click` on a row (`id` is the list's) or a button (`id` is the button's), with the entry's id in `entry` |

Ids are 1 to 64 letters, digits, dashes and underscores, and unique in the tree. Inputs and buttons need one.

Updating a view:

- `ui_render` runs when it opens, and again after `refreshAfterMs`.
- `ui_event` can answer with a new tree.
- `ui.update(view, tree)` replaces the tree of an open view from any hook, for example on `position` events. Updates for a view that isn't open are dropped. A view gets at most 10 updates a second: one that comes sooner waits for the rest of that tenth of a second, and a newer one replaces it, so the newest tree always arrives.
- `ui.patch(view, [{ id, ...props }])` changes props of nodes by id.
- `ui.image(id, bytes)` stores a PNG, JPEG or WebP for `image` nodes and list entries. Images are scaled to at most 1024 px, 16 MB and 200 images per plugin.

Rows whose kind and items stay the same are updated in place, so a text field being typed in isn't rebuilt under the cursor.

## Control bar, menus and keys

`ui.controls` in the manifest, up to 3 per plugin counting the panel's button:

```json
{ "id": "loop", "kind": "button", "label": "A-B loop", "stroke": "M 3 9 H 15", "popover": false, "always": false }
```

| Field | |
|---|---|
| `kind` | `button` (an icon), `text` (up to 8 characters, like `12:30`) or `badge` (next to the HW and resolution badges, only while a file plays). |
| `label` | Shown in menus, settings and the Shortcuts page. |
| `fill`, `stroke` | The icon, as SVG path data on an 18 px box. Only path commands and numbers. |
| `text`, `tone`, `active`, `dot`, `highlight` | How it looks at first. `active` tints it like an open panel, `dot` adds a small dot, `tone: "danger"` makes it red. |
| `popover` | Opens the `popover:<id>` view instead of sending a `control` event. |
| `always` | Usable while nothing plays. Otherwise it's dimmed then, like Rona's own buttons. |

`host.controls.set(id, { text, active, visible, ... })` changes a control at runtime. Changes that come less than a tenth of a second apart are joined and shown together, so none is lost. A plugin can't add controls the manifest doesn't list. The user can hide and reorder every plugin control in Settings > Interface, and when the window is too narrow, the ones that don't fit move into the Plugins menu.

`ui.menu` entries go in the Plugins menu on the control bar, `ui.rowMenu` entries in the playlist's right-click menu. Up to 8 each, with an optional `checked`. A click sends a `menu` event.

Every action and every control shows on the Shortcuts page as "Plugin: Label". A bound key sends an `action` event, or works like a click on the control.

## Talking to other programs

Rona has one local endpoint for all plugins, so programs reach plugins without each plugin opening a port of its own:

- Windows: the named pipe `\\.\pipe\rona-plugins-<session id>`, where the session id is the Windows session's. Only the user running Rona can open it, and remote clients are refused.
- Linux and macOS: the Unix socket `rona-plugins.sock` in `XDG_RUNTIME_DIR`, or `rona-plugins.sock-<uid>` in the temp folder. Rona checks that the other side runs as the same user.

No network port is opened, so web pages can't reach it.

Every frame is a 4 byte little endian length, then that many bytes of UTF-8 JSON, at most 1 MiB. Browser native messaging uses the same framing.

1. The program connects and sends `{ "rona": 1, "plugin": "<plugin id>", "channel": "anything" }`.
2. Rona checks the plugin is installed, on and has `ipc.serve` on, and calls its `ipc_accept` hook (if it has one) with the program's path and process id from the system, and the channel the program sent.
3. Rona answers `{ "ok": true, "connection": 7 }`, or `{ "ok": false, "error": "..." }` and closes.
4. After that, each frame from the program is an `ipc-message` event, and the plugin answers with `ipcServe.send(connection, message)`. Closing either side sends `ipc-closed`.

A plugin takes up to 16 connections. A program that stops reading the plugin's messages gets dropped. Connections close when the plugin stops or loses `ipc.serve`.

From a script or a Stream Deck button:

```sh
rona --plugin-send com.rona.examples.remote-control "{\"cmd\":\"toggle\"}"
```

It prints the plugin's first answer (waiting up to 10 s) and exits with 0, or 2 when Rona isn't running or the plugin refused.

For a browser extension, register a native messaging host whose program runs `rona --native-messaging <plugin id>`. Browsers don't let the host's manifest pass arguments, so point it at a small script or batch file that does. Rona passes frames between the browser and the plugin unchanged. The extension's origin, which the browser passes as an argument, becomes the channel.

A plugin can also connect out to the pipes its manifest names under `ipc.connect`. On Windows `discord-ipc-0` means `\\.\pipe\discord-ipc-0`, elsewhere `discord-ipc-0` in `XDG_RUNTIME_DIR`. The bytes are raw, in whatever protocol the other program speaks: `pipe-data` says something arrived and `pipes.read` takes it.

## Asking Rona to install a plugin

A program can ship a `.rp` file with its own installer and ask Rona to install it. The user decides in the install dialog, which names the program that asked.

```sh
rona --install-plugin path/to/plugin.rp
```

It starts Rona if it isn't running, waits for the user's answer (up to 10 minutes), prints `installed`, `declined` or `failed: <reason>`, and exits with 0, 1 or 2.

Over the endpoint, send `{ "rona": 1, "install": "<absolute path>" }` as the first frame instead. The answer is `{ "ok": true, "result": "installed" }` or `"declined"`, or `{ "ok": false, "error": "..." }`.

Rona copies the file and hashes it before showing the dialog, and installs that copy, so the file can't be swapped after the user looked at it. A program that asks again for a plugin the user declined in the last 10 minutes gets `declined` without a dialog.

## Limits

| | |
|---|---|
| `plugin.json` | 256 KB |
| `plugin.wasm` | 64 MB, and 1 GB of memory while it runs |
| A view tree | 300 nodes, 2000 characters per text, 4 items per row, 500 list entries |
| Events waiting | 64 |
| Storage | 256 MB per plugin, 64 MB per value, keys up to 256 bytes |
| Images | 16 MB and 200 per plugin, 1024 px on the long side |
| Network | 32 MB per response, 5 redirects, up to 5 minutes |
| Pipes and connections | 4 outgoing pipes, 16 programs connected, 1 MiB per frame |
| Programs | 10 minutes per run, 32 MB of input and of each output, 256 arguments. 4 started at a time, 4 MB of unread output each |
| Hooks | 2 s, 20 s for `resolve_link` |
