//! Template helpers shared by the server and the `hermes-admin` CLI.

use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext};
use serde_json::{Map, Value};

/// Escapes a raw string so it is safe to embed inside a JSON string literal.
/// Covers all U+0000–U+001F control characters as required by RFC 8259.
pub fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"), // backspace
            '\x0C' => out.push_str("\\f"), // form feed
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Handlebars helper that delegates to [`escape_json_string`].
/// Usage in templates: `{{jsonEscape someField}}`
pub fn json_escape_helper(
    h: &Helper,
    _: &Handlebars,
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    if let Some(param) = h.param(0) {
        let raw = param.value().as_str().unwrap_or("");
        out.write(&escape_json_string(raw))?;
    }
    Ok(())
}

/// A `Handlebars` registry with every Hermes helper pre-registered.
/// Add new helpers here and both the server and the admin CLI get them.
pub fn new_handlebars() -> Handlebars<'static> {
    let mut hbs = Handlebars::new();
    hbs.register_helper("jsonEscape", Box::new(json_escape_helper));
    // Backward-compat alias so existing templates keep working.
    hbs.register_helper("escapeNewlines", Box::new(json_escape_helper));
    hbs
}

/// Renders a registered template and parses the result as JSON.
pub fn render_template(
    handlebars: &Handlebars,
    template_name: &str,
    data: &Map<String, Value>,
) -> Result<Value, String> {
    let rendered = handlebars
        .render(template_name, data)
        .map_err(|e| format!("Template rendering failed: {e}"))?;

    serde_json::from_str(&rendered).map_err(|e| format!("Rendered template is not valid JSON: {e}"))
}

/// Wraps non-object JSON bodies as `{"data": <value>}` so templates always
/// have a map to render against.
pub fn json_to_template_data(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        _ => {
            let mut result = Map::new();
            result.insert("data".to_string(), value.clone());
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_newline() {
        assert_eq!(escape_json_string("a\nb"), "a\\nb");
    }

    #[test]
    fn test_escape_tab() {
        assert_eq!(escape_json_string("a\tb"), "a\\tb");
    }

    #[test]
    fn test_escape_carriage_return() {
        assert_eq!(escape_json_string("a\rb"), "a\\rb");
    }

    #[test]
    fn test_escape_backslash() {
        assert_eq!(escape_json_string("a\\b"), "a\\\\b");
    }

    #[test]
    fn test_escape_double_quote() {
        assert_eq!(escape_json_string("say \"hi\""), "say \\\"hi\\\"");
    }

    #[test]
    fn test_escape_null_control_char() {
        assert_eq!(escape_json_string("\x00"), "\\u0000");
    }

    #[test]
    fn test_escape_other_control_char() {
        // U+001B = ESC
        assert_eq!(escape_json_string("\x1B"), "\\u001B");
    }

    #[test]
    fn test_escape_plain_string_unchanged() {
        assert_eq!(escape_json_string("hello world"), "hello world");
    }

    #[test]
    fn test_escape_combined() {
        let input = "line1\nline2\ttabbed\r\n\"quoted\"";
        let expected = "line1\\nline2\\ttabbed\\r\\n\\\"quoted\\\"";
        assert_eq!(escape_json_string(input), expected);
    }

    #[test]
    fn test_json_to_template_data_object() {
        let json = serde_json::json!({"name": "John", "age": 30});
        let data = json_to_template_data(&json);
        assert_eq!(data.get("name").unwrap(), &Value::String("John".into()));
        assert_eq!(
            data.get("age").unwrap(),
            &Value::Number(serde_json::Number::from(30))
        );
    }

    #[test]
    fn test_json_to_template_data_non_object_wrapped() {
        let json = serde_json::json!("just a string");
        let data = json_to_template_data(&json);
        assert_eq!(
            data.get("data").unwrap(),
            &Value::String("just a string".into())
        );
    }

    #[test]
    fn test_registry_has_both_helpers() {
        let hbs = new_handlebars();
        let data = serde_json::json!({"m": "a\nb"});
        let mut hbs = hbs;
        hbs.register_template_string("t", "{{jsonEscape m}}|{{escapeNewlines m}}")
            .unwrap();
        assert_eq!(hbs.render("t", &data).unwrap(), "a\\nb|a\\nb");
    }
}
