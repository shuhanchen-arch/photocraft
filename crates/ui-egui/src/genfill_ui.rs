//! Edit › Generative Fill…: the prompt dialog in front of `edit.generativeFill` (Gemini image
//! generation into the selection). The fields are the command's params, so `ui.dialog.set` /
//! `ui.dialog.confirm` drive it like any dialog. OK remembers the model, size and feather (not the
//! prompt or any key) in the preferences (`dialogs["edit.generativeFill"]`).

use serde_json::{Map, Value, json};

use crate::PhotocraftApp;

pub const COMMAND: &str = photocraft_engine::genai_cmds::ID;
const MARK: &str = "__generativeFill";
/// Params sent to the engine.
const KEYS: [&str; 4] = ["prompt", "model", "imageSize", "feather"];
/// Params remembered for next time.
const REMEMBERED: [&str; 3] = ["model", "imageSize", "feather"];
/// The models offered in the dialog (any other name can be passed through the command).
const MODELS: [(&str, &str); 4] = [
    ("gemini-nano-banana-2.1", "Nano Banana 2.1 (default)"),
    ("gemini-3-pro-image", "Nano Banana Pro (best quality)"),
    ("gemini-3.1-flash-lite-image", "Nano Banana 2 Lite (fastest, 1K only)"),
    ("gemini-2.5-flash-image", "Nano Banana (legacy)"),
];
const SIZES: [(&str, &str); 4] = [("auto", "Auto"), ("1K", "1K"), ("2K", "2K"), ("4K", "4K")];

pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.contains_key(MARK)
}

fn valid(key: &str, v: &Value) -> bool {
    match key {
        "model" => v.as_str().is_some_and(|m| MODELS.iter().any(|(id, _)| *id == m)),
        "imageSize" => v.as_str().is_some_and(|s| SIZES.iter().any(|(id, _)| *id == s)),
        "feather" => v.as_f64().is_some_and(|f| f.is_finite() && (0.0..=250.0).contains(&f)),
        _ => false,
    }
}

/// The dialog's fields: defaults, then the valid remembered choices.
pub fn fields(app: &PhotocraftApp) -> Map<String, Value> {
    let mut f = Map::new();
    f.insert(MARK.into(), json!(true));
    f.insert("__command".into(), json!(COMMAND));
    f.insert("__label".into(), json!("Generative Fill…"));
    f.insert("prompt".into(), json!(""));
    f.insert("model".into(), json!(MODELS[0].0));
    f.insert("imageSize".into(), json!("auto"));
    f.insert("feather".into(), json!(0.0));
    if let Some(Value::Object(saved)) = app.session.prefs().dialogs.get(COMMAND) {
        for key in REMEMBERED {
            if let Some(value) = saved.get(key).filter(|v| valid(key, v)) {
                f.insert(key.into(), value.clone());
            }
        }
    }
    f
}

pub fn open(app: &mut PhotocraftApp) -> u64 {
    let f = fields(app);
    app.ui.open_dialog(crate::state::DialogKind::Command, f)
}

/// Only the command's params go to the engine; private UI markers are dropped.
pub fn params(f: &Map<String, Value>) -> Value {
    Value::Object(KEYS.iter().filter_map(|k| f.get(*k).map(|v| (k.to_string(), v.clone()))).collect())
}

pub fn confirm(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    let p = params(f);
    if p.get("prompt").and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()) {
        return Err(tl!("Describe what to generate in the selection.").into());
    }
    let result = app.run(COMMAND, p);
    if result.is_ok() {
        let remembered: Map<String, Value> = REMEMBERED.iter().filter_map(|k| f.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
        app.session.prefs.edit(|prefs| prefs.dialogs.insert(COMMAND.into(), Value::Object(remembered)));
    }
    result
}

pub fn body(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    ui.spacing_mut().item_spacing.y = 6.0;
    ui.label(tl!("Prompt:"));
    let mut prompt = f.get("prompt").and_then(Value::as_str).unwrap_or_default().to_string();
    let edit = egui::TextEdit::multiline(&mut prompt)
        .desired_rows(4)
        .desired_width(360.0)
        .hint_text(tl!("Describe what to generate in the selection, e.g. “a red umbrella on the sand”"));
    if ui.add(edit).changed() {
        f.insert("prompt".into(), json!(prompt));
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(tl!("Model:"));
        let mut model = f.get("model").and_then(Value::as_str).unwrap_or(MODELS[0].0).to_string();
        let options: Vec<(String, &str)> = MODELS.iter().map(|(id, label)| (id.to_string(), *label)).collect();
        if crate::widgets::dropdown_literal(ui, "genfill-model", &mut model, &options, 250.0) {
            f.insert("model".into(), json!(model));
        }
    });
    ui.horizontal(|ui| {
        ui.label(tl!("Output Size:"));
        let mut size = f.get("imageSize").and_then(Value::as_str).unwrap_or("auto").to_string();
        let options: Vec<(String, &str)> = SIZES.iter().map(|(id, label)| (id.to_string(), *label)).collect();
        if crate::widgets::dropdown_literal(ui, "genfill-size", &mut size, &options, 90.0) {
            f.insert("imageSize".into(), json!(size));
        }
        ui.add_space(12.0);
        ui.label(tl!("Feather:"));
        let mut feather = f.get("feather").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        if crate::widgets::value_field(ui, &mut feather, 0.0..=250.0, "px", 64.0).changed() {
            f.insert("feather".into(), json!(feather.round()));
        }
    });
    ui.add_space(4.0);
    ui.weak(tl!("The prompt is sent to Google Gemini. The API key is read from the GEMINI_API_KEY environment variable."));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> PhotocraftApp {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 32, "height": 32})).unwrap();
        s.execute("select.rect", json!({"x": 4, "y": 4, "width": 8, "height": 8})).unwrap();
        PhotocraftApp::new(s, crate::Services::default())
    }

    #[test]
    fn fields_start_from_defaults_and_valid_remembered_choices() {
        let mut app = app();
        let f = fields(&app);
        assert!(owns(&f));
        assert_eq!(f["prompt"], "");
        assert_eq!(f["model"], MODELS[0].0);
        app.session.prefs.edit(|p| p.dialogs.insert(COMMAND.into(), json!({"model": "gemini-3-pro-image", "imageSize": "8K", "prompt": "old"})));
        let f = fields(&app);
        assert_eq!(f["model"], "gemini-3-pro-image");
        assert_eq!(f["imageSize"], "auto", "an invalid remembered size falls back");
        assert_eq!(f["prompt"], "", "the prompt is never restored");
    }

    #[test]
    fn params_drop_private_markers() {
        let app = app();
        let mut f = fields(&app);
        f.insert("prompt".into(), json!("a cat"));
        let p = params(&f);
        assert_eq!(p["prompt"], "a cat");
        assert!(p.get("__command").is_none() && p.get(MARK).is_none());
    }

    #[test]
    fn an_empty_prompt_is_refused_without_running() {
        let mut app = app();
        let f = fields(&app);
        let layers = app.session.active().unwrap().doc.layers.len();
        assert!(confirm(&mut app, &f).is_err());
        assert_eq!(app.session.active().unwrap().doc.layers.len(), layers);
    }
}
