import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { checkManifest } from "../src/pack.mts";

function example(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(new URL(`../../examples/${name}/plugin.json`, import.meta.url), "utf-8"));
}

const base = { apiVersion: 1, id: "com.example.test", name: "Test", version: "1.0.0" };

test("the example plugins pass the same rules Rona applies", () => {
  assert.deepEqual(checkManifest(example("ab-loop")), { id: "com.rona.examples.ab-loop", version: "1.0.0" });
  assert.deepEqual(checkManifest(example("remote-control")), { id: "com.rona.examples.remote-control", version: "1.0.0" });
});

test("a program to run must be a bare name, so a plugin can't run a file it wrote", () => {
  for (const program of ["C:\\Windows\\System32\\cmd.exe", "/bin/sh", "../tool", "sub/tool", "-rf", "a b"]) {
    const manifest = { ...base, permissions: { "system.run": { reason: "", programs: [program] } } };
    assert.throws(() => checkManifest(manifest), /not valid/, program);
  }
  checkManifest({ ...base, permissions: { "system.run": { reason: "Reads links", programs: ["yt-dlp", "ffprobe"] } } });
});

test("lists only go with the permission they belong to", () => {
  assert.throws(() => checkManifest({ ...base, permissions: { "system.run": "No list" } }), /needs a list of programs/);
  assert.throws(() => checkManifest({ ...base, permissions: { "playback.read": { programs: ["yt-dlp"] } } }), /Only system.run/);
  assert.throws(() => checkManifest({ ...base, permissions: { network: { hosts: ["127.0.0.1"] } } }), /not valid/);
});

test("a plugin that talks to other programs needs on_event to hear them", () => {
  assert.throws(() => checkManifest({ ...base, permissions: { "ipc.serve": "" } }), /on_event/);
});

test("settings Rona would refuse on install are refused when packing", () => {
  const settings = (children: unknown[]) => ({ ...base, settings: [{ type: "group", title: "General", children }] });
  const row = (item: unknown) => [{ type: "row", label: "Item", children: [item] }];
  assert.throws(() => checkManifest(settings(row({ type: "button", id: "go", label: "Go" }))), /not allowed in manifest settings/);
  assert.throws(() => checkManifest(settings(row({ type: "toggle" }))), /needs an id/);
  assert.throws(() => checkManifest(settings([{ type: "toggle", id: "loose" }])), /Put it in a row/);
  assert.throws(() => checkManifest(settings(row({ type: "slider", id: "gain", min: 10, max: 0 }))), /min below max/);
  assert.throws(() => checkManifest(settings([...row({ type: "toggle", id: "a" }), ...row({ type: "text", id: "a" })])), /used twice/);
  checkManifest(settings([
    ...row({ type: "segmented", id: "mode", items: [{ value: "a", label: "A" }, { value: "b", label: "B" }], value: "a" }),
    ...row({ type: "text", id: "token", secret: true }),
  ]));
});

test("pipes can't point at Rona's own endpoint", () => {
  const manifest = { ...base, hooks: ["on_event"], permissions: { "ipc.connect": { pipes: ["rona-plugins-1"] } } };
  assert.throws(() => checkManifest(manifest), /not valid/);
});
