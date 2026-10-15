//! Edit › Generative Fill… (`edit.generativeFill`): generates an image for the selection with
//! Google's Gemini image models (`photocraft-genai`) and places it on a new layer masked by the
//! selection. Photoshop's item of the same name uses Adobe Firefly; this one is our own.
//!
//! Two modes (`docs/gemini-generative-fill.md`):
//!
//! * **Surrounding image** (`context`, the default): a window around the selection
//!   ([`context_window`], padded and grown to an aspect ratio the models accept) is sent as one
//!   8-bit sRGB PNG of the visible composite with a red outline drawn around the selection
//!   ([`context_images`]), and a prompt asking to change only what is inside the outline
//!   ([`context_prompt`]). The outline runs where the layer mask hides the reply ([`Outline`]),
//!   so if the model keeps it, nobody sees it. Earlier designs sent a second image (the photo
//!   with the selection tinted magenta, then a black-and-white mask) and the models sometimes
//!   blended that image into the selection. The reply is scaled back to the window's exact size and masked by the
//!   selection grown by `expand` px and feathered by `feather` px, so nothing outside the
//!   selection changes. The models redraw the whole picture and often shift it a few pixels, so
//!   the reply is first lined up with the original on a ring outside the selection
//!   ([`crate::genai_align`]). A reply that doesn't match the picture on that ring (an unrelated
//!   or blank image) is rejected and asked for once more ([`with_retry`]); a second miss fails
//!   with nothing added. An empty prompt asks to remove what is selected. `sampleLayer` picks
//!   which layers the surroundings are read from ([`Sample`]): all of them (the default, so
//!   earlier results are part of the picture), or only the active layer and those below it, or
//!   the active layer alone, so repeated fills can start from the original photo every time.
//! * **Text only** (`context: false`): the prompt alone, with the aspect ratio closest to the
//!   selection's bounds; the reply is scaled to cover those bounds.
//!
//! Either way the document's own pixels never change and deleting the layer undoes everything.
//! The request runs as a background job ([`crate::jobs::run`]): the UI stays responsive, and the
//! result is applied as one undo step. The API key comes from the `apiKey` param, Preferences ›
//! AI Generation or the `GEMINI_API_KEY` environment variable; it is never journalled or shown.

use photocraft_algo::resample::{Resample, resize_surface_in_canvas, translate_surface};
use photocraft_doc::{Document, Layer, LayerContent, LayerMask};
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// The command id.
pub const ID: &str = "edit.generativeFill";
/// The environment variable read when Preferences › AI Generation has no key.
pub const API_KEY_ENV: &str = photocraft_genai::API_KEY_ENV;
/// History label.
const LABEL: &str = "Generative Fill";
/// Longest generated layer name, in characters of the prompt.
const NAME_CHARS: usize = 32;
/// Largest selection bounds accepted (pixels), so scaling the reply up can't exhaust memory.
const MAX_AREA: u64 = 400_000_000;
/// Largest window read around the selection (pixels): the composite and the image sent are held
/// in memory at once.
const MAX_CONTEXT_AREA: u64 = 64_000_000;
/// The context window reaches at least this far past the selection's bounds…
const CONTEXT_MIN_PAD: u32 = 64;
/// …or this fraction of their longer side, whichever is more.
const CONTEXT_PAD_FRACTION: f64 = 0.25;
/// Default mask growth and feather (px) with the surrounding image: they hide the seam.
const CONTEXT_EXPAND: f32 = 4.0;
const CONTEXT_FEATHER: f32 = 4.0;
/// Largest `expand` accepted (px).
const MAX_EXPAND: f64 = 100.0;
/// A reply whose ring outside the selection correlates less than this with the original's
/// ([`crate::genai_align::similarity`]) is not the picture: faithful redraws score above 0.9,
/// a blank or unrelated image about 0.
const MIN_SIMILARITY: f64 = 0.35;
/// Requests per generation: the first, and one more after a rejected reply.
const MAX_ATTEMPTS: usize = 2;
/// Width (px) of the ring, just outside the outline, that the alignment compares.
const ALIGN_BAND: f32 = 32.0;
/// The outline drawn around the selection: pure red, starting this far (px) beyond the reach of
/// the layer mask, and this wide (px) in the image as sent.
const OUTLINE_COLOR: [f32; 3] = [1.0, 0.0, 0.0];
const OUTLINE_CLEARANCE: f32 = 8.0;
const OUTLINE_WIDTH: f32 = 3.0;
/// Largest alignment shift: this fraction of the window's long side, within the bounds below.
const ALIGN_SHIFT_FRACTION: f32 = 0.02;
const ALIGN_SHIFT_MIN: f32 = 4.0;
const ALIGN_SHIFT_MAX: f32 = 24.0;

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: ID.into(), msg: msg.into() }
}

fn has_selection(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    let sel = d.doc.selection.as_ref().ok_or("make a selection first: the image is generated inside it")?;
    if sel.content_bounds().intersect(&d.doc.bounds()).is_empty() { Err("the selection is empty or outside the canvas".into()) } else { Ok(()) }
}

/// Which layers the surrounding image is read from (`sampleLayer`), as in Photoshop's Sample
/// menus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sample {
    /// Every visible layer: the picture as shown.
    All,
    /// The active layer and the visible layers below it.
    CurrentAndBelow,
    /// The active layer alone.
    Current,
}

impl Sample {
    fn id(self) -> &'static str {
        match self {
            Sample::All => "all",
            Sample::CurrentAndBelow => "currentAndBelow",
            Sample::Current => "current",
        }
    }
}

/// `doc` with only the layers `sample` reads from left visible. `active` is the active layer;
/// without one every visible layer is read. The active layer's enclosing groups stay as they are,
/// so it composites as it does on the canvas.
fn sampled_doc(doc: &Document, active: Option<photocraft_doc::LayerId>, sample: Sample) -> Document {
    let mut out = doc.clone();
    let Some(active) = active.filter(|_| sample != Sample::All) else { return out };
    // Bottom to top, groups before their contents.
    let walk: Vec<(Vec<usize>, photocraft_doc::LayerId)> = doc.walk().into_iter().map(|(p, _, l)| (p, l.id)).collect();
    let Some(at) = walk.iter().position(|(_, id)| *id == active) else { return out };
    let active_path = walk[at].0.clone();
    for (i, (path, _)) in walk.iter().enumerate() {
        let ancestor_or_self = active_path.starts_with(path);
        let hide = match sample {
            Sample::All => false,
            Sample::CurrentAndBelow => i > at && !ancestor_or_self,
            Sample::Current => !ancestor_or_self,
        };
        if hide && let Some(l) = out.layer_at_mut(path) {
            l.visible = false;
        }
    }
    out
}

/// Validated params.
#[derive(Debug)]
struct Params {
    /// What the user typed (may be empty with `context`).
    prompt: String,
    model: String,
    image_size: Option<String>,
    api_key: Option<String>,
    feather: f32,
    expand: f32,
    context: bool,
    sample: Sample,
}

fn opt_str<'a>(p: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(bad(format!("`{key}` must be a string"))),
    }
}

/// A pixel distance in `0..=max`, or `default` when absent.
fn opt_px(p: &Value, key: &str, default: f32, max: f64) -> Result<f32> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_f64() {
            Some(f) if f.is_finite() && (0.0..=max).contains(&f) => Ok(f as f32),
            _ => Err(bad(format!("`{key}` must be a number of pixels in 0..{max}"))),
        },
    }
}

/// `p` validated, with the model, size, key and mode from Preferences › AI Generation where `p`
/// has none (the key then falls back to the `GEMINI_API_KEY` environment variable).
fn params(p: &Value, ai: &crate::prefs::Ai) -> Result<Params> {
    if !p.is_object() {
        return Err(bad("params must be a JSON object with a `prompt`"));
    }
    let context = match p.get("context") {
        None | Some(Value::Null) => ai.use_surrounding_image,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad("`context` must be true or false")),
    };
    let prompt = opt_str(p, "prompt")?.map(str::trim).unwrap_or_default().to_string();
    if prompt.is_empty() && !context {
        return Err(bad("`prompt` is required without `context`: describe what to generate in the selection"));
    }
    let model = opt_str(p, "model")?
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .or_else(|| Some(ai.model.trim()).filter(|m| !m.is_empty()))
        .unwrap_or(photocraft_genai::DEFAULT_MODEL)
        .to_string();
    // An unusable saved size is ignored (auto) rather than blocking every generation.
    let saved_size = Some(ai.image_size.trim()).filter(|s| photocraft_genai::IMAGE_SIZES.contains(s));
    let image_size = match opt_str(p, "imageSize")?.map(str::trim).or(saved_size) {
        None | Some("" | "auto") => None,
        Some(s) if photocraft_genai::IMAGE_SIZES.contains(&s) => Some(s.to_string()),
        Some(s) => return Err(bad(format!("`imageSize` must be auto, 1K, 2K or 4K, not `{s}`"))),
    };
    let api_key = opt_str(p, "apiKey")?.map(str::to_string).or_else(|| Some(ai.api_key.trim().to_string()).filter(|k| !k.is_empty()));
    let (expand, feather) = if context { (CONTEXT_EXPAND, CONTEXT_FEATHER) } else { (0.0, 0.0) };
    let feather = opt_px(p, "feather", feather, 1000.0)?;
    let expand = opt_px(p, "expand", expand, MAX_EXPAND)?;
    // Same checks the API client makes, before a job is started.
    let sent = if context { context_prompt(&prompt) } else { prompt.clone() };
    let request = photocraft_genai::Request { model: model.clone(), prompt: sent, images: Vec::new(), aspect_ratio: None, image_size: image_size.clone() };
    request.validate().map_err(|e| bad(e.to_string()))?;
    let sample = match opt_str(p, "sampleLayer")?.map(str::trim) {
        None | Some("" | "all") => Sample::All,
        Some("currentAndBelow") => Sample::CurrentAndBelow,
        Some("current") => Sample::Current,
        Some(o) => return Err(bad(format!("unknown sampleLayer `{o}` (all|currentAndBelow|current)"))),
    };
    Ok(Params { prompt, model, image_size, api_key, feather, expand, context, sample })
}

/// The prompt sent with the surrounding image: the user's words wrapped in instructions to edit
/// only what the red outline encloses. An empty prompt asks to remove what is there.
fn context_prompt(user: &str) -> String {
    let user = user.split_whitespace().collect::<Vec<_>>().join(" ");
    let change =
        if user.is_empty() { "remove what is there and fill it in naturally from the surroundings".to_string() } else { format!("change it to: {user}") };
    format!(
        "This photo has a red outline drawn on it. Change ONLY the area inside the red outline: {change}. Keep any new \
         object entirely inside the outline, well away from it. Keep everything outside the outline exactly as it is, \
         with the same framing, perspective, lighting and colours, and blend the change in seamlessly. Return the \
         edited photo without the red outline, at the same size and aspect ratio."
    )
}

/// The layer name for `prompt`: "Generative Fill: <first words>", or just "Generative Fill".
fn layer_name(prompt: &str) -> String {
    let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return LABEL.to_string();
    }
    let mut short: String = one_line.chars().take(NAME_CHARS).collect();
    if one_line.chars().count() > NAME_CHARS {
        short.push('…');
    }
    format!("{LABEL}: {short}")
}

/// `a × b / c`, rounded (`c > 0`).
fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    if c <= 0 { 0 } else { (a.saturating_mul(b).saturating_mul(2) + c) / (2 * c) }
}

/// The start of a span `len` long, as close as possible to centred on `centre2 / 2`, inside
/// `lo..hi` and covering `must_lo..must_hi`; `None` when no such span exists.
fn place_span(len: i64, centre2: i64, (lo, hi): (i64, i64), (must_lo, must_hi): (i64, i64)) -> Option<i64> {
    if len <= 0 || len > hi - lo || len < must_hi - must_lo {
        return None;
    }
    let (min, max) = (lo.max(must_hi - len), (hi - len).min(must_lo));
    (min <= max).then(|| (centre2 - len).div_euclid(2).clamp(min, max))
}

/// The window sent around the selection bounds `hb`: padded by [`CONTEXT_MIN_PAD`] or
/// [`CONTEXT_PAD_FRACTION`], kept inside `canvas`, always covering `hb`, and shaped to one of the
/// models' aspect ratios (the closest that fits) so the reply scales back without distortion.
/// For each ratio it grows the short side first, else trims the long one, else takes the largest
/// window of that ratio in the canvas. When no ratio fits (a canvas far from all of them with the
/// selection spanning it) the padded window is returned with its nearest ratio, and the reply is
/// stretched slightly to fit. `None` when `hb` misses the canvas.
pub(crate) fn context_window(hb: Rect, canvas: Rect) -> Option<(Rect, &'static str)> {
    let hb = hb.intersect(&canvas);
    if hb.is_empty() {
        return None;
    }
    let long = f64::from(hb.width().max(hb.height()));
    let pad = (long * CONTEXT_PAD_FRACTION).ceil().max(f64::from(CONTEXT_MIN_PAD)).min(f64::from(i32::MAX)) as i32;
    let base = hb.inflate(pad).intersect(&canvas);
    let (bw, bh) = (i64::from(base.width()), i64::from(base.height()));
    let (cw, ch) = (i64::from(canvas.width()), i64::from(canvas.height()));
    let span_x = (i64::from(canvas.x0), i64::from(canvas.x1));
    let span_y = (i64::from(canvas.y0), i64::from(canvas.y1));
    let must_x = (i64::from(hb.x0), i64::from(hb.x1));
    let must_y = (i64::from(hb.y0), i64::from(hb.y1));
    let mut ratios = photocraft_genai::ASPECT_RATIOS;
    let target = (bw as f64 / bh as f64).ln();
    ratios.sort_by(|a, b| {
        let d = |r: &(&str, u32, u32)| (target - (f64::from(r.1) / f64::from(r.2)).ln()).abs();
        d(a).total_cmp(&d(b))
    });
    for (name, rw, rh) in ratios {
        let (rw, rh) = (i64::from(rw), i64::from(rh));
        let narrow = bw * rh < bh * rw;
        let grow = if narrow { (mul_div(bh, rw, rh), bh) } else { (bw, mul_div(bw, rh, rw)) };
        let trim = if narrow { (bw, mul_div(bw, rh, rw)) } else { (mul_div(bh, rw, rh), bh) };
        let largest_w = cw.min(mul_div(ch, rw, rh));
        let largest = (largest_w, mul_div(largest_w, rh, rw).min(ch));
        for (w, h) in [grow, trim, largest] {
            let x = place_span(w, i64::from(base.x0) + i64::from(base.x1), span_x, must_x);
            let y = place_span(h, i64::from(base.y0) + i64::from(base.y1), span_y, must_y);
            if let (Some(x), Some(y)) = (x, y) {
                let r = Rect::new(i32::try_from(x).ok()?, i32::try_from(y).ok()?, i32::try_from(x + w).ok()?, i32::try_from(y + h).ok()?);
                return Some((r, name));
            }
        }
    }
    Some((base, photocraft_genai::nearest_aspect_ratio(base.width(), base.height()).0))
}

/// The pixel length of an output size's long side.
fn size_px(size: &str) -> usize {
    match size {
        "2K" => 2048,
        "4K" => 4096,
        _ => 1024,
    }
}

/// Where the red outline runs, in window pixels from the selection: from `start` to
/// `start + width`. It starts [`OUTLINE_CLEARANCE`] px past the farthest the layer mask shows the
/// reply (the selection grown by `expand`, plus three feather sigmas), so an outline the model
/// keeps stays hidden; it is wide enough to be [`OUTLINE_WIDTH`] px in the image as sent.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Outline {
    start: f32,
    width: f32,
}

impl Outline {
    fn new(expand: f32, feather: f32, (w, h): (usize, usize), max_side: usize) -> Self {
        let shrink = (w.max(h) as f32 / max_side.max(1) as f32).max(1.0);
        Self { start: expand + 3.0 * feather + OUTLINE_CLEARANCE, width: OUTLINE_WIDTH * shrink }
    }

    /// Where the alignment ring starts: clear of the outline, whether or not the model kept it.
    fn ring_gap(self) -> f32 {
        self.start + self.width + 3.0
    }
}

/// Draws `outline` ([`OUTLINE_COLOR`]) into `rgb` (interleaved, `w × h`) around the pixels the
/// selection `cover` touches.
fn draw_outline(rgb: &mut [f32], cover: &[f32], w: usize, h: usize, outline: Outline) {
    let inside: Vec<bool> = cover.iter().map(|c| *c > 0.0).collect();
    let d = photocraft_algo::selection::edt(&inside, w, h);
    for (px, d) in rgb.chunks_exact_mut(3).zip(&d) {
        if *d > outline.start && *d <= outline.start + outline.width {
            px.copy_from_slice(&OUTLINE_COLOR);
        }
    }
}

/// `rgb` (interleaved, `w × h`) shrunk by area averaging so its long side is at most `max_side`;
/// unchanged when it already fits.
fn downscale(rgb: &[f32], w: usize, h: usize, max_side: usize) -> (Vec<f32>, usize, usize) {
    use rayon::prelude::*;
    let long = w.max(h);
    if long <= max_side || max_side == 0 || rgb.len() != w * h * 3 {
        return (rgb.to_vec(), w, h);
    }
    let s = max_side as f64 / long as f64;
    let (ow, oh) = (((w as f64 * s).round() as usize).clamp(1, w), ((h as f64 * s).round() as usize).clamp(1, h));
    let span = |o: usize, n: usize, on: usize| {
        let a = o * n / on;
        (a, ((o + 1) * n / on).clamp(a + 1, n))
    };
    let mut out = vec![0.0f32; ow * oh * 3];
    out.par_chunks_mut(ow * 3).enumerate().for_each(|(oy, row)| {
        let (y0, y1) = span(oy, h, oh);
        for (ox, px) in row.chunks_exact_mut(3).enumerate() {
            let (x0, x1) = span(ox, w, ow);
            let mut sum = [0.0f64; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 3;
                    for (k, s) in sum.iter_mut().enumerate() {
                        *s += f64::from(rgb.get(i + k).copied().unwrap_or(0.0));
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)).max(1) as f64;
            for (p, s) in px.iter_mut().zip(sum) {
                *p = (s / n) as f32;
            }
        }
    });
    (out, ow, oh)
}

/// An 8-bit RGB PNG of `rgb` (interleaved sRGB values in 0..1).
fn png(rgb: &[f32], w: usize, h: usize) -> Result<Vec<u8>> {
    let err = |e: photocraft_codecs::CodecError| EngineError::Other(format!("could not encode the image to send: {e}"));
    let bytes: Vec<u8> = rgb.iter().map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8).collect();
    let (w, h) = (u32::try_from(w).map_err(|_| bad("image too wide"))?, u32::try_from(h).map_err(|_| bad("image too tall"))?);
    let image = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgb, bytes).map_err(err)?;
    photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &photocraft_codecs::EncodeOptions::default()).map_err(err)
}

/// What [`context_images`] produces: the PNG sent, and the window's luminance (without the
/// outline) and selection coverage at full size, for lining the reply up and checking it.
struct ContextImages {
    photo: Vec<u8>,
    luma: Vec<f32>,
    cover: Vec<f32>,
    outline: Outline,
}

/// The image sent with the surrounding image: the visible composite of `window` over white,
/// converted to 8-bit sRGB, with the selection outlined in red ([`Outline`] for `expand` and
/// `feather`), as a PNG shrunk to at most `max_side` px on the long side.
fn context_images(doc: &Document, sel: &Surface, window: Rect, max_side: usize, expand: f32, feather: f32) -> Result<ContextImages> {
    let (w, h) = (window.width() as usize, window.height() as usize);
    let too_large = || EngineError::Other("the area around the selection is too large to send: select a smaller area".into());
    let n = w.checked_mul(h).filter(|n| *n > 0 && *n as u64 <= MAX_CONTEXT_AREA).ok_or_else(too_large)?;
    let buf = photocraft_compose::render(doc, window).over_background([1.0; 3]);
    if buf.px.len() != n {
        return Err(too_large());
    }
    let mut rgb: Vec<f32> = buf.px.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    drop(buf);
    let src = crate::color_cmds::composite_profile(doc);
    let opts = photocraft_cms::TransformOptions { intent: photocraft_cms::Intent::RelativeColorimetric, bpc: true, ..Default::default() };
    let to_srgb =
        photocraft_cms::cached(&src, photocraft_cms::Builtin::Srgb.profile(), opts).map_err(|e| EngineError::Other(format!("colour management: {e}")))?;
    to_srgb.apply(&mut rgb, 3);
    let cover = crate::fill_cmds::read_window_mask(sel, window, &|| false).ok_or_else(too_large)?;
    if cover.len() != n {
        return Err(EngineError::Other("could not read the selection".into()));
    }
    let luma = crate::genai_align::luma(&rgb);
    let outline = Outline::new(expand, feather, (w, h), max_side);
    draw_outline(&mut rgb, &cover, w, h, outline);
    let (photo, pw, ph) = downscale(&rgb, w, h, max_side);
    let photo = png(&photo, pw, ph)?;
    Ok(ContextImages { photo, luma, cover, outline })
}

/// How far to move the reply so it lines up with the original window around the selection
/// ([`crate::genai_align::estimate`]); no offset when the reply can't be read or nothing clearly
/// better is found. The reply is compared as sRGB, as it was sent.
fn align_reply(generated: &photocraft_genai::Generated, reference: &ContextImages, window: Rect) -> crate::genai_align::Alignment {
    let Ok(imported) = photocraft_io::import(&format!("generated.{}", reply_ext(&generated.mime_type)), &generated.bytes) else { return Default::default() };
    let src = imported.document;
    let buf = photocraft_compose::render(&src, src.bounds()).over_background([1.0; 3]);
    let rgb: Vec<f32> = buf.px.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let (w, h) = (window.width() as usize, window.height() as usize);
    let max_shift = (w.max(h) as f32 * ALIGN_SHIFT_FRACTION).clamp(ALIGN_SHIFT_MIN, ALIGN_SHIFT_MAX);
    // Compare just outside the outline: past where the mask shows the reply, and not on the line.
    let size = (src.size.width as usize, src.size.height as usize);
    let reach = crate::genai_align::Reach { gap: reference.outline.ring_gap(), band: ALIGN_BAND, max_shift };
    crate::genai_align::estimate(&reference.luma, &reference.cover, (w, h), &crate::genai_align::luma(&rgb), size, reach)
}

/// Whether a reply lined up as `a` is the picture: rejected (with the reason) when its ring
/// outside the selection is unlike the original's ([`MIN_SIMILARITY`]). A ring that can't tell
/// (too small, flat) accepts it.
fn verdict(a: &crate::genai_align::Alignment) -> std::result::Result<(), String> {
    match a.similarity {
        Some(sim) if sim < MIN_SIMILARITY => Err(format!(
            "Gemini's result doesn't match the picture around the selection (similarity {sim:.2}), so nothing was added. Try again, or rephrase the prompt."
        )),
        _ => Ok(()),
    }
}

/// Runs `attempt` (given its 1-based number) until it returns an accepted result, at most
/// `max` times. `Err` from `attempt` (network, cancellation) ends at once; `Ok(Err(reason))` is a
/// rejected result, tried again while attempts remain, and the last reason is the error.
/// Returns the result and how many attempts it took.
fn with_retry<T>(max: usize, mut attempt: impl FnMut(usize) -> Result<std::result::Result<T, String>>) -> Result<(T, usize)> {
    let mut last = String::from("no attempt was made");
    for n in 1..=max {
        match attempt(n)? {
            Ok(t) => return Ok((t, n)),
            Err(reason) => last = reason,
        }
    }
    Err(EngineError::Other(last))
}

/// The file extension `photocraft_io::import` should see for a reply of `mime`.
fn reply_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// `(dx, dy)` limited so the window moved by it still covers `need` (the mask's bounds): the
/// shifted reply must leave no gap under the mask.
fn clamp_shift((dx, dy): (i32, i32), window: Rect, need: Rect) -> (i32, i32) {
    if need.is_empty() || !window.contains_rect(&need) {
        return (0, 0);
    }
    (dx.clamp(need.x1 - window.x1, need.x0 - window.x0), dy.clamp(need.y1 - window.y1, need.y0 - window.y0))
}

/// `sel` grown by `r` px (within `canvas`), for the layer mask; `sel` itself when `r` is 0 or the
/// area is too large to grow.
fn expanded_mask(sel: &Surface, r: f32, canvas: Rect) -> Surface {
    if r <= 0.0 || sel.channels() != 1 {
        return sel.clone();
    }
    let reach = r.ceil() as i32 + 2;
    let area = sel.content_bounds().intersect(&canvas).inflate(reach).intersect(&canvas);
    let (w, h) = (area.width() as usize, area.height() as usize);
    if area.is_empty() || (w as u64) * (h as u64) > MAX_CONTEXT_AREA {
        return sel.clone();
    }
    let Some(m) = crate::fill_cmds::read_window_mask(sel, area, &|| false) else { return sel.clone() };
    let grown = photocraft_algo::selection::expand(&m, w, h, r);
    if grown.len() != w * h {
        return sel.clone();
    }
    let mut out = sel.clone();
    out.write_region(area, &grown);
    out
}

/// Converts the decoded reply from sRGB (what the models produce, and what
/// [`context_images`] sends) to the document's profile `dst`, so the round trip leaves colours
/// alone in Display P3, Adobe RGB, CMYK, Lab or gray documents too. An embedded profile in the
/// reply wins over the sRGB assumption.
fn to_document_colors(reply: &mut Document, dst: &photocraft_cms::Profile, mode: photocraft_color::ColorMode) -> Result<()> {
    if reply.icc_profile.is_none() {
        reply.icc_profile = Some(photocraft_cms::Builtin::Srgb.profile().to_bytes());
    }
    let src = crate::color_cmds::document_profile(reply);
    if reply.mode == mode && src.to_bytes() == dst.to_bytes() {
        return Ok(());
    }
    crate::color_cmds::convert_document(reply, dst, photocraft_cms::Intent::RelativeColorimetric, true)
}

/// How the reply goes onto the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fit {
    /// Scaled uniformly to cover the rect, centred (text only).
    Cover(Rect),
    /// Scaled to exactly the rect (the surrounding image's window, already at the reply's ratio).
    Exact(Rect),
}

impl Fit {
    fn rect(self) -> Rect {
        match self {
            Fit::Cover(r) | Fit::Exact(r) => r,
        }
    }

    /// The x and y scales and the top-left offset for a `w × h` reply.
    fn placement(self, w: u32, h: u32) -> Option<(f64, f64, i32, i32)> {
        match self {
            Fit::Cover(target) => cover(w, h, target).map(|(s, dx, dy)| (s, s, dx, dy)),
            Fit::Exact(window) => {
                if w == 0 || h == 0 || window.is_empty() {
                    return None;
                }
                Some((f64::from(window.width()) / f64::from(w), f64::from(window.height()) / f64::from(h), window.x0, window.y0))
            }
        }
    }
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
    let prm = params(p, &s.prefs().ai)?;
    has_selection(s).map_err(bad)?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let doc = st.doc.clone();
    let active = st.active_layer;
    let canvas = doc.bounds();
    let sel: Surface = doc.selection.clone().ok_or_else(|| bad("no selection"))?;
    let hb = sel.content_bounds().intersect(&canvas);
    let (fit, ratio) = if prm.context {
        let (window, ratio) = context_window(hb, canvas).ok_or_else(|| bad("the selection is outside the canvas"))?;
        if u64::from(window.width()) * u64::from(window.height()) > MAX_CONTEXT_AREA {
            return Err(bad("the selection is too large to send with its surroundings: select a smaller area, or turn off Use Surrounding Image"));
        }
        (Fit::Exact(window), ratio)
    } else {
        if u64::from(hb.width()) * u64::from(hb.height()) > MAX_AREA {
            return Err(bad("the selection is too large for Generative Fill"));
        }
        (Fit::Cover(hb), photocraft_genai::nearest_aspect_ratio(hb.width(), hb.height()).0)
    };
    let config = photocraft_genai::Config::from_env(prm.api_key.as_deref()).map_err(|e| EngineError::Other(e.to_string()))?;
    let area = fit.rect();
    let image_size = prm.image_size.clone().unwrap_or_else(|| photocraft_genai::image_size_for(area.width().max(area.height())).to_string());
    let request = photocraft_genai::Request {
        model: prm.model.clone(),
        prompt: if prm.context { context_prompt(&prm.prompt) } else { prm.prompt.clone() },
        images: Vec::new(),
        aspect_ratio: Some(ratio.to_string()),
        image_size: Some(image_size.clone()),
    };
    let name = layer_name(&prm.prompt);
    let (feather, expand, context) = (prm.feather, prm.expand, prm.context);
    let sample = prm.sample;
    let info = json!({"model": prm.model, "aspectRatio": ratio, "imageSize": image_size, "context": context, "sampleLayer": sample.id()});
    let max_side = size_px(&image_size);
    crate::jobs::run(
        s,
        LABEL,
        true,
        move |ctx| {
            let mut request = request;
            let mut reference = None;
            if let Fit::Exact(window) = fit {
                ctx.progress(0.02, "Reading the area around the selection…");
                let read = sampled_doc(&doc, active, sample);
                let mut images = context_images(&read, &sel, window, max_side, expand, feather)?;
                request.images = vec![photocraft_genai::InputImage { mime_type: "image/png".into(), bytes: std::mem::take(&mut images.photo) }];
                reference = Some((window, images));
                ctx.check()?;
            }
            // Generate, then line the reply up and check it is the picture; a rejected reply is
            // asked for once more.
            let ((generated, found), attempts) = with_retry(MAX_ATTEMPTS, |n| {
                let what = if n == 1 { "Generating with Gemini…" } else { "The result didn't match the picture: generating again…" };
                ctx.progress(if n == 1 { 0.05 } else { 0.45 }, what);
                let generated = photocraft_genai::generate(&request, &config).map_err(|e| EngineError::Other(e.to_string()))?;
                ctx.check()?;
                let Some((window, images)) = &reference else { return Ok(Ok((generated, None))) };
                ctx.progress(0.85, "Lining the result up…");
                let a = align_reply(&generated, images, *window);
                ctx.check()?;
                Ok(verdict(&a).map(|()| (generated, Some(a))))
            })?;
            let mask = expanded_mask(&sel, expand, canvas);
            let shift = match (&reference, &found) {
                (Some((window, _)), Some(a)) => clamp_shift((a.dx, a.dy), *window, mask.content_bounds().intersect(&canvas)),
                _ => (0, 0),
            };
            ctx.check()?;
            ctx.progress(0.95, "Placing the result…");
            Ok((generated, mask, shift, found, attempts))
        },
        move |s, (generated, mask, shift, found, attempts)| {
            let mut info = info;
            if let Some(o) = info.as_object_mut() {
                o.insert("attempts".into(), json!(attempts));
                if let Some(a) = found {
                    o.insert(
                        "alignment".into(),
                        json!({"shift": [shift.0, shift.1], "found": [a.dx, a.dy], "error": a.error, "errorUnshifted": a.error_unshifted, "similarity": a.similarity}),
                    );
                }
            }
            place(s, generated, mask, fit, shift, &name, feather, info)
        },
    )
}

/// Decode the generated image, scale it per `fit`, move it by `shift` (the alignment), and add it
/// above the active layer with `mask` as its layer mask, as one undo step.
#[allow(clippy::too_many_arguments)]
fn place(
    s: &mut Session,
    generated: photocraft_genai::Generated,
    mask: Surface,
    fit: Fit,
    shift: (i32, i32),
    name: &str,
    feather: f32,
    info: Value,
) -> Result<Value> {
    let target = &s.active().ok_or(EngineError::NoDocument)?.doc;
    let fmt = target.pixel_format();
    let dst_profile = crate::color_cmds::document_profile(target);
    let imported = photocraft_io::import(&format!("generated.{}", reply_ext(&generated.mime_type)), &generated.bytes)
        .map_err(|e| EngineError::Other(format!("could not read the generated image: {e}")))?;
    let mut src = imported.document;
    to_document_colors(&mut src, &dst_profile, fmt.mode)?;
    let (w, h) = (src.size.width, src.size.height);
    let (sx, sy, dx, dy) = fit.placement(w, h).ok_or_else(|| EngineError::Other("the generated image is empty".into()))?;
    let px = crate::file_cmds::flattened(&src, fmt);
    // Edges repeat outwards while scaling, so the result stays opaque up to its border.
    let px = if (sx - 1.0).abs() > 1e-9 || (sy - 1.0).abs() > 1e-9 { resize_surface_in_canvas(&px, sx, sy, Resample::Bicubic, src.bounds()) } else { px };
    let px = translate_surface(&px, dx.saturating_add(shift.0), dy.saturating_add(shift.1));
    let mask = LayerMask { surface: mask, feather, ..LayerMask::reveal_all() };
    let layer_name = name.to_string();
    let id = s.edit(LABEL, |doc, active| {
        let mut layer = Layer::new(layer_name, LayerContent::Raster(px));
        layer.mask = Some(mask);
        let id = doc.insert_above(*active, layer);
        *active = Some(id);
        Ok(id)
    })?;
    let r = fit.rect();
    let mut out = json!({
        "layer": id.0,
        "bounds": [r.x0, r.y0, r.x1, r.y1],
        "generatedSize": [w, h],
        "scale": sx.max(sy) * 100.0,
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
        params: r##"{"prompt":string (what to generate in the selection; may be empty with context: remove what is selected), "context":bool? (send the area around the selection so the result matches it; default Preferences › AI Generation › Use Surrounding Image, on), "model":string? (default gemini-nano-banana-2.1), "imageSize":"auto"|"1K"|"2K"|"4K"?, "expand":px? (mask growth, 0..100; default 4 with context, else 0), "sampleLayer":"all"|"currentAndBelow"|"current"? (which layers the surroundings are read from; default all), "feather":px? (mask feather; default 4 with context, else 0), "apiKey":string?} (model, imageSize and apiKey default to Edit › Preferences › AI Generation; the key then to the GEMINI_API_KEY environment variable) -> a new layer above the active one, masked by the selection"##,
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
            json!({"context": false}),
            json!({"prompt": "   ", "context": false}),
            json!({"prompt": 5}),
            json!({"prompt": "x", "context": "yes"}),
            json!({"prompt": "x", "sampleLayer": "everything"}),
            json!({"prompt": "x", "sampleLayer": 2}),
            json!({"prompt": "x", "model": 3}),
            json!({"prompt": "x", "model": "has space"}),
            json!({"prompt": "x", "imageSize": "8K"}),
            json!({"prompt": "x", "feather": -1}),
            json!({"prompt": "x", "feather": "wide"}),
            json!({"prompt": "x", "expand": -1}),
            json!({"prompt": "x", "expand": 101}),
            json!({"prompt": "x".repeat(photocraft_genai::MAX_PROMPT_CHARS + 1)}),
        ] {
            assert!(matches!(s.execute(ID, p.clone()), Err(EngineError::BadParams { .. })), "{p}");
        }
    }

    #[test]
    fn preferences_fill_in_what_the_params_leave_out() {
        let ai = crate::prefs::Ai { model: "gemini-3-pro-image".into(), api_key: " pref-key ".into(), image_size: "2K".into(), use_surrounding_image: false };
        let p = params(&json!({"prompt": "a cat"}), &ai).unwrap();
        assert_eq!((p.model.as_str(), p.image_size.as_deref(), p.api_key.as_deref(), p.context), ("gemini-3-pro-image", Some("2K"), Some("pref-key"), false));
        assert_eq!((p.expand, p.feather), (0.0, 0.0), "text only: the mask is the selection");
        let p =
            params(&json!({"prompt": "a cat", "model": "gemini-3.1-flash-image", "imageSize": "auto", "apiKey": "param-key", "context": true}), &ai).unwrap();
        assert_eq!((p.model.as_str(), p.image_size.as_deref(), p.api_key.as_deref(), p.context), ("gemini-3.1-flash-image", None, Some("param-key"), true));
        // Empty or unusable saved values fall back: default model, auto size, no key (environment).
        let empty = crate::prefs::Ai { model: " ".into(), api_key: "".into(), image_size: "huge".into(), use_surrounding_image: true };
        let p = params(&json!({"prompt": "a cat"}), &empty).unwrap();
        assert_eq!((p.model.as_str(), p.image_size, p.api_key), (photocraft_genai::DEFAULT_MODEL, None, None));
    }

    #[test]
    fn the_surrounding_image_is_the_default_and_allows_an_empty_prompt() {
        let ai = crate::prefs::Ai::default();
        assert!(ai.use_surrounding_image);
        let p = params(&json!({}), &ai).unwrap();
        assert!(p.context && p.prompt.is_empty());
        assert_eq!((p.expand, p.feather), (CONTEXT_EXPAND, CONTEXT_FEATHER), "the seam is hidden by default");
        let p = params(&json!({"prompt": "a boat", "expand": 0, "feather": 10}), &ai).unwrap();
        assert_eq!((p.expand, p.feather), (0.0, 10.0));
        assert!(params(&json!({"context": false}), &ai).is_err(), "text only still needs a prompt");
    }

    #[test]
    fn the_surroundings_can_be_read_from_fewer_layers() {
        // White background, a red result of an earlier fill above it.
        let mut s = session_with_selection();
        let bg = s.active().unwrap().active_layer.unwrap();
        let sel = s.active().unwrap().doc.selection.clone().unwrap();
        let red = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png_file(64, 48), text: None };
        place(&mut s, red, sel, Fit::Exact(Rect::new(0, 0, 64, 48)), (0, 0), "earlier", 0.0, json!({})).unwrap();
        let st = s.active().unwrap();
        let top = st.active_layer.unwrap();
        let doc = &st.doc;
        let pixel = |d: &Document| photocraft_compose::render(d, Rect::new(20, 12, 21, 13)).px[0];
        let is_red = |p: [f32; 4]| p[0] > 0.99 && p[1] < 0.01;
        assert!(is_red(pixel(&sampled_doc(doc, Some(bg), Sample::All))), "all: the earlier result shows");
        assert!(!is_red(pixel(&sampled_doc(doc, Some(bg), Sample::Current))), "current = the background alone");
        assert!(!is_red(pixel(&sampled_doc(doc, Some(bg), Sample::CurrentAndBelow))), "nothing below the background");
        assert!(is_red(pixel(&sampled_doc(doc, Some(top), Sample::CurrentAndBelow))), "from the top: everything");
        assert!(is_red(pixel(&sampled_doc(doc, None, Sample::Current))), "no active layer: as shown");
        // The document itself is left alone.
        assert!(doc.walk().iter().all(|(_, _, l)| l.visible));
        let p = params(&json!({"prompt": "x", "sampleLayer": "current"}), &crate::prefs::Ai::default()).unwrap();
        assert_eq!(p.sample, Sample::Current);
        assert_eq!(params(&json!({"prompt": "x"}), &crate::prefs::Ai::default()).unwrap().sample, Sample::All);
    }

    #[test]
    fn the_context_prompt_asks_to_change_only_the_outlined_area() {
        let p = context_prompt("  a  red\nboat ");
        assert!(p.contains("change it to: a red boat."), "{p}");
        assert!(p.contains("ONLY the area inside the red outline") && p.contains("without the red outline") && !p.contains("mask"));
        let remove = context_prompt(" ");
        assert!(remove.contains("remove what is there"), "{remove}");
        assert!(!remove.contains("change it to"));
    }

    #[test]
    fn the_saved_key_is_never_in_debug_output() {
        let ai = crate::prefs::Ai { api_key: "secret-xyz".into(), ..Default::default() };
        assert!(!format!("{ai:?}").contains("secret-xyz"));
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
        assert_eq!(layer_name("  "), "Generative Fill");
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

    #[test]
    fn exact_fit_stretches_to_the_window() {
        let (sx, sy, dx, dy) = Fit::Exact(Rect::new(10, 20, 310, 220)).placement(1536, 1024).unwrap();
        assert!((sx - 300.0 / 1536.0).abs() < 1e-12 && (sy - 200.0 / 1024.0).abs() < 1e-12);
        assert_eq!((dx, dy), (10, 20));
        assert!(Fit::Exact(Rect::new(0, 0, 10, 10)).placement(0, 5).is_none());
        assert!(Fit::Exact(Rect::new(0, 0, 0, 10)).placement(5, 5).is_none());
    }

    /// `window` is inside `canvas`, covers `hb`, and has the shape of the ratio `name`.
    fn check_window(hb: Rect, canvas: Rect) -> (Rect, &'static str) {
        let (w, name) = context_window(hb, canvas).unwrap();
        assert!(canvas.contains_rect(&w), "{w:?} inside {canvas:?}");
        assert!(w.contains_rect(&hb.intersect(&canvas)), "{w:?} covers {hb:?}");
        let (_, rw, rh) = photocraft_genai::ASPECT_RATIOS.iter().copied().find(|r| r.0 == name).unwrap();
        let got = f64::from(w.width()) / f64::from(w.height());
        let want = f64::from(rw) / f64::from(rh);
        assert!((got / want - 1.0).abs() < 0.02, "{w:?} is {got}, not {name}");
        (w, name)
    }

    #[test]
    fn the_context_window_pads_and_takes_a_supported_ratio() {
        let canvas = Rect::new(0, 0, 2100, 1500);
        // A small box in the middle: padded by 64 px (more than a quarter of 200), then 3:2.
        let hb = Rect::new(900, 700, 1100, 800);
        let (w, name) = check_window(hb, canvas);
        assert_eq!(name, "3:2");
        assert!(w.x0 <= 900 - 64 && w.y0 <= 700 - 64 && w.x1 >= 1100 + 64 && w.y1 >= 800 + 64, "{w:?}");
        // A large box: a quarter of its long side.
        let (w, _) = check_window(Rect::new(400, 400, 1200, 800), canvas);
        assert!(w.x0 <= 200 && w.x1 >= 1400, "{w:?}");
        // Corners and edges push the window back inside the canvas.
        check_window(Rect::new(0, 0, 100, 100), canvas);
        check_window(Rect::new(2000, 1400, 2100, 1500), canvas);
        check_window(Rect::new(0, 600, 2100, 700), canvas);
        // A one-pixel selection still gets a context window.
        let (w, _) = check_window(Rect::new(1000, 700, 1001, 701), canvas);
        assert!(w.width() >= 129 && w.height() >= 129);
        // Tall boxes take a portrait ratio.
        let (_, name) = check_window(Rect::new(1000, 100, 1100, 1400), canvas);
        assert!(["9:16", "2:3", "3:4"].contains(&name), "{name}");
    }

    #[test]
    fn the_context_window_falls_back_when_no_ratio_fits() {
        // The whole canvas of a ratio the models don't have (7:5): nothing can grow or shrink.
        let canvas = Rect::new(0, 0, 2100, 1500);
        let (w, name) = context_window(canvas, canvas).unwrap();
        assert_eq!(w, canvas);
        assert_eq!(name, "4:3", "the nearest ratio; the reply is stretched 5 %");
        // A thin strip of a canvas: still inside, still covering.
        let strip = Rect::new(0, 0, 40, 3000);
        let (w, _) = context_window(Rect::new(0, 1000, 40, 2000), strip).unwrap();
        assert!(strip.contains_rect(&w) && w.contains_rect(&Rect::new(0, 1000, 40, 2000)));
        // Off the canvas: none.
        assert!(context_window(Rect::new(5000, 5000, 5100, 5100), canvas).is_none());
        assert!(context_window(Rect::new(0, 0, 0, 0), canvas).is_none());
    }

    #[test]
    fn spans_stay_inside_and_cover() {
        assert_eq!(place_span(10, 20, (0, 100), (8, 12)), Some(5), "centred on 10");
        assert_eq!(place_span(10, 0, (0, 100), (0, 4)), Some(0), "pushed in from the left edge");
        assert_eq!(place_span(10, 200, (0, 100), (96, 100)), Some(90), "pushed in from the right edge");
        assert_eq!(place_span(10, 20, (0, 100), (0, 20)), None, "too short to cover");
        assert_eq!(place_span(200, 20, (0, 100), (0, 20)), None, "longer than the canvas");
        assert_eq!(place_span(0, 20, (0, 100), (5, 5)), None);
    }

    #[test]
    fn the_outline_runs_where_the_mask_hides_the_reply() {
        let o = Outline::new(CONTEXT_EXPAND, CONTEXT_FEATHER, (1000, 800), 2048);
        // The layer shows the reply up to expand + 3 sigmas = 16 px out; the line starts 8 px later.
        assert_eq!((o.start, o.width), (24.0, 3.0));
        assert!(o.ring_gap() > o.start + o.width, "the alignment ring is clear of the line");
        // A window shrunk 4× for sending gets a 4× wider line: still 3 px in the image sent.
        assert_eq!(Outline::new(CONTEXT_EXPAND, CONTEXT_FEATHER, (4096, 2000), 1024).width, 12.0);
        assert_eq!(Outline::new(0.0, 0.0, (10, 10), 0).start, OUTLINE_CLEARANCE);
        // Drawn around the selection's shape, outside it only.
        let (w, h) = (100, 100);
        let cover: Vec<f32> = (0..w * h).map(|i| if (40..60).contains(&(i % w)) && (40..60).contains(&(i / w)) { 1.0 } else { 0.0 }).collect();
        let mut rgb = vec![0.5f32; w * h * 3];
        draw_outline(&mut rgb, &cover, w, h, Outline { start: 10.0, width: 3.0 });
        let px = |x: usize, y: usize| [rgb[(y * w + x) * 3], rgb[(y * w + x) * 3 + 1], rgb[(y * w + x) * 3 + 2]];
        assert_eq!(px(71, 50), OUTLINE_COLOR, "12 px right of the selection's last column");
        assert_eq!(px(28, 50), OUTLINE_COLOR, "12 px left of its first");
        assert_eq!(px(50, 50), [0.5; 3], "inside: untouched");
        assert_eq!(px(65, 50), [0.5; 3], "nearer than the start: untouched");
        assert_eq!(px(75, 50), [0.5; 3], "past the width: untouched");
    }

    #[test]
    fn downscale_averages_areas_and_keeps_small_images() {
        // 4×2: left half black, right half white → 2×1 at most 2 px: black, white.
        let rgb: Vec<f32> = (0..8).flat_map(|i| if i % 4 < 2 { [0.0; 3] } else { [1.0; 3] }).collect();
        let (out, w, h) = downscale(&rgb, 4, 2, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
        let (same, w, h) = downscale(&rgb, 4, 2, 1024);
        assert_eq!((same, w, h), (rgb.clone(), 4, 2));
        assert_eq!(downscale(&rgb, 5, 2, 2).1, 5, "a buffer of the wrong length is left alone");
    }

    #[test]
    fn context_images_are_srgb_pngs_of_the_window() {
        let mut s = session_with_selection();
        s.execute("edit.fill", json!({"use": "color", "color": "#336699"})).ok();
        let st = s.active().unwrap();
        let sel = st.doc.selection.clone().unwrap();
        let window = Rect::new(0, 0, 60, 40);
        let ContextImages { photo, luma, cover, .. } = context_images(&st.doc, &sel, window, 1024, CONTEXT_EXPAND, CONTEXT_FEATHER).unwrap();
        assert_eq!((luma.len(), cover.len()), (60 * 40, 60 * 40), "kept at full size for the alignment");
        let size = |png: &[u8]| {
            let i = photocraft_codecs::decode(png).unwrap();
            (i.width(), i.height())
        };
        assert_eq!(size(&photo), (60, 40));
        // Shrunk to the output size's long side.
        let small = context_images(&st.doc, &sel, window, 30, CONTEXT_EXPAND, CONTEXT_FEATHER).unwrap().photo;
        assert_eq!(size(&small), (30, 20));
        // Absurd windows are refused before anything is read.
        assert!(context_images(&st.doc, &sel, Rect::new(0, 0, 100_000, 100_000), 1024, CONTEXT_EXPAND, CONTEXT_FEATHER).is_err());
    }

    #[test]
    fn the_mask_grows_by_the_expand_radius() {
        let s = session_with_selection();
        let st = s.active().unwrap();
        let sel = st.doc.selection.clone().unwrap();
        let canvas = st.doc.bounds();
        assert_eq!(expanded_mask(&sel, 4.0, canvas).content_bounds(), Rect::new(4, 4, 44, 28));
        assert_eq!(expanded_mask(&sel, 0.0, canvas).content_bounds(), sel.content_bounds());
        // Growth stops at the canvas edge.
        assert!(canvas.contains_rect(&expanded_mask(&sel, 50.0, canvas).content_bounds()));
    }

    /// A tiny PNG, encoded through the io crate so the test needs no fixtures.
    fn png_file(w: u32, h: u32) -> Vec<u8> {
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
        let generated = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png_file(64, 32), text: Some("ok".into()) };
        let out = place(&mut s, generated, sel.clone(), Fit::Cover(target), (0, 0), "Generative Fill: test", 2.0, json!({"model": "m"})).unwrap();
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
    fn a_context_reply_fills_the_window_exactly_under_a_grown_mask() {
        let mut s = session_with_selection();
        let st = s.active().unwrap();
        let sel = st.doc.selection.clone().unwrap();
        let canvas = st.doc.bounds();
        let (window, ratio) = context_window(sel.content_bounds(), canvas).unwrap();
        assert!(window.contains_rect(&Rect::new(8, 8, 40, 24)));
        let mask = expanded_mask(&sel, CONTEXT_EXPAND, canvas);
        // The model answers at its own resolution (here 3× the window, a rounded ratio).
        let generated = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png_file(window.width() * 3, window.height() * 3 + 1), text: None };
        let out = place(&mut s, generated, mask, Fit::Exact(window), (0, 0), "Generative Fill", CONTEXT_FEATHER, json!({"aspectRatio": ratio})).unwrap();
        assert_eq!(out["bounds"], json!([window.x0, window.y0, window.x1, window.y1]));
        let st = s.active().unwrap();
        let layer = st.doc.layer(st.active_layer.unwrap()).unwrap();
        let LayerContent::Raster(px) = &layer.content else { panic!("a pixel layer") };
        let got = px.content_bounds();
        assert!(got.contains_rect(&window) && window.inflate(1).contains_rect(&got), "{got:?} is the window {window:?}");
        let mask = layer.mask.as_ref().unwrap();
        assert_eq!(mask.surface.content_bounds(), Rect::new(4, 4, 44, 28), "the selection grown by 4 px");
        assert_eq!(mask.feather, CONTEXT_FEATHER);
    }

    #[test]
    fn the_reply_is_converted_from_srgb_to_the_documents_profile() {
        // sRGB pure red is less saturated than P3's red: pasting the sRGB numbers into a P3
        // document unconverted would oversaturate it.
        let mut s = session_with_selection();
        s.execute("edit.assignProfile", json!({"profile": "display-p3"})).unwrap();
        let st = s.active().unwrap();
        assert!(crate::color_cmds::document_profile(&st.doc).description.contains("P3"), "the document is P3");
        let sel = st.doc.selection.clone().unwrap();
        let red = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png_file(32, 16), text: None };
        place(&mut s, red, sel, Fit::Exact(Rect::new(8, 8, 40, 24)), (0, 0), "x", 0.0, json!({})).unwrap();
        let st = s.active().unwrap();
        let LayerContent::Raster(px) = &st.doc.layer(st.active_layer.unwrap()).unwrap().content else { panic!("a pixel layer") };
        let c = px.read_region(Rect::new(20, 12, 21, 13));
        assert!(c[0] < 0.95 && c[1] > 0.1 && c[2] > 0.05, "sRGB red in P3 is about (0.92, 0.20, 0.14): {c:?}");
        // In an sRGB document the numbers stay as they are.
        let mut s = session_with_selection();
        let sel = s.active().unwrap().doc.selection.clone().unwrap();
        let red = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png_file(32, 16), text: None };
        place(&mut s, red, sel, Fit::Exact(Rect::new(8, 8, 40, 24)), (0, 0), "x", 0.0, json!({})).unwrap();
        let st = s.active().unwrap();
        let LayerContent::Raster(px) = &st.doc.layer(st.active_layer.unwrap()).unwrap().content else { panic!("a pixel layer") };
        let c = px.read_region(Rect::new(20, 12, 21, 13));
        assert!(c[0] > 0.99 && c[1] < 0.01 && c[2] < 0.01, "{c:?}");
    }

    #[test]
    fn shifts_never_uncover_the_mask() {
        let window = Rect::new(0, 0, 200, 100);
        let need = Rect::new(10, 20, 150, 90);
        assert_eq!(clamp_shift((5, -3), window, need), (5, -3));
        // Moving the window 15 px right would leave x 0..15 bare under a mask starting at 10.
        assert_eq!(clamp_shift((15, 0), window, need), (10, 0));
        assert_eq!(clamp_shift((-80, -30), window, need), (-50, -10));
        assert_eq!(clamp_shift((3, 3), window, Rect::new(0, 0, 0, 0)), (0, 0));
        assert_eq!(clamp_shift((3, 3), window, Rect::new(-5, 0, 10, 10)), (0, 0));
    }

    /// An 8-bit RGB texture value at (x, y): waves plus hashed noise, different at every offset.
    fn texel(x: i32, y: i32) -> [u8; 3] {
        let n = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) as u32;
        let noise = (n.wrapping_mul(2_654_435_761) >> 24) as f32 / 255.0;
        let v = 0.5 + 0.2 * (x as f32 * 0.21).sin() * (y as f32 * 0.13).cos() + 0.2 * (noise - 0.5);
        let g = (v.clamp(0.0, 1.0) * 255.0) as u8;
        [g, g.saturating_add(20), g / 2]
    }

    fn texture_png(x0: i32, y0: i32, w: u32, h: u32) -> Vec<u8> {
        let px: Vec<u8> = (0..h as i32).flat_map(|y| (0..w as i32).flat_map(move |x| texel(x0 + x, y0 + y))).collect();
        let image = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgb, px).unwrap();
        photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &Default::default()).unwrap()
    }

    #[test]
    fn a_shifted_reply_is_lined_up_before_it_is_placed() {
        let mut s = Session::new();
        let doc = photocraft_io::import("t.png", &texture_png(0, 0, 240, 160)).unwrap().document;
        s.add_document(doc, None);
        s.execute("select.rect", json!({"x": 90, "y": 60, "width": 60, "height": 40})).unwrap();
        let st = s.active().unwrap();
        let (doc, sel) = (st.doc.clone(), st.doc.selection.clone().unwrap());
        let (window, _) = context_window(sel.content_bounds(), doc.bounds()).unwrap();
        let reference = context_images(&doc, &sel, window, 4096, CONTEXT_EXPAND, CONTEXT_FEATHER).unwrap();
        // The "model" draws the window 4 px left and 2 px up of where it belongs.
        let bytes = texture_png(window.x0 + 4, window.y0 + 2, window.width(), window.height());
        let generated = photocraft_genai::Generated { mime_type: "image/png".into(), bytes, text: None };
        let a = align_reply(&generated, &reference, window);
        assert_eq!((a.dx, a.dy), (4, 2), "{a:?}");
        // A reply that is already in place stays there.
        let bytes = texture_png(window.x0, window.y0, window.width(), window.height());
        let generated = photocraft_genai::Generated { mime_type: "image/png".into(), bytes, text: None };
        let a = align_reply(&generated, &reference, window);
        assert_eq!((a.dx, a.dy), (0, 0), "{a:?}");
        // Placing with the shift moves the layer's pixels by it.
        let generated = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png_file(window.width(), window.height()), text: None };
        let out = place(&mut s, generated, sel, Fit::Exact(window), (4, 2), "x", 0.0, json!({})).unwrap();
        assert_eq!(out["bounds"], json!([window.x0, window.y0, window.x1, window.y1]));
        let st = s.active().unwrap();
        let LayerContent::Raster(px) = &st.doc.layer(st.active_layer.unwrap()).unwrap().content else { panic!("a pixel layer") };
        assert_eq!(px.content_bounds().x0, window.x0 + 4);
        assert_eq!(px.content_bounds().y0, window.y0 + 2);
    }

    #[test]
    fn replies_unlike_the_picture_are_rejected() {
        let a = |similarity| crate::genai_align::Alignment { similarity, ..Default::default() };
        assert!(verdict(&a(Some(0.93))).is_ok());
        assert!(verdict(&a(None)).is_ok(), "a ring that can't tell accepts the reply");
        let e = verdict(&a(Some(0.05))).unwrap_err();
        assert!(e.contains("doesn't match") && e.contains("0.05"), "{e}");
    }

    #[test]
    fn only_one_image_is_sent_with_the_outline_outside_the_selection() {
        let mut s = Session::new();
        let doc = photocraft_io::import("t.png", &texture_png(0, 0, 240, 160)).unwrap().document;
        s.add_document(doc, None);
        s.execute("select.rect", json!({"x": 90, "y": 60, "width": 60, "height": 40})).unwrap();
        let st = s.active().unwrap();
        let (doc, sel) = (st.doc.clone(), st.doc.selection.clone().unwrap());
        let (window, _) = context_window(sel.content_bounds(), doc.bounds()).unwrap();
        let reference = context_images(&doc, &sel, window, 4096, CONTEXT_EXPAND, CONTEXT_FEATHER).unwrap();
        let decoded = photocraft_codecs::decode(&reference.photo).unwrap();
        let rgba = decoded.to_rgba8();
        let at = |x: i32, y: i32| {
            let i = (((y - window.y0) as u32 * decoded.width() + (x - window.x0) as u32) * 4) as usize;
            [rgba[i], rgba[i + 1], rgba[i + 2]]
        };
        // The selection's last column is 149; the line starts 24 px past it.
        let close = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2);
        assert_eq!(at(174, 80), [255, 0, 0], "the outline");
        assert!(close(at(120, 80), texel(120, 80)), "inside the selection: the photo itself");
        assert!(close(at(165, 80), texel(165, 80)), "where the layer can show the reply: no line");
    }

    #[test]
    fn a_blank_or_unrelated_reply_is_rejected_and_the_picture_accepted() {
        let mut s = Session::new();
        let doc = photocraft_io::import("t.png", &texture_png(0, 0, 240, 160)).unwrap().document;
        s.add_document(doc, None);
        s.execute("select.rect", json!({"x": 90, "y": 60, "width": 60, "height": 40})).unwrap();
        let st = s.active().unwrap();
        let (doc, sel) = (st.doc.clone(), st.doc.selection.clone().unwrap());
        let (window, _) = context_window(sel.content_bounds(), doc.bounds()).unwrap();
        let reference = context_images(&doc, &sel, window, 4096, CONTEXT_EXPAND, CONTEXT_FEATHER).unwrap();
        // A blank reply (black around a white selection: what a model returning a mask sends).
        let (w, h) = (window.width() as usize, window.height() as usize);
        let blank: Vec<f32> = reference.cover.iter().flat_map(|c| [*c; 3]).collect();
        let blank = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: png(&blank, w, h).unwrap(), text: None };
        let a = align_reply(&blank, &reference, window);
        assert!(verdict(&a).is_err(), "{a:?}");
        // An unrelated scene.
        let other = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: texture_png(5000, 9000, window.width(), window.height()), text: None };
        let a = align_reply(&other, &reference, window);
        assert!(verdict(&a).is_err(), "{a:?}");
        // A faithful picture passes, with a similarity close to 1, even with the outline left in.
        let picture = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: reference.photo.clone(), text: None };
        let a = align_reply(&picture, &reference, window);
        assert!(verdict(&a).is_ok() && a.similarity.unwrap() > 0.9, "{a:?}");
    }

    #[test]
    fn a_rejected_reply_is_asked_for_once_more() {
        // Accepted at once.
        assert_eq!(with_retry(MAX_ATTEMPTS, |_| Ok(Ok::<_, String>("ok"))).unwrap(), ("ok", 1));
        // Rejected, then accepted.
        let mut calls = 0;
        let r = with_retry(MAX_ATTEMPTS, |n| {
            calls += 1;
            Ok(if n == 1 { Err("not the picture".to_string()) } else { Ok(n) })
        });
        assert_eq!((r.unwrap(), calls), ((2, 2), 2));
        // Rejected every time: the last reason is the error, after exactly MAX_ATTEMPTS requests.
        let mut calls = 0;
        let r = with_retry(MAX_ATTEMPTS, |n| {
            calls += 1;
            Ok(Err::<(), _>(format!("miss {n}")))
        });
        assert!(matches!(r, Err(EngineError::Other(ref m)) if m == "miss 2"), "{r:?}");
        assert_eq!(calls, MAX_ATTEMPTS);
        // A network error or a cancellation is not retried.
        let mut calls = 0;
        let r = with_retry(MAX_ATTEMPTS, |_| {
            calls += 1;
            Err::<std::result::Result<(), String>, _>(EngineError::Cancelled)
        });
        assert!(matches!(r, Err(EngineError::Cancelled)));
        assert_eq!(calls, 1);
        assert!(with_retry(0, |_| Ok(Ok::<_, String>(()))).is_err());
    }

    #[test]
    fn a_jpeg_reply_is_placed_too() {
        // The image models answer in JPEG only (`photocraft_genai::OUTPUT_MIME`).
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 48, "height": 24, "background": "#00ff00"})).unwrap();
        let doc = &s.active().unwrap().doc;
        let jpeg = photocraft_io::export(doc, "jpg", &photocraft_io::ExportOptions::default()).unwrap().bytes;
        assert!(jpeg.starts_with(&[0xFF, 0xD8]), "a JPEG file");
        let mut s = session_with_selection();
        let sel = s.active().unwrap().doc.selection.clone().unwrap();
        let generated = photocraft_genai::Generated { mime_type: photocraft_genai::OUTPUT_MIME.into(), bytes: jpeg, text: None };
        let out = place(&mut s, generated, sel, Fit::Cover(Rect::new(8, 8, 40, 24)), (0, 0), "Generative Fill: jpeg", 0.0, json!({})).unwrap();
        assert_eq!(out["generatedSize"], json!([48, 24]));
    }

    #[test]
    fn an_unreadable_reply_is_an_error_and_changes_nothing() {
        let mut s = session_with_selection();
        let before = s.active().unwrap().doc.layers.len();
        let sel = s.active().unwrap().doc.selection.clone().unwrap();
        let junk = photocraft_genai::Generated { mime_type: "image/png".into(), bytes: b"not an image".to_vec(), text: None };
        assert!(place(&mut s, junk, sel, Fit::Cover(Rect::new(8, 8, 40, 24)), (0, 0), "x", 0.0, json!({})).is_err());
        assert_eq!(s.active().unwrap().doc.layers.len(), before);
    }
}
