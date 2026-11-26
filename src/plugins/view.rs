//! Turns a checked view tree into rows the Slint view draws. Slint can't build nested components from data, so
//! a tree is groups of rows of up to 4 items, flattened here into one list of rows.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::runtime::Image;

#[derive(Clone, Default)]
pub struct Item {
    pub kind: String,
    pub id: String,
    pub text: String,
    pub tone: i32,
    pub value: String,
    pub checked: bool,
    pub number: f32,
    pub minimum: f32,
    pub maximum: f32,
    pub step: f32,
    pub options: Vec<String>,
    pub values: Vec<Value>,
    pub index: i32,
    pub enabled: bool,
    pub primary: bool,
    pub danger: bool,
    pub ghost: bool,
    pub highlight: bool,
    pub secret: bool,
    pub numeric: bool,
    pub live: bool,
    pub placeholder: String,
    pub url: String,
    pub image: Option<Arc<Image>>,
    pub width: f32,
    pub height: f32,
    pub fraction: f32,
    pub indeterminate: bool,
}

#[derive(Clone, Default)]
pub struct Row {
    /// `heading`, `subheading`, `row`, `label`, `separator`, `empty` or `entry`.
    pub kind: &'static str,
    pub id: String,
    pub label: String,
    pub text: String,
    pub tone: i32,
    pub items: Vec<Item>,
    pub image: Option<Arc<Image>>,
    /// For list entries, the entry id. `id` is then the list's.
    pub entry: String,
}

pub fn tone(text: Option<&str>) -> i32 {
    match text {
        Some("muted") => 1,
        Some("accent") => 2,
        Some("success") => 3,
        Some("danger") => 4,
        _ => 0,
    }
}

fn string(map: &Map<String, Value>, key: &str) -> String {
    match map.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    }
}

fn number(map: &Map<String, Value>, key: &str, default: f64) -> f32 {
    map.get(key).and_then(Value::as_f64).unwrap_or(default) as f32
}

fn flag(map: &Map<String, Value>, key: &str, default: bool) -> bool {
    map.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn children(map: &Map<String, Value>) -> impl Iterator<Item = &Map<String, Value>> {
    map.get("children").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object)
}

type Images<'a> = &'a dyn Fn(&str) -> Option<Arc<Image>>;

fn item(map: &Map<String, Value>, images: Images) -> Item {
    let kind = string(map, "type");
    let mut item = Item {
        id: string(map, "id"),
        tone: tone(map.get("tone").and_then(Value::as_str)),
        enabled: flag(map, "enabled", true),
        primary: flag(map, "primary", false),
        danger: flag(map, "danger", false),
        ghost: flag(map, "ghost", false),
        highlight: flag(map, "highlight", false),
        secret: flag(map, "secret", false),
        numeric: flag(map, "numeric", false),
        live: flag(map, "live", false),
        placeholder: string(map, "placeholder"),
        url: string(map, "url"),
        ..Item::default()
    };
    match kind.as_str() {
        "label" | "badge" => item.text = string(map, "text"),
        "button" | "link" => item.text = string(map, "label"),
        "toggle" => {
            item.text = string(map, "label");
            item.checked = flag(map, "value", false);
        }
        "select" | "segmented" => {
            let choices = map.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let current = map.get("value").cloned().unwrap_or(Value::Null);
            item.options = choices.iter().map(|choice| choice.get("label").and_then(Value::as_str).unwrap_or_default().to_owned()).collect();
            item.values = choices.iter().map(|choice| choice.get("value").cloned().unwrap_or(Value::Null)).collect();
            item.index = item.values.iter().position(|value| *value == current).unwrap_or(0) as i32;
        }
        "slider" => {
            item.minimum = number(map, "min", 0.0);
            item.maximum = number(map, "max", 100.0);
            item.step = number(map, "step", 1.0);
            item.number = number(map, "value", item.minimum as f64);
            item.text = string(map, "unit");
        }
        "text" => item.value = string(map, "value"),
        "color" => item.value = string(map, "value"),
        "progress" => match map.get("value").and_then(Value::as_f64) {
            Some(fraction) => item.fraction = fraction.clamp(0.0, 1.0) as f32,
            None => item.indeterminate = true,
        },
        "image" => {
            item.image = images(&string(map, "image"));
            item.width = number(map, "width", 64.0).clamp(8.0, 480.0);
            item.height = number(map, "height", 64.0).clamp(8.0, 480.0);
        }
        _ => {}
    }
    item.kind = kind;
    item
}

fn push_row(rows: &mut Vec<Row>, map: &Map<String, Value>, images: Images) {
    rows.push(Row {
        kind: "row",
        id: string(map, "id"),
        label: string(map, "label"),
        text: string(map, "description"),
        items: children(map).map(|child| item(child, images)).collect(),
        ..Row::default()
    });
}

fn push_simple(rows: &mut Vec<Row>, map: &Map<String, Value>) {
    let kind = match string(map, "type").as_str() {
        "separator" => "separator",
        "empty" => "empty",
        _ => "label",
    };
    rows.push(Row {
        kind,
        id: string(map, "id"),
        text: string(map, "text"),
        tone: tone(map.get("tone").and_then(Value::as_str)),
        ..Row::default()
    });
}

fn push_list(rows: &mut Vec<Row>, map: &Map<String, Value>, images: Images) {
    let list_id = string(map, "id");
    for entry in map.get("entries").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
        let buttons = entry
            .get("buttons")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .map(|button| Item { kind: "button".into(), id: string(button, "id"), text: string(button, "label"), enabled: true, ghost: true, ..Item::default() })
            .collect();
        rows.push(Row {
            kind: "entry",
            id: list_id.clone(),
            entry: string(entry, "id"),
            label: string(entry, "title"),
            text: string(entry, "subtitle"),
            image: entry.get("image").and_then(Value::as_str).and_then(|id| images(id)),
            items: buttons,
            ..Row::default()
        });
    }
}

fn push_group(rows: &mut Vec<Row>, map: &Map<String, Value>, sub: bool, images: Images) {
    let title = string(map, "title");
    if !title.is_empty() {
        rows.push(Row { kind: if sub { "subheading" } else { "heading" }, id: string(map, "id"), label: title, ..Row::default() });
    }
    let description = string(map, "description");
    if !description.is_empty() {
        rows.push(Row { kind: "label", text: description, tone: 1, ..Row::default() });
    }
    for child in children(map) {
        match string(child, "type").as_str() {
            "row" => push_row(rows, child, images),
            "group" => push_group(rows, child, true, images),
            "list" => push_list(rows, child, images),
            _ => push_simple(rows, child),
        }
    }
}

/// `tree` must already be checked with `tree::validate_tree`.
pub fn rows(tree: &[Value], images: Images) -> Vec<Row> {
    let mut rows = Vec::new();
    for node in tree.iter().filter_map(Value::as_object) {
        match string(node, "type").as_str() {
            "group" => push_group(&mut rows, node, false, images),
            "list" => push_list(&mut rows, node, images),
            _ => push_simple(&mut rows, node),
        }
    }
    rows
}

/// Changes the props of the nodes `patches` name by id. Returns false when one of them names no node.
pub fn patch(tree: &mut [Value], patches: &[Value]) -> bool {
    let mut all_found = true;
    for patch in patches {
        let Some(id) = patch.get("id").and_then(Value::as_str) else { continue };
        let Some(props) = patch.as_object() else { continue };
        match find_mut(tree, id) {
            Some(node) => {
                for (key, value) in props {
                    if key != "id" && key != "type" {
                        node.insert(key.clone(), value.clone());
                    }
                }
            }
            None => all_found = false,
        }
    }
    all_found
}

fn holds(node: &Value, id: &str) -> bool {
    node.get("id").and_then(Value::as_str) == Some(id)
        || ["children", "entries"]
            .iter()
            .any(|key| node.get(*key).and_then(Value::as_array).is_some_and(|children| children.iter().any(|child| holds(child, id))))
}

// Finds where the node is first, then walks down to it, since a mutable search that backs out of a branch
// doesn't pass the borrow checker.
fn find_mut<'a>(nodes: &'a mut [Value], id: &str) -> Option<&'a mut Map<String, Value>> {
    let node = nodes.iter_mut().find(|node| holds(node, id))?;
    let map = node.as_object_mut()?;
    if map.get("id").and_then(Value::as_str) == Some(id) {
        return Some(map);
    }
    let key = ["children", "entries"]
        .into_iter()
        .find(|key| map.get(*key).and_then(Value::as_array).is_some_and(|children| children.iter().any(|child| holds(child, id))))?;
    match map.get_mut(key) {
        Some(Value::Array(children)) => find_mut(children, id),
        _ => None,
    }
}

/// The JSON value a change event carries, from what the Slint control reports.
pub fn event_value(item: &Item, reported: &str) -> Value {
    match item.kind.as_str() {
        "toggle" => Value::Bool(reported == "true"),
        "select" | "segmented" => {
            reported.parse::<usize>().ok().and_then(|index| item.values.get(index).cloned()).unwrap_or(Value::Null)
        }
        "slider" => reported.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map_or(Value::Null, Value::Number),
        "text" if item.numeric => reported.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map_or(Value::Null, Value::Number),
        _ => Value::String(reported.to_owned()),
    }
}

/// Sets the stored values of static settings into the manifest's settings tree before it is drawn.
pub fn with_values(tree: &[Value], value_of: &dyn Fn(&str) -> Option<Value>) -> Vec<Value> {
    fn fill(node: &mut Value, value_of: &dyn Fn(&str) -> Option<Value>) {
        let Some(map) = node.as_object_mut() else { return };
        if let Some(id) = map.get("id").and_then(Value::as_str).map(str::to_owned)
            && let Some(value) = value_of(&id)
        {
            map.insert("value".into(), value);
        }
        if let Some(Value::Array(children)) = map.get_mut("children") {
            for child in children {
                fill(child, value_of);
            }
        }
    }
    let mut tree = tree.to_vec();
    for node in &mut tree {
        fill(node, value_of);
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_images(_: &str) -> Option<Arc<Image>> {
        None
    }

    #[test]
    fn a_choice_reports_the_value_the_plugin_gave_not_its_place() {
        let tree = json!([{ "type": "group", "title": "Uploads", "children": [{ "type": "row", "label": "Keep for", "children": [
            { "type": "select", "id": "keep", "value": 24, "items": [{ "value": 1, "label": "1 hour" }, { "value": 24, "label": "1 day" }] },
        ]}]}]);
        let rows = rows(tree.as_array().unwrap(), &no_images);
        assert_eq!(rows[0].kind, "heading");
        let select = &rows[1].items[0];
        assert_eq!(select.index, 1);
        assert_eq!(event_value(select, "0"), json!(1));
    }

    #[test]
    fn a_patch_changes_only_the_named_node() {
        let mut tree = vec![json!({ "type": "group", "children": [{ "type": "row", "children": [
            { "type": "label", "id": "elapsed", "text": "0:10" },
            { "type": "progress", "id": "bar", "value": 0.1 },
        ]}]})];
        assert!(patch(&mut tree, &[json!({ "id": "elapsed", "text": "1:20", "type": "button" })]));
        let rows = rows(&tree, &no_images);
        assert_eq!(rows[0].items[0].kind, "label");
        assert_eq!(rows[0].items[0].text, "1:20");
        assert!((rows[0].items[1].fraction - 0.1).abs() < 1e-6);
        assert!(!patch(&mut tree, &[json!({ "id": "missing", "text": "x" })]));
    }

    #[test]
    fn stored_settings_replace_the_declared_defaults() {
        let tree = vec![json!({ "type": "group", "children": [{ "type": "row", "children": [
            { "type": "toggle", "id": "private", "value": false },
        ]}]})];
        let filled = with_values(&tree, &|id| (id == "private").then(|| json!(true)));
        assert!(rows(&filled, &no_images)[0].items[0].checked);
    }
}
