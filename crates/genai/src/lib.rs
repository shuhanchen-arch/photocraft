//! Client for Google's Gemini image-generation API, used by Edit › Generative Fill.
//!
//! The crate is deliberately small and independent of the rest of the workspace: it turns a
//! [`Request`] (prompt, optional reference images, aspect ratio, output size) into the JSON body of
//! the Gemini Interactions API, sends it over HTTPS (native builds only), and parses the reply into
//! the encoded image bytes ([`Generated`]). Decoding the image and placing it in a document is the
//! engine's job (`photocraft-engine`, `genai_cmds`).
//!
//! Every failure (no key, network, HTTP error, a refusal, a malformed reply) is an [`Error`] with a
//! message a user can act on; nothing here panics. The API key is only ever sent in the
//! `x-goog-api-key` header and never appears in an error message.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

/// The model used when none is given.
pub const DEFAULT_MODEL: &str = "gemini-nano-banana-2.1";
/// The Interactions API endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";
/// Environment variable holding the API key.
pub const API_KEY_ENV: &str = "GEMINI_API_KEY";
/// Environment variable overriding [`DEFAULT_ENDPOINT`] (a proxy or a local test server).
pub const ENDPOINT_ENV: &str = "GEMINI_API_ENDPOINT";
/// How long one request may take before it fails.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);
/// Longest prompt accepted, in characters.
pub const MAX_PROMPT_CHARS: usize = 8_000;
/// Largest reply body read, and largest decoded image accepted (bytes).
pub const MAX_RESPONSE_BYTES: u64 = 128 * 1024 * 1024;
/// Most reference images in one request (the API's documented maximum).
pub const MAX_INPUT_IMAGES: usize = 14;

/// The aspect ratios the image models accept, as (name, width, height).
pub const ASPECT_RATIOS: [(&str, u32, u32); 10] = [
    ("1:1", 1, 1),
    ("3:2", 3, 2),
    ("2:3", 2, 3),
    ("3:4", 3, 4),
    ("4:3", 4, 3),
    ("4:5", 4, 5),
    ("5:4", 5, 4),
    ("9:16", 9, 16),
    ("16:9", 16, 9),
    ("21:9", 21, 9),
];

/// The output format requested: the only one the image models accept.
pub const OUTPUT_MIME: &str = "image/jpeg";

/// The output sizes the image models accept (the `K` must be upper case).
pub const IMAGE_SIZES: [&str; 3] = ["1K", "2K", "4K"];

/// Why a generation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("no Gemini API key: enter one in Edit › Preferences › AI Generation (or set the {API_KEY_ENV} environment variable)")]
    NoApiKey,
    #[error("{0}")]
    BadRequest(String),
    #[error("could not reach the Gemini API: {0}")]
    Network(String),
    #[error("Gemini API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
    #[error("generation failed: {0}")]
    Failed(String),
    #[error("Gemini returned no image{}", .0.as_deref().map(|t| format!(": {t}")).unwrap_or_default())]
    NoImage(Option<String>),
    #[error("unexpected reply from the Gemini API: {0}")]
    Malformed(String),
    #[error("image generation is not available in the web version")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, Error>;

/// A reference image sent with the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputImage {
    /// `image/png`, `image/jpeg`, …
    pub mime_type: String,
    /// The encoded file.
    pub bytes: Vec<u8>,
}

/// One generation request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub model: String,
    pub prompt: String,
    /// Reference images, sent before the prompt in this order.
    pub images: Vec<InputImage>,
    /// One of [`ASPECT_RATIOS`]' names; `None` lets the model choose.
    pub aspect_ratio: Option<String>,
    /// One of [`IMAGE_SIZES`]; `None` lets the model choose.
    pub image_size: Option<String>,
}

impl Request {
    /// A text-only request for `prompt` with the default model.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self { model: DEFAULT_MODEL.into(), prompt: prompt.into(), images: Vec::new(), aspect_ratio: None, image_size: None }
    }

    /// Rejects what the API would reject anyway, with a clearer message.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::BadRequest(m));
        if self.model.trim().is_empty() || !self.model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/')) {
            return bad(format!("`{}` is not a valid model name", self.model));
        }
        if self.prompt.trim().is_empty() {
            return bad("the prompt is empty".into());
        }
        if self.prompt.chars().count() > MAX_PROMPT_CHARS {
            return bad(format!("the prompt is longer than {MAX_PROMPT_CHARS} characters"));
        }
        if self.images.len() > MAX_INPUT_IMAGES {
            return bad(format!("at most {MAX_INPUT_IMAGES} reference images can be sent"));
        }
        if let Some(r) = &self.aspect_ratio
            && !ASPECT_RATIOS.iter().any(|(name, _, _)| name == r)
        {
            return bad(format!("unsupported aspect ratio `{r}`"));
        }
        if let Some(s) = &self.image_size
            && !IMAGE_SIZES.contains(&s.as_str())
        {
            return bad(format!("unsupported image size `{s}` (1K, 2K or 4K)"));
        }
        Ok(())
    }

    /// The JSON body for the Interactions API.
    pub fn body(&self) -> Result<Value> {
        self.validate()?;
        let mut input: Vec<Value> =
            self.images.iter().map(|i| json!({"type": "image", "mime_type": i.mime_type, "data": BASE64.encode(&i.bytes)})).collect();
        input.push(json!({"type": "text", "text": self.prompt}));
        // The image models only produce JPEG (`image/png` is refused with HTTP 400).
        let mut format = json!({"type": "image", "mime_type": OUTPUT_MIME});
        if let Some(r) = &self.aspect_ratio {
            format["aspect_ratio"] = json!(r);
        }
        if let Some(s) = &self.image_size {
            format["image_size"] = json!(s);
        }
        // `store: false`: Google keeps neither the request nor the reply for later retrieval.
        Ok(json!({"model": self.model, "input": input, "response_format": format, "store": false}))
    }
}

/// A generated image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generated {
    pub mime_type: String,
    /// The encoded file ([`OUTPUT_MIME`] unless the model chose otherwise).
    pub bytes: Vec<u8>,
    /// Any text the model returned alongside the image.
    pub text: Option<String>,
}

/// Where and how to send requests.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub api_key: String,
    pub endpoint: String,
    pub timeout: Duration,
}

impl std::fmt::Debug for Config {
    // The key stays out of logs and debug output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config").field("api_key", &"<redacted>").field("endpoint", &self.endpoint).field("timeout", &self.timeout).finish()
    }
}

impl Config {
    /// The key given explicitly, else the one in [`API_KEY_ENV`]; the endpoint from
    /// [`ENDPOINT_ENV`], else [`DEFAULT_ENDPOINT`].
    pub fn from_env(explicit_key: Option<&str>) -> Result<Self> {
        Self::resolve(explicit_key, |name| std::env::var(name).ok())
    }

    /// [`Config::from_env`] with the environment as a function (for tests).
    pub fn resolve(explicit_key: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let key = explicit_key.map(str::to_string).or_else(|| env(API_KEY_ENV)).map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).ok_or(Error::NoApiKey)?;
        if !key.chars().all(|c| c.is_ascii_graphic()) {
            return Err(Error::BadRequest(format!("the {API_KEY_ENV} value contains spaces or non-ASCII characters")));
        }
        let endpoint = env(ENDPOINT_ENV).map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        if !(endpoint.starts_with("https://") || endpoint.starts_with("http://127.0.0.1") || endpoint.starts_with("http://localhost")) {
            return Err(Error::BadRequest(format!("{ENDPOINT_ENV} must be an https:// URL")));
        }
        Ok(Self { api_key: key, endpoint, timeout: DEFAULT_TIMEOUT })
    }
}

/// The supported aspect ratio closest to `width × height` (compared on a log scale, so 2:1 and
/// 1:2 are equally far from 1:1). Degenerate sizes give 1:1.
pub fn nearest_aspect_ratio(width: u32, height: u32) -> (&'static str, u32, u32) {
    let square = ASPECT_RATIOS[0];
    if width == 0 || height == 0 {
        return square;
    }
    let target = (f64::from(width) / f64::from(height)).ln();
    ASPECT_RATIOS
        .iter()
        .copied()
        .min_by(|a, b| {
            let d = |r: &(&str, u32, u32)| (target - (f64::from(r.1) / f64::from(r.2)).ln()).abs();
            d(a).total_cmp(&d(b))
        })
        .unwrap_or(square)
}

/// The smallest output size whose long side covers `long_side` pixels (4K beyond 2048).
pub fn image_size_for(long_side: u32) -> &'static str {
    match long_side {
        0..=1024 => "1K",
        1025..=2048 => "2K",
        _ => "4K",
    }
}

/// The text of an API error body (`{"error": {"message": …}}`), or a short excerpt of the body.
pub fn api_error_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        let e = v.get("error")?;
        e.get("message").and_then(Value::as_str).map(str::to_string).or_else(|| e.as_str().map(str::to_string))
    });
    message.unwrap_or_else(|| {
        let excerpt: String = body.chars().take(300).collect();
        if excerpt.trim().is_empty() { "empty reply".into() } else { excerpt }
    })
}

/// The image (and any text) in an Interactions API reply. The last image the model produced
/// wins; images in the echoed input are ignored.
pub fn parse_response(v: &Value) -> Result<Generated> {
    if let Some(e) = v.get("error") {
        let message = e.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string();
        let status = e.get("code").and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok()).unwrap_or(0);
        return Err(Error::Api { status, message });
    }
    let errors = || -> String {
        v.get("errors")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect::<Vec<_>>().join("; "))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "no reason given".into())
    };
    if let Some("failed" | "cancelled" | "incomplete") = v.get("status").and_then(Value::as_str) {
        return Err(Error::Failed(errors()));
    }
    let mut blocks: Vec<&Value> = Vec::new();
    if let Some(steps) = v.get("steps").and_then(Value::as_array) {
        for step in steps {
            if step.get("type").and_then(Value::as_str) == Some("user_input") {
                continue;
            }
            if let Some(content) = step.get("content").and_then(Value::as_array) {
                blocks.extend(content.iter());
            }
        }
    }
    // The SDKs' convenience shapes, in case the reply carries them.
    if let Some(outputs) = v.get("outputs").and_then(Value::as_array) {
        blocks.extend(outputs.iter());
    }
    if let Some(image) = v.get("output_image") {
        blocks.push(image);
    }
    let text: Vec<&str> = blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("text")).filter_map(|b| b.get("text").and_then(Value::as_str)).collect();
    let text = Some(text.join("\n").trim().to_string()).filter(|t| !t.is_empty());
    let image = blocks.iter().rev().find(|b| b.get("type").and_then(Value::as_str).is_none_or(|t| t == "image") && b.get("data").and_then(Value::as_str).is_some());
    let Some(image) = image else {
        return Err(Error::NoImage(text));
    };
    let data = image.get("data").and_then(Value::as_str).unwrap_or_default();
    if data.len() as u64 > MAX_RESPONSE_BYTES.saturating_mul(4) / 3 + 4 {
        return Err(Error::Malformed("the image is too large".into()));
    }
    let bytes = BASE64.decode(data.trim()).map_err(|e| Error::Malformed(format!("image data is not base64 ({e})")))?;
    if bytes.is_empty() {
        return Err(Error::Malformed("the image is empty".into()));
    }
    let mime_type = image.get("mime_type").or_else(|| image.get("mimeType")).and_then(Value::as_str).unwrap_or(OUTPUT_MIME).to_string();
    Ok(Generated { mime_type, bytes, text })
}

/// Send `request` and wait for the image. Blocking: call it from a worker thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn generate(request: &Request, config: &Config) -> Result<Generated> {
    let body = request.body()?.to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(config.timeout)).http_status_as_error(false).build().into();
    let mut response = agent
        .post(config.endpoint.as_str())
        .header("x-goog-api-key", config.api_key.as_str())
        .header("content-type", "application/json")
        .header("user-agent", "Photocraft-genai")
        .send(body.as_str())
        .map_err(|e| Error::Network(e.to_string()))?;
    let status = response.status().as_u16();
    let text = response.body_mut().with_config().limit(MAX_RESPONSE_BYTES).read_to_string().map_err(|e| Error::Network(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(Error::Api { status, message: api_error_message(&text) });
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| Error::Malformed(format!("not JSON ({e})")))?;
    parse_response(&value)
}

/// The web build has no HTTP client here.
#[cfg(target_arch = "wasm32")]
pub fn generate(request: &Request, _config: &Config) -> Result<Generated> {
    request.validate()?;
    Err(Error::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_b64() -> String {
        BASE64.encode(b"\x89PNG fake")
    }

    #[test]
    fn body_has_images_then_prompt_and_the_output_format() {
        let mut r = Request::new("a red umbrella");
        r.images.push(InputImage { mime_type: "image/png".into(), bytes: vec![1, 2, 3] });
        r.aspect_ratio = Some("16:9".into());
        r.image_size = Some("2K".into());
        let b = r.body().unwrap();
        assert_eq!(b["model"], DEFAULT_MODEL);
        assert_eq!(b["store"], false);
        assert_eq!(b["input"][0], json!({"type": "image", "mime_type": "image/png", "data": "AQID"}));
        assert_eq!(b["input"][1], json!({"type": "text", "text": "a red umbrella"}));
        assert_eq!(b["response_format"], json!({"type": "image", "mime_type": "image/jpeg", "aspect_ratio": "16:9", "image_size": "2K"}));
    }

    #[test]
    fn invalid_requests_are_refused_before_sending() {
        let base = Request::new("x");
        let cases = [
            Request { prompt: "  ".into(), ..base.clone() },
            Request { prompt: "a".repeat(MAX_PROMPT_CHARS + 1), ..base.clone() },
            Request { model: "".into(), ..base.clone() },
            Request { model: "bad model?".into(), ..base.clone() },
            Request { aspect_ratio: Some("7:3".into()), ..base.clone() },
            Request { image_size: Some("2k".into()), ..base.clone() },
            Request { images: vec![InputImage { mime_type: "image/png".into(), bytes: vec![] }; MAX_INPUT_IMAGES + 1], ..base.clone() },
        ];
        for r in cases {
            assert!(matches!(r.body(), Err(Error::BadRequest(_))), "{r:?}");
        }
    }

    #[test]
    fn parses_the_last_model_image_and_skips_echoed_input() {
        let reply = json!({
            "id": "v1_x", "status": "completed",
            "steps": [
                {"type": "user_input", "content": [{"type": "image", "mime_type": "image/png", "data": BASE64.encode(b"input")}]},
                {"type": "model_output", "content": [
                    {"type": "text", "text": "Here you go."},
                    {"type": "image", "mime_type": "image/png", "data": png_b64()}
                ]}
            ]
        });
        let g = parse_response(&reply).unwrap();
        assert_eq!(g.bytes, b"\x89PNG fake");
        assert_eq!(g.mime_type, "image/png");
        assert_eq!(g.text.as_deref(), Some("Here you go."));
    }

    #[test]
    fn a_refusal_without_image_reports_the_models_text() {
        let reply = json!({"status": "completed", "steps": [{"type": "model_output", "content": [{"type": "text", "text": "I can't help with that."}]}]});
        assert_eq!(parse_response(&reply), Err(Error::NoImage(Some("I can't help with that.".into()))));
        assert_eq!(parse_response(&json!({})), Err(Error::NoImage(None)));
    }

    #[test]
    fn errors_and_failed_runs_are_reported() {
        let e = parse_response(&json!({"error": {"code": 400, "message": "API key not valid", "status": "INVALID_ARGUMENT"}}));
        assert_eq!(e, Err(Error::Api { status: 400, message: "API key not valid".into() }));
        let f = parse_response(&json!({"status": "failed", "errors": [{"code": "x", "message": "blocked by safety filters"}]}));
        assert_eq!(f, Err(Error::Failed("blocked by safety filters".into())));
        let bad = json!({"steps": [{"type": "model_output", "content": [{"type": "image", "data": "@@not base64@@"}]}]});
        assert!(matches!(parse_response(&bad), Err(Error::Malformed(_))));
        let empty = json!({"steps": [{"type": "model_output", "content": [{"type": "image", "data": ""}]}]});
        assert!(matches!(parse_response(&empty), Err(Error::Malformed(_))));
    }

    #[test]
    fn hostile_reply_shapes_never_panic() {
        for v in [json!(null), json!([]), json!("x"), json!({"steps": 3}), json!({"steps": [1, null, {"content": "x"}]}), json!({"error": 5}), json!({"status": 7})] {
            let _ = parse_response(&v);
        }
    }

    #[test]
    fn api_error_bodies_become_readable_messages() {
        assert_eq!(api_error_message(r#"{"error":{"code":403,"message":"Permission denied"}}"#), "Permission denied");
        assert_eq!(api_error_message("<html>Bad gateway</html>"), "<html>Bad gateway</html>");
        assert_eq!(api_error_message(""), "empty reply");
    }

    #[test]
    fn nearest_aspect_ratios() {
        assert_eq!(nearest_aspect_ratio(512, 512).0, "1:1");
        assert_eq!(nearest_aspect_ratio(1920, 1080).0, "16:9");
        assert_eq!(nearest_aspect_ratio(1080, 1920).0, "9:16");
        assert_eq!(nearest_aspect_ratio(3000, 1000).0, "21:9");
        assert_eq!(nearest_aspect_ratio(600, 400).0, "3:2");
        assert_eq!(nearest_aspect_ratio(0, 400).0, "1:1");
        assert_eq!(nearest_aspect_ratio(u32::MAX, 1).0, "21:9");
    }

    #[test]
    fn image_sizes_cover_the_long_side() {
        assert_eq!(image_size_for(0), "1K");
        assert_eq!(image_size_for(1024), "1K");
        assert_eq!(image_size_for(1025), "2K");
        assert_eq!(image_size_for(2048), "2K");
        assert_eq!(image_size_for(9000), "4K");
    }

    #[test]
    fn config_takes_the_explicit_key_then_the_environment() {
        let env = |name: &str| match name {
            API_KEY_ENV => Some(" env-key ".to_string()),
            _ => None,
        };
        assert_eq!(Config::resolve(Some("given"), env).unwrap().api_key, "given");
        let c = Config::resolve(None, env).unwrap();
        assert_eq!(c.api_key, "env-key");
        assert_eq!(c.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(Config::resolve(None, |_| None), Err(Error::NoApiKey));
        assert_eq!(Config::resolve(Some("  "), |_| None), Err(Error::NoApiKey));
        assert!(matches!(Config::resolve(Some("has space"), |_| None), Err(Error::BadRequest(_))));
        let insecure = |name: &str| (name == ENDPOINT_ENV).then(|| "http://example.com".to_string());
        assert!(matches!(Config::resolve(Some("k"), insecure), Err(Error::BadRequest(_))));
    }

    #[test]
    fn the_key_never_shows_in_debug_output() {
        let c = Config::resolve(Some("secret-123"), |_| None).unwrap();
        assert!(!format!("{c:?}").contains("secret-123"));
    }
}
