//! Builders for view trees. A view is groups of rows, each row holding up to 4 items. Every
//! builder returns a plain JSON node, and `.with(key, value)` sets any other prop.
//!
//! ```ignore
//! use rona_plugin::ui::*;
//! let tree = vec![group("Account", vec![
//!     row("Token", vec![text("token", "").secret()]),
//!     row("", vec![button("sign-in", "Sign in").primary()]),
//! ])];
//! ```

use serde_json::{Map, Value, json};

pub trait NodeExt {
    fn with(self, key: &str, value: impl Into<Value>) -> Value;
    fn tone(self, tone: &str) -> Value;
    fn disabled(self) -> Value;
    fn primary(self) -> Value;
    fn danger(self) -> Value;
    fn ghost(self) -> Value;
    fn secret(self) -> Value;
    fn live(self) -> Value;
}

impl NodeExt for Value {
    fn with(mut self, key: &str, value: impl Into<Value>) -> Value {
        if let Some(map) = self.as_object_mut() {
            map.insert(key.to_owned(), value.into());
        }
        self
    }

    fn tone(self, tone: &str) -> Value {
        self.with("tone", tone)
    }

    fn disabled(self) -> Value {
        self.with("enabled", false)
    }

    fn primary(self) -> Value {
        self.with("primary", true)
    }

    fn danger(self) -> Value {
        self.with("danger", true)
    }

    fn ghost(self) -> Value {
        self.with("ghost", true)
    }

    /// A text field that hides what is typed, for tokens and passwords.
    fn secret(self) -> Value {
        self.with("secret", true)
    }

    /// A text field that sends `change` 300 ms after the last key instead of on Enter.
    fn live(self) -> Value {
        self.with("live", true)
    }
}

fn node(kind: &str, props: Value) -> Value {
    let mut map = Map::new();
    map.insert("type".into(), kind.into());
    if let Value::Object(props) = props {
        map.extend(props);
    }
    Value::Object(map)
}

pub fn group(title: &str, children: Vec<Value>) -> Value {
    node("group", json!({ "title": title, "children": children }))
}

/// A label on the left and up to 4 items on the right. An empty label leaves the items the whole row.
pub fn row(label: &str, items: Vec<Value>) -> Value {
    node("row", json!({ "label": label, "children": items }))
}

pub fn label(text: &str) -> Value {
    node("label", json!({ "text": text }))
}

pub fn badge(text: &str) -> Value {
    node("badge", json!({ "text": text }))
}

pub fn button(id: &str, label: &str) -> Value {
    node("button", json!({ "id": id, "label": label }))
}

pub fn toggle(id: &str, label: &str, value: bool) -> Value {
    node("toggle", json!({ "id": id, "label": label, "value": value }))
}

/// `items` are `(value, label)` pairs.
pub fn select(id: &str, value: &str, items: &[(&str, &str)]) -> Value {
    let items: Vec<Value> = items.iter().map(|(value, label)| json!({ "value": value, "label": label })).collect();
    node("select", json!({ "id": id, "value": value, "items": items }))
}

pub fn segmented(id: &str, value: &str, items: &[(&str, &str)]) -> Value {
    select(id, value, items).with("type", "segmented")
}

pub fn slider(id: &str, value: f64, min: f64, max: f64, step: f64) -> Value {
    node("slider", json!({ "id": id, "value": value, "min": min, "max": max, "step": step }))
}

pub fn text(id: &str, value: &str) -> Value {
    node("text", json!({ "id": id, "value": value }))
}

/// `#rrggbb` or `#rrggbbaa`.
pub fn color(id: &str, value: &str) -> Value {
    node("color", json!({ "id": id, "value": value }))
}

/// A bar from 0 to 1, or a spinner for `None`.
pub fn progress(fraction: Option<f64>) -> Value {
    match fraction {
        Some(fraction) => node("progress", json!({ "value": fraction })),
        None => node("progress", json!({})),
    }
}

/// An image stored with `host::ui::image`, drawn in a box of `width` by `height` px.
pub fn image(image: &str, width: u32, height: u32) -> Value {
    node("image", json!({ "image": image, "width": width, "height": height }))
}

pub fn link(label: &str, url: &str) -> Value {
    node("link", json!({ "label": label, "url": url }))
}

pub fn separator() -> Value {
    node("separator", json!({}))
}

/// Centered text for a view with nothing to show yet.
pub fn empty(text: &str) -> Value {
    node("empty", json!({ "text": text }))
}

pub struct Entry {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    pub buttons: Vec<(String, String)>,
}

impl Entry {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self { id: id.into(), title: title.into(), subtitle: String::new(), image: None, buttons: Vec::new() }
    }

    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = subtitle.into();
        self
    }

    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// Up to 2 buttons. A click arrives as a `click` event with the button id and the entry id.
    pub fn button(mut self, id: impl Into<String>, label: impl Into<String>) -> Self {
        self.buttons.push((id.into(), label.into()));
        self
    }
}

/// Up to 500 rows of title, subtitle, image and buttons. A click on a row is a `click` with the list's id.
pub fn list(id: &str, entries: Vec<Entry>) -> Value {
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|entry| {
            let buttons: Vec<Value> =
                entry.buttons.into_iter().map(|(id, label)| json!({ "id": id, "label": label })).collect();
            json!({
                "id": entry.id,
                "title": entry.title,
                "subtitle": entry.subtitle,
                "image": entry.image,
                "buttons": buttons,
            })
        })
        .collect();
    node("list", json!({ "id": id, "entries": entries }))
}
