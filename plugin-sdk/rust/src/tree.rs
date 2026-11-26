//! What a plugin view may hold. Rona draws the tree with its own controls, in three levels:
//! groups hold rows, rows hold up to 4 items.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::manifest::{is_valid_item_id, validate_tone};

pub const MAX_NODES: usize = 300;
pub const MAX_STRING: usize = 2000;
pub const MAX_SELECT_ITEMS: usize = 200;
pub const MAX_SEGMENTS: usize = 6;
pub const MAX_LIST_ENTRIES: usize = 500;
pub const MAX_ROW_ITEMS: usize = 4;
pub const MAX_ENTRY_BUTTONS: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The manifest's static settings: groups, rows, labels and inputs, every input with an id.
    Settings,
    View,
}

const INPUTS: &[&str] = &["toggle", "select", "segmented", "slider", "text", "color"];
const ITEMS: &[&str] = &[
    "label", "badge", "button", "toggle", "select", "segmented", "slider", "text", "color", "progress", "image", "link",
];

struct Checker {
    mode: Mode,
    count: usize,
    ids: HashSet<String>,
}

pub fn validate_tree(nodes: &[Value], mode: Mode) -> Result<(), String> {
    let mut checker = Checker { mode, count: 0, ids: HashSet::new() };
    for node in nodes {
        checker.top(node)?;
    }
    Ok(())
}

fn object(node: &Value) -> Result<&Map<String, Value>, String> {
    node.as_object().ok_or_else(|| "Every node must be an object".to_string())
}

fn kind(map: &Map<String, Value>) -> Result<&str, String> {
    map.get("type").and_then(Value::as_str).ok_or_else(|| "A node has no type".to_string())
}

fn children(map: &Map<String, Value>) -> Result<&[Value], String> {
    match map.get("children") {
        None => Ok(&[]),
        Some(Value::Array(children)) => Ok(children),
        Some(_) => Err("children must be a list".into()),
    }
}

impl Checker {
    fn count(&mut self, map: &Map<String, Value>) -> Result<(), String> {
        self.count += 1;
        if self.count > MAX_NODES {
            return Err(format!("More than {MAX_NODES} nodes"));
        }
        check_strings(&Value::Object(map.clone()))?;
        if let Some(tone) = map.get("tone").and_then(Value::as_str) {
            validate_tone(tone)?;
        }
        Ok(())
    }

    fn id(&mut self, map: &Map<String, Value>, node_type: &str, required: bool) -> Result<(), String> {
        match map.get("id") {
            Some(Value::String(id)) => {
                if !is_valid_item_id(id) {
                    return Err(format!("Node id \"{id}\" is not valid. Use letters, digits, dashes and underscores"));
                }
                if !self.ids.insert(id.clone()) {
                    return Err(format!("Node id \"{id}\" is used twice"));
                }
                Ok(())
            }
            Some(_) => Err("A node id must be a string".into()),
            None if required => Err(format!("A \"{node_type}\" node needs an id")),
            None => Ok(()),
        }
    }

    fn top(&mut self, node: &Value) -> Result<(), String> {
        let map = object(node)?;
        match kind(map)? {
            "group" => self.group(map, true),
            "label" | "separator" | "empty" => self.simple(map),
            "list" if self.mode == Mode::View => self.list(map),
            other => Err(format!("\"{other}\" can't be at the top of a view. Put it in a row inside a group")),
        }
    }

    fn simple(&mut self, map: &Map<String, Value>) -> Result<(), String> {
        self.count(map)?;
        let node_type = kind(map)?;
        self.id(map, node_type, false)
    }

    fn group(&mut self, map: &Map<String, Value>, outer: bool) -> Result<(), String> {
        self.count(map)?;
        self.id(map, "group", false)?;
        for child in children(map)? {
            let child = object(child)?;
            match kind(child)? {
                "row" => self.row(child)?,
                "label" | "separator" => self.simple(child)?,
                "group" if outer => self.group(child, false)?,
                "group" => return Err("Groups can only hold one more level of groups".into()),
                "list" if self.mode == Mode::View => self.list(child)?,
                other => return Err(format!("A group can't hold \"{other}\" directly. Put it in a row")),
            }
        }
        Ok(())
    }

    fn row(&mut self, map: &Map<String, Value>) -> Result<(), String> {
        self.count(map)?;
        self.id(map, "row", false)?;
        let items = children(map)?;
        if items.len() > MAX_ROW_ITEMS {
            return Err(format!("A row can hold at most {MAX_ROW_ITEMS} items"));
        }
        for item in items {
            self.item(object(item)?)?;
        }
        Ok(())
    }

    fn item(&mut self, map: &Map<String, Value>) -> Result<(), String> {
        self.count(map)?;
        let item_type = kind(map)?;
        if !ITEMS.contains(&item_type) {
            return Err(format!("Unknown item \"{item_type}\""));
        }
        let input = INPUTS.contains(&item_type);
        if self.mode == Mode::Settings && !input && item_type != "label" {
            return Err(format!("\"{item_type}\" is not allowed in manifest settings"));
        }
        self.id(map, item_type, input || item_type == "button")?;

        match item_type {
            "select" | "segmented" => {
                let items = map.get("items").and_then(Value::as_array).ok_or("A choice needs a list of items")?;
                let limit = if item_type == "select" { MAX_SELECT_ITEMS } else { MAX_SEGMENTS };
                if items.is_empty() || items.len() > limit {
                    return Err(format!("A {item_type} takes 1 to {limit} items"));
                }
                if items.iter().any(|item| item.get("value").is_none() || item.get("label").is_none()) {
                    return Err("Every choice needs a value and a label".into());
                }
            }
            "slider" => {
                let number = |key: &str| map.get(key).and_then(Value::as_f64);
                let (Some(min), Some(max)) = (number("min"), number("max")) else {
                    return Err("A slider needs min and max".into());
                };
                if !(min < max) || number("step").is_some_and(|step| step <= 0.0) {
                    return Err("A slider needs min below max and a step above 0".into());
                }
            }
            "link" => {
                let url = map.get("url").and_then(Value::as_str).unwrap_or_default();
                if !url.starts_with("https://") {
                    return Err("A link must be an https URL".into());
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn list(&mut self, map: &Map<String, Value>) -> Result<(), String> {
        self.count(map)?;
        self.id(map, "list", true)?;
        let entries = match map.get("entries") {
            None => return Ok(()),
            Some(Value::Array(entries)) => entries,
            Some(_) => return Err("entries must be a list".into()),
        };
        if entries.len() > MAX_LIST_ENTRIES {
            return Err(format!("A list can hold at most {MAX_LIST_ENTRIES} entries"));
        }
        let mut entry_ids = HashSet::new();
        for entry in entries {
            let entry = object(entry)?;
            check_strings(&Value::Object(entry.clone()))?;
            let id = entry.get("id").and_then(Value::as_str).ok_or("Every list entry needs an id")?;
            if !is_valid_item_id(id) || !entry_ids.insert(id.to_owned()) {
                return Err(format!("List entry id \"{id}\" is not valid or used twice"));
            }
            let buttons = entry.get("buttons").and_then(Value::as_array).map_or(0, Vec::len);
            if buttons > MAX_ENTRY_BUTTONS {
                return Err(format!("A list entry can have at most {MAX_ENTRY_BUTTONS} buttons"));
            }
        }
        Ok(())
    }
}

fn check_strings(value: &Value) -> Result<(), String> {
    match value {
        Value::String(text) if text.chars().count() > MAX_STRING => {
            Err(format!("A text is longer than {MAX_STRING} characters"))
        }
        Value::Array(items) => items.iter().try_for_each(check_strings),
        Value::Object(map) => map.values().try_for_each(check_strings),
        _ => Ok(()),
    }
}

pub fn find_node<'a>(nodes: &'a [Value], id: &str) -> Option<&'a Value> {
    nodes.iter().find_map(|node| {
        if node.get("id").and_then(Value::as_str) == Some(id) {
            return Some(node);
        }
        node.get("children").and_then(Value::as_array).and_then(|children| find_node(children, id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(tree: Value) -> Result<(), String> {
        validate_tree(tree.as_array().unwrap(), Mode::View)
    }

    #[test]
    fn a_settings_page_like_the_ones_plugins_write_passes() {
        let tree = json!([{ "type": "group", "title": "Account", "children": [
            { "type": "row", "label": "Token", "children": [{ "type": "text", "id": "token", "secret": true }] },
            { "type": "row", "label": "Private", "children": [{ "type": "toggle", "id": "private", "value": false }] },
            { "type": "row", "children": [
                { "type": "button", "id": "sign-in", "label": "Sign in", "primary": true },
                { "type": "link", "label": "Help", "url": "https://example.com/help" },
            ]},
        ]}]);
        assert!(view(tree).is_ok());
    }

    #[test]
    fn items_must_sit_in_rows() {
        assert!(view(json!([{ "type": "button", "id": "go", "label": "Go" }])).is_err());
        assert!(view(json!([{ "type": "group", "children": [{ "type": "toggle", "id": "a" }] }])).is_err());
    }

    #[test]
    fn a_row_holds_at_most_four_items() {
        let items: Vec<Value> = (0..5).map(|index| json!({ "type": "badge", "text": index.to_string() })).collect();
        assert!(view(json!([{ "type": "group", "children": [{ "type": "row", "children": items }] }])).is_err());
    }

    #[test]
    fn groups_nest_one_level() {
        let deep = json!([{ "type": "group", "children": [{ "type": "group", "children": [{ "type": "group" }] }] }]);
        assert!(view(deep).is_err());
    }

    #[test]
    fn too_many_nodes_are_refused() {
        let nodes: Vec<Value> = (0..=MAX_NODES).map(|_| json!({ "type": "label", "text": "x" })).collect();
        assert!(validate_tree(&nodes, Mode::View).is_err());
    }

    #[test]
    fn oversized_text_is_refused_wherever_it_hides() {
        let long = "x".repeat(MAX_STRING + 1);
        let in_choice = json!([{ "type": "group", "children": [{ "type": "row", "children": [
            { "type": "select", "id": "a", "items": [{ "value": "v", "label": long }] },
        ]}]}]);
        assert!(view(in_choice).is_err());
        let in_entry = json!([{ "type": "list", "id": "found", "entries": [{ "id": "a", "title": long }] }]);
        assert!(view(in_entry).is_err());
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let tree = json!([{ "type": "group", "children": [{ "type": "row", "children": [
            { "type": "toggle", "id": "a" }, { "type": "button", "id": "a", "label": "Two" },
        ]}]}]);
        assert!(view(tree).is_err());
    }

    #[test]
    fn links_must_be_https() {
        let link = |url: &str| json!([{ "type": "group", "children": [{ "type": "row", "children": [
            { "type": "link", "label": "Site", "url": url },
        ]}]}]);
        assert!(view(link("http://example.com")).is_err());
        assert!(view(link("javascript:alert(1)")).is_err());
        assert!(view(link("https://example.com")).is_ok());
    }

    #[test]
    fn manifest_settings_only_take_inputs_with_ids() {
        let button = json!([{ "type": "group", "children": [{ "type": "row", "children": [
            { "type": "button", "id": "go", "label": "Go" },
        ]}]}]);
        let no_id = json!([{ "type": "group", "children": [{ "type": "row", "children": [{ "type": "toggle" }] }] }]);
        assert!(validate_tree(button.as_array().unwrap(), Mode::Settings).is_err());
        assert!(validate_tree(no_id.as_array().unwrap(), Mode::Settings).is_err());
    }
}
