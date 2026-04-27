//! Helpers to build Feishu interactive-card JSON 2.0 payloads.
//!
//! Card JSON 2.0 root shape:
//! ```json
//! {
//!   "schema": "2.0",
//!   "config": {...},
//!   "header": {...},
//!   "body": { "direction": "vertical", "elements": [...] }
//! }
//! ```
//!
//! Card JSON 2.0 requires Lark client v7.20 or newer. Older clients see a
//! standard fallback "please upgrade" prompt.

use serde_json::{json, Value};

// ---------- card root ----------

pub fn card(header_value: Value, elements: Vec<Value>) -> Value {
    json!({
        "schema": "2.0",
        "config": {
            "wide_screen_mode": true,
            "update_multi": true,
        },
        "header": header_value,
        "body": {
            "direction": "vertical",
            "padding": "12px 16px 12px 16px",
            "vertical_spacing": "8px",
            "elements": elements,
        },
    })
}

// ---------- header ----------

pub fn header(title: &str, template: &str) -> Value {
    json!({
        "title": { "tag": "plain_text", "content": title },
        "template": template,
    })
}

pub fn header_with_subtitle(title: &str, subtitle: &str, template: &str) -> Value {
    json!({
        "title": { "tag": "plain_text", "content": title },
        "subtitle": { "tag": "plain_text", "content": subtitle },
        "template": template,
    })
}

// ---------- content ----------

/// Markdown component (v2.0). Accepts the same Lark markdown extensions as
/// `lark_md` did in v1.0 — bold (`**...**`), `<at id="ou_..."></at>`, etc.
pub fn markdown(content: &str) -> Value {
    json!({
        "tag": "markdown",
        "content": content,
    })
}

/// Smaller / muted markdown line, equivalent to the old "note" element.
pub fn note(content: &str) -> Value {
    json!({
        "tag": "markdown",
        "text_size": "notation",
        "content": content,
    })
}

/// Bigger / heading-sized markdown — used for the rank+suit on card tiles.
pub fn heading_md(content: &str) -> Value {
    json!({
        "tag": "markdown",
        "text_size": "heading",
        "text_align": "center",
        "content": content,
    })
}

/// Same as `heading_md` but the content is wrapped in a `<font>` tag so the
/// caller can pick a colour (e.g. `"white"` for content sitting on a red
/// background). Lark's `markdown` component honours inline `<font color="...">`.
pub fn heading_md_colored(content: &str, text_color: &str) -> Value {
    json!({
        "tag": "markdown",
        "text_size": "heading",
        "text_align": "center",
        "content": format!("<font color='{}'>{}</font>", text_color, content),
    })
}

/// Aliases kept so callers from the v1.0 API don't need to be renamed.
pub fn div_md(content: &str) -> Value {
    markdown(content)
}
pub fn note_md(content: &str) -> Value {
    note(content)
}

pub fn hr() -> Value {
    json!({ "tag": "hr" })
}

// ---------- layout ----------

pub fn column_set(columns: Vec<Value>) -> Value {
    json!({
        "tag": "column_set",
        "horizontal_spacing": "8px",
        "horizontal_align": "left",
        "columns": columns,
    })
}

pub fn column(elements: Vec<Value>, weight: u32) -> Value {
    json!({
        "tag": "column",
        "width": "weighted",
        "weight": weight,
        "vertical_align": "center",
        "elements": elements,
    })
}

/// A column with a coloured background — used for visual card tiles.
pub fn tile_column(elements: Vec<Value>, weight: u32, bg_style: &str) -> Value {
    json!({
        "tag": "column",
        "width": "weighted",
        "weight": weight,
        "vertical_align": "center",
        "padding": "12px 8px 12px 8px",
        "background_style": bg_style,
        "elements": elements,
    })
}

// ---------- buttons ----------

/// Plain interactive button. `style`: any of `default | primary | danger |
/// text | primary_text | danger_text | primary_filled | danger_filled`.
pub fn button(text: &str, value: Value, style: &str) -> Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": text },
        "type": style,
        "size": "medium",
        "width": "default",
        "behaviors": [
            { "type": "callback", "value": value }
        ],
    })
}

/// A submit button for use *inside a `form` container*. When clicked, the
/// form's input values are bundled in `event.action.form_value`.
pub fn submit_button(text: &str, value: Value, style: &str) -> Value {
    json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": text },
        "type": style,
        "size": "medium",
        "width": "default",
        "form_action_type": "submit",
        "name": format!("submit_{}", uuid::Uuid::new_v4().simple()),
        "behaviors": [
            { "type": "callback", "value": value }
        ],
    })
}

/// Lay out multiple buttons in a horizontal row that wraps on narrow screens.
///
/// `flex_mode: "flow"` 让窄屏（移动端）放不下时自动换行，而不是把按钮挤成
/// 一坨字看不清。`width: "auto"` 让每个按钮按文字宽度占位，不强行等分。
/// `vertical_spacing` 给换行后的两行之间留间距。
pub fn button_row(buttons: Vec<Value>) -> Value {
    if buttons.len() == 1 {
        return buttons.into_iter().next().unwrap();
    }
    let cols: Vec<Value> = buttons
        .into_iter()
        .map(|b| {
            json!({
                "tag": "column",
                "width": "auto",
                "vertical_align": "center",
                "elements": [b],
            })
        })
        .collect();
    json!({
        "tag": "column_set",
        "flex_mode": "flow",
        "horizontal_spacing": "8px",
        "vertical_spacing": "8px",
        "horizontal_align": "left",
        "columns": cols,
    })
}

/// Backwards-compat alias for `button_row`. v1.0 used `tag: "action"` to wrap
/// buttons; v2.0 uses a column_set of buttons instead.
pub fn actions(buttons: Vec<Value>) -> Value {
    button_row(buttons)
}

// ---------- form + input ----------

pub fn form(name: &str, elements: Vec<Value>) -> Value {
    json!({
        "tag": "form",
        "name": name,
        "elements": elements,
    })
}

pub fn input_field(name: &str, placeholder: &str, default_value: &str, label: &str) -> Value {
    json!({
        "tag": "input",
        "name": name,
        "placeholder": { "tag": "plain_text", "content": placeholder },
        "default_value": default_value,
        "label": { "tag": "plain_text", "content": label },
        "label_position": "left",
        "max_length": 10,
        "input_type": "text",
        "width": "fill",
    })
}

// ---------- people ----------

/// `person_list` component — show avatars + names for a list of users.
pub fn person_list(open_ids: &[String]) -> Value {
    let persons: Vec<Value> = open_ids
        .iter()
        .map(|id| json!({ "id": id }))
        .collect();
    json!({
        "tag": "person_list",
        "size": "medium",
        "show_avatar": true,
        "show_name": true,
        "lines": 2,
        "persons": persons,
    })
}

// ---------- text helpers ----------

/// At-mention chunk usable inside markdown content.
pub fn at(open_id: &str) -> String {
    format!("<at id=\"{open_id}\"></at>")
}
