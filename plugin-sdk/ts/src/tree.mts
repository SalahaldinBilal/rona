// What a plugin view may hold, the same rules as plugin-sdk/rust/src/tree.rs. Keep them in step.

export const MAX_NODES = 300;
export const MAX_STRING = 2000;
const MAX_SELECT_ITEMS = 200;
const MAX_SEGMENTS = 6;
const MAX_LIST_ENTRIES = 500;
const MAX_ROW_ITEMS = 4;
const MAX_ENTRY_BUTTONS = 2;

// "settings" is the manifest's static settings: groups, rows, labels and inputs, every input with an id.
export type TreeMode = "settings" | "view";

const INPUTS = ["toggle", "select", "segmented", "slider", "text", "color"];
const ITEMS = ["label", "badge", "button", "toggle", "select", "segmented", "slider", "text", "color", "progress", "image", "link"];
const TONES = ["", "normal", "muted", "accent", "success", "danger"];
const ITEM_ID = /^[A-Za-z0-9_-]{1,64}$/;

type Node = Record<string, unknown>;

function fail(message: string): never {
  throw new Error(message);
}

function object(node: unknown): Node {
  if (typeof node !== "object" || node === null || Array.isArray(node)) fail("Every node must be an object");
  return node as Node;
}

function kind(node: Node): string {
  if (typeof node.type !== "string") fail("A node has no type");
  return node.type;
}

function children(node: Node): unknown[] {
  if (node.children === undefined) return [];
  if (!Array.isArray(node.children)) fail("children must be a list");
  return node.children;
}

function checkStrings(value: unknown): void {
  if (typeof value === "string") {
    if ([...value].length > MAX_STRING) fail(`A text is longer than ${MAX_STRING} characters`);
  } else if (Array.isArray(value)) {
    value.forEach(checkStrings);
  } else if (typeof value === "object" && value !== null) {
    Object.values(value).forEach(checkStrings);
  }
}

class Checker {
  count = 0;
  ids = new Set<string>();
  mode: TreeMode;

  constructor(mode: TreeMode) {
    this.mode = mode;
  }

  counted(node: Node): void {
    this.count += 1;
    if (this.count > MAX_NODES) fail(`More than ${MAX_NODES} nodes`);
    checkStrings(node);
    if (typeof node.tone === "string" && !TONES.includes(node.tone)) {
      fail(`Unknown tone "${node.tone}". Use normal, muted, accent, success or danger`);
    }
  }

  id(node: Node, nodeType: string, required: boolean): void {
    const id = node.id;
    if (id === undefined) {
      if (required) fail(`A "${nodeType}" node needs an id`);
      return;
    }
    if (typeof id !== "string") fail("A node id must be a string");
    if (!ITEM_ID.test(id)) fail(`Node id "${id}" is not valid. Use letters, digits, dashes and underscores`);
    if (this.ids.has(id)) fail(`Node id "${id}" is used twice`);
    this.ids.add(id);
  }

  top(value: unknown): void {
    const node = object(value);
    const type = kind(node);
    if (type === "group") this.group(node, true);
    else if (type === "label" || type === "separator" || type === "empty") this.simple(node);
    else if (type === "list" && this.mode === "view") this.list(node);
    else fail(`"${type}" can't be at the top of a view. Put it in a row inside a group`);
  }

  simple(node: Node): void {
    this.counted(node);
    this.id(node, kind(node), false);
  }

  group(node: Node, outer: boolean): void {
    this.counted(node);
    this.id(node, "group", false);
    for (const value of children(node)) {
      const child = object(value);
      const type = kind(child);
      if (type === "row") this.row(child);
      else if (type === "label" || type === "separator") this.simple(child);
      else if (type === "group" && outer) this.group(child, false);
      else if (type === "group") fail("Groups can only hold one more level of groups");
      else if (type === "list" && this.mode === "view") this.list(child);
      else fail(`A group can't hold "${type}" directly. Put it in a row`);
    }
  }

  row(node: Node): void {
    this.counted(node);
    this.id(node, "row", false);
    const items = children(node);
    if (items.length > MAX_ROW_ITEMS) fail(`A row can hold at most ${MAX_ROW_ITEMS} items`);
    for (const item of items) this.item(object(item));
  }

  item(node: Node): void {
    this.counted(node);
    const type = kind(node);
    if (!ITEMS.includes(type)) fail(`Unknown item "${type}"`);
    const input = INPUTS.includes(type);
    if (this.mode === "settings" && !input && type !== "label") fail(`"${type}" is not allowed in manifest settings`);
    this.id(node, type, input || type === "button");

    if (type === "select" || type === "segmented") {
      if (!Array.isArray(node.items)) fail("A choice needs a list of items");
      const limit = type === "select" ? MAX_SELECT_ITEMS : MAX_SEGMENTS;
      if (node.items.length === 0 || node.items.length > limit) fail(`A ${type} takes 1 to ${limit} items`);
      const complete = (item: unknown) => typeof item === "object" && item !== null && "value" in item && "label" in item;
      if (!node.items.every(complete)) fail("Every choice needs a value and a label");
    } else if (type === "slider") {
      const { min, max, step } = node;
      if (typeof min !== "number" || typeof max !== "number") fail("A slider needs min and max");
      if (!(min < max) || (typeof step === "number" && step <= 0)) fail("A slider needs min below max and a step above 0");
    } else if (type === "link") {
      if (typeof node.url !== "string" || !node.url.startsWith("https://")) fail("A link must be an https URL");
    }
  }

  list(node: Node): void {
    this.counted(node);
    this.id(node, "list", true);
    if (node.entries === undefined) return;
    if (!Array.isArray(node.entries)) fail("entries must be a list");
    if (node.entries.length > MAX_LIST_ENTRIES) fail(`A list can hold at most ${MAX_LIST_ENTRIES} entries`);
    const entryIds = new Set<string>();
    for (const value of node.entries) {
      const entry = object(value);
      checkStrings(entry);
      const id = entry.id;
      if (typeof id !== "string") fail("Every list entry needs an id");
      if (!ITEM_ID.test(id) || entryIds.has(id)) fail(`List entry id "${id}" is not valid or used twice`);
      entryIds.add(id);
      const buttons = Array.isArray(entry.buttons) ? entry.buttons.length : 0;
      if (buttons > MAX_ENTRY_BUTTONS) fail(`A list entry can have at most ${MAX_ENTRY_BUTTONS} buttons`);
    }
  }
}

// Throws with the reason when Rona would refuse the tree.
export function validateTree(nodes: unknown[], mode: TreeMode): void {
  const checker = new Checker(mode);
  for (const node of nodes) checker.top(node);
}
