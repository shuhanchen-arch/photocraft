//! Edit › Generative Fill…: the prompt dialog in front of `edit.generativeFill` (Gemini image
//! generation into the selection). The fields are the command's params, so `ui.dialog.set` /
//! `ui.dialog.confirm` drive it like any dialog. The model and size start from Edit › Preferences ›
//! AI Generation and OK saves them back there; the feather is remembered in
//! `dialogs["edit.generativeFill"]`. The prompt is never remembered. [`prefs_rows`] draws the
//! AI Generation page of Preferences (model, size and the masked API key field).

use serde_json::{Map, Value, json};

use crate::PhotocraftApp;

pub const COMMAND: &str = photocraft_engine::genai_cmds::ID;
const MARK: &str = "__generativeFill";
/// Params sent to the engine.
const KEYS: [&str; 4] = ["prompt", "model", "imageSize", "feather"];
/// Params remembered in `dialogs[COMMAND]` (model and size live in `prefs.ai`).
const REMEMBERED: [&str; 1] = ["feather"];
/// The models offered in the dialog and Preferences (any other name can be passed through the
/// command or `prefs.set`).
pub const MODELS: [(&str, &str); 4] = [
    ("gemini-nano-banana-2.1", "Nano Banana 2.1 (default)"),
    ("gemini-3-pro-image", "Nano Banana Pro (best quality)"),
    ("gemini-3.1-flash-lite-image", "Nano Banana 2 Lite (fastest, 1K only)"),
    ("gemini-2.5-flash-image", "Nano Banana (legacy)"),
];
pub const SIZES: [(&str, &str); 4] = [("auto", "Auto"), ("1K", "1K"), ("2K", "2K"), ("4K", "4K")];

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
    let ai = &app.session.prefs().ai;
    let model = if valid("model", &json!(ai.model)) { ai.model.clone() } else { MODELS[0].0.to_string() };
    let size = if valid("imageSize", &json!(ai.image_size)) { ai.image_size.clone() } else { "auto".to_string() };
    f.insert("model".into(), json!(model));
    f.insert("imageSize".into(), json!(size));
    f.insert("feather".into(), json!(0.0));
    f.insert("__hasKey".into(), json!(has_key(app)));
    if let Some(Value::Object(saved)) = app.session.prefs().dialogs.get(COMMAND) {
        for key in REMEMBERED {
            if let Some(value) = saved.get(key).filter(|v| valid(key, v)) {
                f.insert(key.into(), value.clone());
            }
        }
    }
    f
}

/// Whether a key is available: in Preferences or the environment.
fn has_key(app: &PhotocraftApp) -> bool {
    !app.session.prefs().ai.api_key.trim().is_empty() || std::env::var(photocraft_engine::genai_cmds::API_KEY_ENV).is_ok_and(|k| !k.trim().is_empty())
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
        let model = f.get("model").filter(|v| valid("model", v)).and_then(Value::as_str).map(str::to_string);
        let size = f.get("imageSize").filter(|v| valid("imageSize", v)).and_then(Value::as_str).map(str::to_string);
        app.session.prefs.edit(|prefs| {
            prefs.dialogs.insert(COMMAND.into(), Value::Object(remembered));
            if let Some(m) = model {
                prefs.ai.model = m;
            }
            if let Some(s) = size {
                prefs.ai.image_size = s;
            }
        });
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
    if f.get("__hasKey").and_then(Value::as_bool) == Some(false) {
        ui.colored_label(crate::theme::Tokens::get(ui.ctx()).warning, tl!("No API key yet: add one in Edit › Preferences › AI Generation."));
    }
    ui.weak(tl!("The prompt is sent to Google Gemini."));
}

/// Preferences › AI Generation: model, output size and the API key (masked unless Show is on).
/// `obj` is the dialog's working copy of the `ai` section; Apply / OK commit it with `prefs.set`.
pub fn prefs_rows(ui: &mut egui::Ui, obj: &mut Map<String, Value>) {
    ui.spacing_mut().item_spacing.y = 8.0;
    ui.horizontal(|ui| {
        ui.label(tl!("Model"));
        let mut model = obj.get("model").and_then(Value::as_str).unwrap_or(MODELS[0].0).to_string();
        let mut options: Vec<(String, &str)> = MODELS.iter().map(|(id, label)| (id.to_string(), *label)).collect();
        // A model set through prefs.set that the list doesn't know stays selectable.
        if !MODELS.iter().any(|(id, _)| *id == model) {
            options.push((model.clone(), model.as_str()));
        }
        let options: Vec<(String, String)> = options.into_iter().map(|(id, l)| (id, l.to_string())).collect();
        let pairs: Vec<(String, &str)> = options.iter().map(|(id, l)| (id.clone(), l.as_str())).collect();
        if crate::widgets::dropdown_literal(ui, "prefs-ai-model", &mut model, &pairs, 260.0) {
            obj.insert("model".into(), json!(model));
        }
    });
    ui.horizontal(|ui| {
        ui.label(tl!("Output Size"));
        let mut size = obj.get("imageSize").and_then(Value::as_str).unwrap_or("auto").to_string();
        let options: Vec<(String, &str)> = SIZES.iter().map(|(id, label)| (id.to_string(), *label)).collect();
        if crate::widgets::dropdown_literal(ui, "prefs-ai-size", &mut size, &options, 90.0) {
            obj.insert("imageSize".into(), json!(size));
        }
    });
    let show_id = ui.id().with("prefs-ai-show-key");
    let mut show = ui.ctx().data(|m| m.get_temp::<bool>(show_id).unwrap_or(false));
    ui.horizontal(|ui| {
        ui.label(tl!("API Key"));
        let mut key = obj.get("apiKey").and_then(Value::as_str).unwrap_or_default().to_string();
        let edit = egui::TextEdit::singleline(&mut key).password(!show).desired_width(260.0).hint_text(tl!("Paste your Gemini API key"));
        if ui.add(edit).changed() {
            obj.insert("apiKey".into(), json!(key.trim()));
        }
        if ui.checkbox(&mut show, tl!("Show")).changed() {
            ui.ctx().data_mut(|m| m.insert_temp(show_id, show));
        }
    });
    let t = crate::theme::Tokens::get(ui.ctx());
    ui.label(egui::RichText::new(tl!("Get a key at aistudio.google.com/apikey. It is saved in PhotoCraft's preferences file on this computer, in plain text, and sent only to Google. Leave it empty to use the GEMINI_API_KEY environment variable.")).color(t.text_faint));
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
        app.session.prefs.edit(|p| {
            p.dialogs.insert(COMMAND.into(), json!({"feather": 6.0, "prompt": "old"}));
            p.ai.model = "gemini-3-pro-image".into();
            p.ai.image_size = "8K".into();
        });
        let f = fields(&app);
        assert_eq!(f["model"], "gemini-3-pro-image", "the model comes from Preferences");
        assert_eq!(f["imageSize"], "auto", "an invalid saved size falls back");
        assert_eq!(f["feather"], 6.0);
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
