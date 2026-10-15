//! Edit › Generative Fill… (`edit.generativeFill`): generates an image for the selection with
//! Google's Gemini image models (`photocraft-genai`) and places it on a new layer masked by the
//! selection. Photoshop's item of the same name uses Adobe Firefly; this one is our own.
//!
//! Stage 1 (this module today) is text-only generation: the prompt is sent with the aspect
//! ratio closest to the selection's bounds, and the reply is scaled to cover those bounds. The
//! layer's mask is the selection, so the document's own pixels never change and deleting the
//! layer undoes everything. Sending the surrounding pixels as a reference (stage 2) is described
//! in `docs/gemini-generative-fill.md`.
//!
//! The request runs as a background job ([`crate::jobs::run`]): the UI stays responsive, and the
//! result is applied as one undo step. The API key comes from the `apiKey` param or the
//! `GEMINI_API_KEY` environment variable; it is never journalled or shown.

use photocraft_algo::resample::{Resample, resize_surface_in_canvas, translate_surface};
use photocraft_doc::{Layer, LayerContent, LayerMask};
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// The command id.
pub const ID: &str = "edit.generativeFill";
/// History label.
const LABEL: &str = "Generative Fill";
/// Longest generated layer name, in characters of the prompt.
const NAME_CHARS: usize = 32;
/// Largest selection bounds accepted (pixels), so scaling the reply up can't exhaust memory.
const MAX_AREA: u64 = 400_000_000;

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: ID.into(), msg: msg.into() }
}

fn has_selection(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    let sel = d.doc.selection.as_ref().ok_or("make a selection first: the image is generated inside it")?;
    if sel.content_bounds().intersect(&d.doc.bounds()).is_empty() { Err("the selection is empty or outside the canvas".into()) } else { Ok(()) }
}

/// Validated params.
struct Params {
    prompt: String,
    model: String,
    image_size: Option<String>,
    api_key: Option<String>,
    feather: f32,
}

fn opt_str<'a>(p: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(bad(format!("`{key}` must be a string"))),
    }
}

fn params(p: &Value) -> Result<Params> {
    if !p.is_object() {
        return Err(bad("params must be a JSON object with a `prompt`"));
    }
    let prompt = opt_str(p, "prompt")?.map(str::trim).unwrap_or_default().to_string();
    if prompt.is_empty() {
        return Err(bad("`prompt` is required: describe what to generate in the selection"));
    }
    let model = opt_str(p, "model")?.map(str::trim).filter(|m| !m.is_empty()).unwrap_or(photocraft_genai::DEFAULT_MODEL).to_string();
    let image_size = match opt_str(p, "imageSize")?.map(str::trim) {
        None | Some("" | "auto") => None,
        Some(s) if photocraft_genai::IMAGE_SIZES.contains(&s) => Some(s.to_string()),
        Some(s) => return Err(bad(format!("`imageSize` must be auto, 1K, 2K or 4K, not `{s}`"))),
    };
    let api_key = opt_str(p, "apiKey")?.map(str::to_string);
    let feather = match p.get("feather") {
        None | Some(Value::Null) => 0.0,
        Some(v) => match v.as_f64() {
            Some(f) if f.is_finite() && (0.0..=1000.0).contains(&f) => f as f32,
            _ => return Err(bad("`feather` must be a number of pixels in 0..1000")),
        },
    };
    // Same checks the API client makes, before a job is started.
    let request = photocraft_genai::Request { model: model.clone(), prompt: prompt.clone(), images: Vec::new(), aspect_ratio: None, image_size: image_size.clone() };
    request.validate().map_err(|e| bad(e.to_string()))?;
    Ok(Params { prompt, model, image_size, api_key, feather })
}

/// The layer name for `prompt`: "Generative Fill: <first words>".
fn layer_name(prompt: &str) -> String {
    let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = one_line.chars().take(NAME_CHARS).collect();
    if one_line.chars().count() > NAME_CHARS {
        short.push('…');
    }
    format!("{LABEL}: {short}")
}

/// The scale that makes a `w × h` image cover `target`, and the top-left offset that centres it.
fn cover(w: u32, h: u32, target: Rect) -> Option<(f64, i32, i32)> {
    if w == 0 || h == 0 || target.is_empty() {
        return None;
    }
    let (tw, th) = (f64::from(target.width()), f64::from(target.height()));
    let scale = (tw / f64::from(w)).max(th / f64::from(h));
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let dx = f64::from(target.x0) + (tw - f64::from(w) * scale) / 2.0;
    let dy = f64::from(target.y0) + (th - f64::from(h) * scale) / 2.0;
    Some((scale, dx.round() as i32, dy.round() as i32))
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let prm = params(p)?;
    has_selection(s).map_err(bad)?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let doc = st.doc.clone();
    let sel: Surface = doc.selection.clone().ok_or_else(|| bad("no selection"))?;
    let target = sel.content_bounds().intersect(&doc.bounds());
    if u64::from(target.width()) * u64::from(target.height()) > MAX_AREA {
        return Err(bad("the selection is too large for Generative Fill"));
    }
    let config = photocraft_genai::Config::from_env(prm.api_key.as_deref()).map_err(|e| EngineError::Other(e.to_string()))?;
    let (ratio, _, _) = photocraft_genai::nearest_aspect_ratio(target.width(), target.height());
    let image_size = prm.image_size.clone().unwrap_or_else(|| photocraft_genai::image_size_for(target.width().max(target.height())).to_string());
    let request = photocraft_genai::Request {
        model: prm.model.clone(),
        prompt: prm.prompt.clone(),
        images: Vec::new(),
        aspect_ratio: Some(ratio.to_string()),
        image_size: Some(image_size.clone()),
    };
    let name = layer_name(&prm.prompt);
    let feather = prm.feather;
    let model = prm.model;
    crate::jobs::run(
        s,
        LABEL,
        true,
        move |ctx| {
            ctx.progress(0.05, "Generating with Gemini…");
            let generated = photocraft_genai::generate(&request, &config).map_err(|e| EngineError::Other(e.to_string()))?;
            ctx.check()?;
            ctx.progress(0.9, "Placing the result…");
            Ok(generated)
        },
        move |s, generated| place(s, generated, &sel, target, &name, feather, json!({"model": model, "aspectRatio": ratio, "imageSize": image_size})),
    )
}

/// Decode the generated image, scale it to cover `target`, and add it above the active layer
/// with `sel` as its mask, as one undo step.
fn place(s: &mut Session, generated: photocraft_genai::Generated, sel: &Surface, target: Rect, name: &str, feather: f32, info: Value) -> Result<Value> {
    let fmt = s.active().ok_or(EngineError::NoDocument)?.doc.pixel_format();
    let ext = match generated.mime_type.as_str() {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    };
    let imported = photocraft_io::import(&format!("generated.{ext}"), &generated.bytes).map_err(|e| EngineError::Other(format!("could not read the generated image: {e}")))?;
    let src = imported.document;
    let (w, h) = (src.size.width, src.size.height);
    let (scale, dx, dy) = cover(w, h, target).ok_or_else(|| EngineError::Other("the generated image is empty".into()))?;
    let px = crate::file_cmds::flattened(&src, fmt);
    // Edges repeat outwards while scaling, so the cover stays opaque up to its border.
    let px = if (scale - 1.0).abs() > 1e-9 { resize_surface_in_canvas(&px, scale, scale, Resample::Bicubic, src.bounds()) } else { px };
    let px = translate_surface(&px, dx, dy);
    let mask = LayerMask { surface: sel.clone(), feather, ..LayerMask::reveal_all() };
    let layer_name = name.to_string();
    let id = s.edit(LABEL, |doc, active| {
        let mut layer = Layer::new(layer_name, LayerContent::Raster(px));
        layer.mask = Some(mask);
        let id = doc.insert_above(*active, layer);
        *active = Some(id);
        Ok(id)
    })?;
    let mut out = json!({
        "layer": id.0,
        "bounds": [target.x0, target.y0, target.x1, target.y1],
        "generatedSize": [w, h],
        "scale": scale * 100.0,
    });
    if let (Some(o), Some(extra)) = (out.as_object_mut(), info.as_object()) {
        o.extend(extra.clone());
        if let Some(t) = generated.text {
            o.insert("text".into(), json!(t));
        }
    }
    Ok(out)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: ID,
        label: "Generative Fill…",
        menu: &["Edit"],
        shortcut: None,
        params: r##"{"prompt":string (what to generate in the selection), "model":string? (default gemini-nano-banana-2.1), "imageSize":"auto"|"1K"|"2K"|"4K"?, "feather":px? (mask feather, default 0), "apiKey":string? (default: GEMINI_API_KEY environment variable)} -> a new layer above the active one, masked by the selection"##,
        enabled: has_selection,
        run,
        journal: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_selection() -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 64, "height": 48, "background": "#ffffff"})).unwrap();
        s.execute("select.rect", json!({"x": 8, "y": 8, "width": 32, "height": 16})).unwrap();
        s
    }

    #[test]
    fn params_are_validated_before_anything_is_sent() {
        let mut s = session_with_selection();
        for p in [
            json!(null),
            json!([]),
            json!({}),
            json!({"prompt": "   "}),
            json!({"prompt": 5}),
            json!({"prompt": "x", "model": 3}),
            json!({"prompt": "x", "model": "has space"}),
            json!({"prompt": "x", "imageSize": "8K"}),
            json!({"prompt": "x", "feather": -1}),
            json!({"prompt": "x", "feather": "wide"}),
            json!({"prompt": "x".repeat(photocraft_genai::MAX_PROMPT_CHARS + 1)}),
        ] {
            assert!(matches!(s.execute(ID, p.clone()), Err(EngineError::BadParams { .. })), "{p}");
        }
    }

    #[test]
    fn it_needs_a_selection() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 16, "height": 16})).unwrap();
        assert!(s.execute(ID, json!({"prompt": "a cat", "apiKey": "k"})).is_err());
        assert!(has_selection(&s).is_err());
        assert!(has_selection(&session_with_selection()).is_ok());
    }

    #[test]
    fn layer_names_are_short_single_lines() {
        assert_eq!(layer_name("a  red\numbrella"), "Generative Fill: a red umbrella");
        let long = layer_name(&"word ".repeat(40));
        assert!(long.ends_with('…'));
        assert_eq!(long.chars().count(), "Generative Fill: ".chars().count() + NAME_CHARS + 1);
    }

    #[test]
    fn cover_scales_to_the_larger_ratio_and_centres() {
        // A 16:9 reply over a 32×16 (2:1) box: the width decides, and the extra height is split.
        let (scale, dx, dy) = cover(160, 90, Rect::new(8, 8, 40, 24)).unwrap();
        assert!((scale - 0.2).abs() < 1e-9, "32/160 = 0.2 ≥ 16/90");
        assert_eq!((dx, dy), (8, 7), "18 px tall, centred on a 16 px box");
        let (scale, dx, _) = cover(100, 100, Rect::new(0, 0, 50, 100)).unwrap();
        assert!((scale - 1.0).abs() < 1e-9);
        assert_eq!(dx, -25);
        assert!(cover(0, 10, Rect::new(0, 0, 5, 5)).is_none());
        assert!(cover(10, 10, Rect::new(0, 0, 0, 5)).is_none());
    }

    /// A tiny PNG, encoded through the io crate so the test needs no fixtures.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": w, "height": h, "background": "#ff0000"})).unwrap();
        let doc = &s.active().unwrap().doc;
        photocraft_io::export(doc, "png", &photocraft_io::ExportOptions::default()).unwrap().bytes
    }

    #[test]
    fn the_reply_lands_on_a_new_layer_masked_by_the_selection() {
        let mut s = session_with_selection();
        let before = s.active().unwrap().doc.layers.len();
        let sel = s.active().unwrap().doc.selection.clone().unwrap();
        let target = Rect::new(8, 8, 40, 24);
        let generated = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png(64, 32), text: Some("ok".into()) };
        let out = place(&mut s, generated, &sel, target, "Generative Fill: test", 2.0, json!({"model": "m"})).unwrap();
        let st = s.active().unwrap();
        assert_eq!(st.doc.layers.len(), before + 1);
        let id = st.active_layer.unwrap();
        assert_eq!(out["layer"], id.0);
        assert_eq!(out["model"], "m");
        assert_eq!(out["text"], "ok");
        let layer = st.doc.layer(id).unwrap();
        assert_eq!(layer.name, "Generative Fill: test");
        let mask = layer.mask.as_ref().unwrap();
        assert_eq!(mask.feather, 2.0);
        assert_eq!(mask.surface.content_bounds(), sel.content_bounds());
        // The pixels cover the selection's bounds.
        let LayerContent::Raster(px) = &layer.content else { panic!("a pixel layer") };
        assert!(px.content_bounds().contains_rect(&target));
        // One undo step removes it.
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.active().unwrap().doc.layers.len(), before);
    }

    #[test]
    fn an_unreadable_reply_is_an_error_and_changes_nothing() {
        let mut s = session_with_selection();
        let before = s.active().unwrap().doc.layers.len();
        let sel = s.active().unwrap().doc.selection.clone().unwrap();
        let junk = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: b"not an image".to_vec(), text: None };
        assert!(place(&mut s, junk, &sel, Rect::new(8, 8, 40, 24), "x", 0.0, json!({})).is_err());
        assert_eq!(s.active().unwrap().doc.layers.len(), before);
    }
}
