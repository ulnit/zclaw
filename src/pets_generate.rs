//! Pet generation — base-draft → hatch pipeline. Port of hermes
//! `agent/pet/generate/` (prompts/imagegen/orchestrate, v2026.8.3).
//!
//! Two steps, mirroring hermes' UX across every surface:
//! 1. `generate_base_drafts` — prompt-only "what should this pet look like"
//!    variants; the user picks one (or retries for a fresh set).
//! 2. `hatch_pet` — takes the chosen base and generates one grounded row
//!    strip per state, slices each into frames, composes the atlas,
//!    validates it, and writes the pet into the store.
//!
//! Known diff vs hermes: image generation rides one OpenAI-compatible
//! images endpoint (the configured base URL + OPENAI_API_KEY, or
//! `[pets] image_base_url`/`image_api_key`/`image_model` overrides), rather
//! than hermes' multi-provider registry (Nous/OpenRouter/Krea/…).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use image::RgbaImage;

// =========================================================================
// Prompts (hermes `agent/pet/generate/prompts.py`)
// =========================================================================

/// What each petdex/Codex state should depict (hermes `STATE_ACTIONS`).
pub fn state_action(state: &str) -> &'static str {
    match state {
        "idle" => "a calm idle loop: subtle breathing, a tiny blink or gentle bob, no big gestures",
        "running-right" => "a sideways walk/run locomotion cycle moving to the RIGHT: the character faces and travels right with clear directional steps, a smooth gait loop",
        "running-left" => "a sideways walk/run locomotion cycle moving to the LEFT: the character faces and travels left with clear directional steps (the mirror of the right-facing run)",
        "waving" => "a friendly greeting: raising a paw/hand/limb to wave, clear up-and-down gesture",
        "jumping" => "a happy celebration jump: anticipation, lift off the ground, peak, and land",
        "failed" => "a sad or deflated reaction: slumped, dejected, small frown — readable but not noisy",
        "waiting" => "an expectant 'waiting on you' pose: looking up/out as if asking for input or approval — distinct from idle and review",
        "running" => "focused active work, staying IN PLACE (NOT walking or foot-running): leaning in, concentrating, busy 'thinking / processing / typing' energy",
        "review" => "careful inspection: a focused lean, head tilt, studying something intently",
        _ => "a simple idle pose",
    }
}

/// Style hint suffix (hermes `_STYLE_HINTS`). `auto` defaults to the popular
/// petdex look: crisp 16-bit PIXEL ART.
pub fn style_hint(style: Option<&str>) -> &'static str {
    match style.unwrap_or("auto").trim().to_lowercase().as_str() {
        "pixel" => " Render in clean 16-bit pixel-art style with visible square pixels and a limited palette.",
        "plush" => " Render as a soft plush toy.",
        "clay" => " Render as a claymation / soft 3D clay figure.",
        "sticker" => " Render as a glossy die-cut sticker.",
        "flat-vector" => " Render in flat vector mascot style.",
        "3d-toy" => " Render as a glossy 3D toy.",
        "painterly" => " Render in a soft painterly style.",
        _ => " Style: crisp 16-bit PIXEL-ART game sprite — visible square pixels, a small limited palette, clean dark outline, flat cel shading, chunky chibi proportions, like a classic SNES/JRPG party member or a petdex.dev mascot. Absolutely NOT 3D-rendered, NOT a smooth painted or vector illustration, NOT photorealistic — no soft gradients, no realistic lighting, no figurine look.",
    }
}

const BACKGROUND_SPEC: &str =
    "Center the character on a SINGLE flat, uniform, high-contrast chroma-key \
background — pure hot magenta #FF00FF (only if magenta appears on the \
character, use pure green #00FF00 instead). The background is ONE continuous \
even color that completely surrounds the character with NO gradient, \
vignette, texture, pattern, scenery, shadow, ground line, frame, border, \
panel, comic cell, gutter line, grid, or divider of any kind, so it keys out \
cleanly. The background color must not appear anywhere on the character. \
No text, no labels, no speech bubbles, no UI.";

/// Row strips are generated on the wider landscape canvas (hermes
/// `_ASSUMED_STRIP_WIDTH`).
const ASSUMED_STRIP_WIDTH: u32 = 1536;

/// (per-pose width px, gap px) for a row of `frame_count` poses (hermes
/// `_spacing_spec`).
pub fn spacing_spec(frame_count: u32) -> (u32, u32) {
    let slots = frame_count.max(1) as f64;
    let slot_w = ASSUMED_STRIP_WIDTH as f64 / slots;
    let pose_px = (slot_w * 0.7).round() as u32;
    let gap_px = (48.0f64).max((slot_w * 0.3).round()) as u32;
    (pose_px, gap_px)
}

/// Per-draft nudges so the base options are actually distinct (hermes
/// `BASE_VARIATIONS`).
pub const BASE_VARIATIONS: &[&str] = &[
    "",
    "a distinctly different colour palette and markings",
    "a heavier, broader silhouette with sturdier proportions",
    "a different facial structure and expression matching the concept tone, with unique accent/accessory details",
    "a leaner, taller build and an alternate colour scheme",
    "bolder, more saturated colours and a stronger expression matching the concept tone",
];

/// The base look: a single, clean, centered full-body mascot (hermes
/// `build_base_prompt`).
pub fn build_base_prompt(concept: &str, style: Option<&str>, variation: &str) -> String {
    let concept = if concept.trim().is_empty() {
        "a distinctive mascot creature"
    } else {
        concept.trim()
    };
    let nudge = if variation.is_empty() {
        String::new()
    } else {
        format!(" Make this design distinct: {variation}.")
    };
    format!(
        "A stylized mascot pet character: {concept}. \
         Honor the requested tone and mood exactly (cute, eerie, scary, menacing, whimsical, etc.) \
         while staying non-graphic. \
         Compact, whole-body silhouette that reads clearly at small size, \
         clear readable facial features, simple consistent palette. \
         Neutral front-facing standing pose, upright and symmetric, arms/limbs \
         relaxed at the sides, feet together on the ground, any cape/accessories \
         hanging straight and still.\
         {nudge} \
         {BACKGROUND_SPEC}{}",
        style_hint(style)
    )
}

/// A row strip: `frame_count` poses of the SAME character, left→right
/// (hermes `build_row_prompt`).
pub fn build_row_prompt(
    state: &str,
    frame_count: u32,
    concept: &str,
    style: Option<&str>,
) -> String {
    let action = state_action(state);
    // NB: hermes binds `concept` here too but never interpolates it — the
    // attached reference image carries the identity, not the text.
    let _ = concept;
    let (pose_px, gap_px) = spacing_spec(frame_count);
    format!(
        "Using the attached reference image as the exact same character \
         (same species, face, colors, markings, proportions, and props), \
         preserving the same emotional tone/mood (e.g., scary stays scary, cute stays cute), \
         draw a single WIDE horizontal strip of {frame_count} animation frames showing {action}. \
         LAYOUT: arrange {frame_count} poses in ONE horizontal row at equal spacing, \
         each pose centered in its own imaginary equal region. Draw NO panel borders, \
         NO comic cells, NO boxes, NO vertical divider/gutter lines, NO grid, NO frame \
         outlines between poses — the backdrop is one unbroken flat field behind all of them. \
         Fill the WHOLE strip with the SAME single flat chroma-key color as the attached \
         reference image's background (identical hue in every frame, no per-pose color shifts). \
         SPACING (critical): draw each pose at a consistent, healthy, clearly \
         visible size (roughly {pose_px}px wide on a {ASSUMED_STRIP_WIDTH}px \
         strip) — do NOT shrink it tiny — but keep its ENTIRE silhouette \
         (wings, tail, halo, horns, cape, every appendage) fully INSIDE its own \
         cell. Leave at least {gap_px}px of empty chroma-key background between \
         neighboring silhouettes at their closest point (wingtip to wingtip), and \
         the same empty margin before the first pose and after the last. If a wing, \
         cape, or tail would reach into a neighbor, FOLD or angle it inward rather \
         than letting it cross the gap. Silhouettes must NEVER touch, overlap, \
         share a shadow, share a ground line, share motion trails, or merge into \
         one connected shape. \
         REGISTRATION (critical): the character is the SAME height and SAME width \
         in every frame, drawn at the SAME scale, centered over the SAME point, \
         with all feet aligned to the SAME invisible horizontal baseline across the \
         whole strip — this baseline is conceptual ONLY: draw NO ground line, floor, \
         platform, horizon, or contact shadow beneath the feet. Keep the body's center, size, and stance fixed frame to \
         frame — ONLY the limbs/features the action needs may move. Capes, cloaks, \
         bags, and scarves stay in the SAME place and shape every frame (no \
         swinging, flowing, or drifting) unless the action itself requires it. No \
         pose is cropped at the strip edges. \
         {BACKGROUND_SPEC}{}",
        style_hint(style)
    )
}

// =========================================================================
// Image provider (hermes `agent/pet/generate/imagegen.py`)
// =========================================================================

const DEFAULT_PET_IMAGE_MODEL: &str = "gpt-image-2";

/// Resolved image-generation endpoint settings.
#[derive(Debug, Clone)]
pub struct ImageGenEndpoint {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Resolve the images endpoint: `[pets]` overrides > OPENAI env/config.
pub fn resolve_image_endpoint() -> Result<ImageGenEndpoint, String> {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let pets = &config.pets;
    let api_key = pets
        .image_api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| crate::config::get_env_value("OPENAI_API_KEY"))
        .or_else(|| crate::config::get_env_value("ULNCLAW_API_KEY"))
        .ok_or_else(|| {
            "Pet generation needs an OpenAI-compatible image API key \
             (OPENAI_API_KEY or [pets] image_api_key)"
                .to_string()
        })?;
    let base_url = pets
        .image_base_url
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let model = pets
        .image_model
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| crate::config::get_env_value("ULNCLAW_PET_IMAGE_MODEL"))
        .unwrap_or_else(|| DEFAULT_PET_IMAGE_MODEL.to_string());
    Ok(ImageGenEndpoint {
        base_url,
        api_key,
        model,
    })
}

/// True when a provider error is specifically about the `background` param
/// (hermes `_rejected_background`).
fn rejected_background(error: &str) -> bool {
    let lowered = error.to_lowercase();
    lowered.contains("background")
        && (lowered.contains("not supported") || lowered.contains("transparent"))
}

/// Turn a raw provider error into a friendly, actionable sentence (hermes
/// `_humanize_image_error`).
pub fn humanize_image_error(error: &str) -> String {
    let low = error.to_lowercase();
    if [
        "moderation_blocked",
        "safety system",
        "content policy",
        "content_policy",
    ]
    .iter()
    .any(|s| low.contains(s))
    {
        return "The image provider blocked this prompt — its safety filter rejects \
                trademarked characters and real people. Try an original description."
            .to_string();
    }
    if ["api key", "unauthorized", "401", "auth"]
        .iter()
        .any(|s| low.contains(s))
    {
        return "The image provider rejected the request — check your API key in Settings → Providers."
            .to_string();
    }
    if low.contains("rate limit") || low.contains("429") {
        return "The image provider is rate-limiting — wait a moment and try again.".to_string();
    }
    error.lines().next().unwrap_or(error).trim()[..200.min(error.trim().len())].to_string()
}

fn decode_image_response(body: &serde_json::Value) -> Result<Vec<u8>, String> {
    use base64::Engine;
    if let Some(b64) = body.pointer("/data/0/b64_json").and_then(|v| v.as_str()) {
        return base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("decode image: {e}"));
    }
    if let Some(url) = body.pointer("/data/0/url").and_then(|v| v.as_str()) {
        let bytes = reqwest::blocking::get(url)
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("download generated image: {e}"))?;
        return Ok(bytes.to_vec());
    }
    Err("image API returned no data".to_string())
}

/// Generate one image. With `reference` present the call rides
/// `/images/edits` (grounded generation); otherwise `/images/generations`.
/// Asks for a transparent background, retries without the flag when the
/// model rejects it (hermes `imagegen.generate` semantics).
pub fn generate_image(
    endpoint: &ImageGenEndpoint,
    prompt: &str,
    reference: Option<&Path>,
    landscape: bool,
) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let size = if landscape { "1536x1024" } else { "1024x1024" };

    let attempt = |transparent: bool| -> Result<Vec<u8>, String> {
        let response = if let Some(reference) = reference {
            let file_bytes = std::fs::read(reference)
                .map_err(|e| format!("read reference {}: {e}", reference.display()))?;
            let file_name = reference
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("reference.png")
                .to_string();
            let part = reqwest::blocking::multipart::Part::bytes(file_bytes)
                .file_name(file_name)
                .mime_str("image/png")
                .map_err(|e| format!("multipart: {e}"))?;
            let mut form = reqwest::blocking::multipart::Form::new()
                .text("model", endpoint.model.clone())
                .text("prompt", prompt.to_string())
                .text("size", size.to_string())
                .text("n", "1".to_string())
                .part("image", part);
            if transparent {
                form = form.text("background", "transparent");
            }
            client
                .post(format!(
                    "{}/images/edits",
                    endpoint.base_url.trim_end_matches('/')
                ))
                .bearer_auth(&endpoint.api_key)
                .multipart(form)
                .send()
        } else {
            let mut payload = serde_json::json!({
                "model": endpoint.model,
                "prompt": prompt,
                "n": 1,
                "size": size,
            });
            if transparent {
                payload["background"] = serde_json::json!("transparent");
            }
            client
                .post(format!(
                    "{}/images/generations",
                    endpoint.base_url.trim_end_matches('/')
                ))
                .bearer_auth(&endpoint.api_key)
                .json(&payload)
                .send()
        };
        let response = response.map_err(|e| format!("image API: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(format!(
                "image API {status}: {}",
                &body[..body.len().min(300)]
            ));
        }
        let body: serde_json::Value = response
            .json()
            .map_err(|e| format!("parse image response: {e}"))?;
        decode_image_response(&body)
    };

    match attempt(true) {
        Ok(bytes) => Ok(bytes),
        Err(err) if rejected_background(&err) => attempt(false),
        Err(err) => Err(err),
    }
}

// =========================================================================
// Orchestration (hermes `agent/pet/generate/orchestrate.py`)
// =========================================================================

/// Image generations fan out instead of running back-to-back (hermes
/// `_MAX_PARALLEL_GENERATIONS`).
const MAX_PARALLEL_GENERATIONS: usize = 4;
/// How many times to (re)generate a single row (hermes `_ROW_GEN_ATTEMPTS`).
const ROW_GEN_ATTEMPTS: u32 = 3;
const MIN_FILLED_STATES: usize = 6;
const REQUIRED_STATES: &[&str] = &["idle", "running-right", "waving"];

/// Outcome of a successful [`hatch_pet`] (hermes `HatchResult`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HatchResult {
    pub slug: String,
    pub display_name: String,
    pub spritesheet: PathBuf,
    pub states: Vec<String>,
    pub validation: crate::pets_atlas::AtlasValidation,
}

/// Key out any solid backdrop the provider painted; returns the cleaned RGBA
/// (hermes `_harden_transparency`).
pub fn harden_transparency(bytes: &[u8]) -> RgbaImage {
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let keyed = crate::pets_atlas::remove_background(&img.to_rgba8(), None);
            let mut cleaned = keyed;
            crate::pets_atlas::clear_transparent_rgb(&mut cleaned);
            cleaned
        }
        Err(_) => RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0])),
    }
}

/// Generate `n` candidate base looks for `concept`; returns decoded images
/// (hermes `generate_base_drafts`). Drafts run concurrently; each gets a
/// distinct variation nudge so the options aren't near-duplicates.
pub fn generate_base_drafts(
    endpoint: &ImageGenEndpoint,
    concept: &str,
    n: usize,
    style: Option<&str>,
    on_draft: Option<&dyn Fn(usize, &RgbaImage)>,
    is_cancelled: Option<&AtomicBool>,
) -> Result<Vec<RgbaImage>, String> {
    let cancelled = |flag: Option<&AtomicBool>| flag.map_or(false, |f| f.load(Ordering::SeqCst));
    let n = n.max(1);
    let results: Mutex<Vec<(usize, Result<RgbaImage, String>)>> = Mutex::new(Vec::new());

    // Fan out in chunks of MAX_PARALLEL_GENERATIONS (hermes caps the
    // concurrent image calls the same way).
    let indices: Vec<usize> = (0..n).collect();
    for chunk in indices.chunks(MAX_PARALLEL_GENERATIONS) {
        if cancelled(is_cancelled) {
            break;
        }
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for &index in chunk {
                if cancelled(is_cancelled) {
                    break;
                }
                let variation = BASE_VARIATIONS[index % BASE_VARIATIONS.len()];
                let prompt = build_base_prompt(concept, style, variation);
                let endpoint = endpoint.clone();
                let results = &results;
                handles.push(scope.spawn(move || {
                    let outcome = generate_image(&endpoint, &prompt, None, false)
                        .map(|bytes| harden_transparency(&bytes));
                    results
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((index, outcome));
                }));
            }
            for handle in handles {
                handle.join().ok();
            }
        });
    }

    let mut drafts: Vec<(usize, RgbaImage)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (index, outcome) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
        match outcome {
            Ok(img) => drafts.push((index, img)),
            Err(err) => errors.push(err),
        }
    }
    drafts.sort_by_key(|(index, _)| *index);
    if drafts.is_empty() {
        let reason = errors
            .first()
            .map(|e| humanize_image_error(e))
            .unwrap_or_else(|| "image generation produced no usable drafts".to_string());
        return Err(reason);
    }
    let images: Vec<RgbaImage> = drafts.into_iter().map(|(_, img)| img).collect();
    if let Some(callback) = on_draft {
        for (i, img) in images.iter().enumerate() {
            callback(i, img);
        }
    }
    Ok(images)
}

/// Turn an approved base image into a full, installed pet (hermes
/// `hatch_pet`). Generates a grounded row strip per state, extracts frames,
/// composes + validates the atlas, and registers it. The idle row falls back
/// to the base look so the pet always renders.
pub fn hatch_pet(
    home: &Path,
    endpoint: &ImageGenEndpoint,
    base_image: &RgbaImage,
    slug: &str,
    display_name: &str,
    description: &str,
    concept: &str,
    style: Option<&str>,
    on_progress: Option<&dyn Fn(&str, &str)>,
    is_cancelled: Option<&AtomicBool>,
) -> Result<HatchResult, String> {
    let progress = |event: &str, detail: &str| {
        if let Some(callback) = on_progress {
            callback(event, detail);
        }
    };
    let cancelled = || is_cancelled.map_or(false, |f| f.load(Ordering::SeqCst));

    // Save the base once so row calls can attach it as a multipart reference.
    let scratch = home.join("pets").join(".hatch");
    std::fs::create_dir_all(&scratch).map_err(|e| format!("create {}: {e}", scratch.display()))?;
    let base_path = scratch.join(format!(
        "base-{}.png",
        &uuid::Uuid::new_v4().to_string()[..8]
    ));
    {
        let mut buffer: Vec<u8> = Vec::new();
        base_image
            .write_to(
                &mut std::io::Cursor::new(&mut buffer),
                image::ImageFormat::Png,
            )
            .map_err(|e| format!("encode base: {e}"))?;
        std::fs::write(&base_path, &buffer).map_err(|e| format!("write base: {e}"))?;
    }

    let mut frames_by_state: HashMap<String, Vec<RgbaImage>> = HashMap::new();
    let total_rows = crate::pets_atlas::ROW_SPECS.len();

    // running-left is derived by mirroring running-right, so we don't
    // generate it directly.
    let specs: Vec<(&str, u32, u32)> = crate::pets_atlas::ROW_SPECS
        .iter()
        .filter(|(state, _, _)| *state != "running-left")
        .copied()
        .collect();

    let rows: Mutex<Vec<(String, Option<Vec<RgbaImage>>)>> = Mutex::new(Vec::new());
    // Fan out row generation in chunks of MAX_PARALLEL_GENERATIONS —
    // hermes gates the same work with an asyncio semaphore.
    for chunk in specs.chunks(MAX_PARALLEL_GENERATIONS) {
        if cancelled() {
            break;
        }
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for &(state, _row, count) in chunk {
                if cancelled() {
                    break;
                }
                let endpoint = endpoint.clone();
                let base_path = base_path.clone();
                let concept = concept.to_string();
                let style = style.map(String::from);
                let cancelled_flag = is_cancelled;
                let rows = &rows;
                handles.push(scope.spawn(move || {
                    let state_name = state.to_string();
                    if cancelled_flag.map_or(false, |f| f.load(Ordering::SeqCst)) {
                        rows.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((state_name, None));
                        return;
                    }
                    let prompt = build_row_prompt(state, count, &concept, style.as_deref());
                    let mut frames: Option<Vec<RgbaImage>> = None;
                    for attempt in 0..ROW_GEN_ATTEMPTS {
                        let strict = attempt + 1 < ROW_GEN_ATTEMPTS;
                        let method = if strict { "components" } else { "auto" };
                        let strip_bytes =
                            match generate_image(&endpoint, &prompt, Some(&base_path), true) {
                                Ok(bytes) => bytes,
                                Err(_) => continue,
                            };
                        let Ok(strip) = image::load_from_memory(&strip_bytes) else {
                            continue;
                        };
                        match crate::pets_atlas::extract_strip_frames(
                            &strip.to_rgba8(),
                            count,
                            None,
                            method,
                            false,
                        ) {
                            Ok(extracted) => {
                                frames = Some(extracted);
                                break;
                            }
                            Err(_) => continue,
                        }
                    }
                    rows.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((state_name, frames));
                }));
            }
            for handle in handles {
                handle.join().ok();
            }
        });
    }

    if cancelled() {
        return Err("hatch cancelled".to_string());
    }

    let mut done = 0usize;
    for (state, frames) in rows.into_inner().unwrap_or_else(|e| e.into_inner()) {
        done += 1;
        progress("row", &format!("{state}:{done}:{total_rows}"));
        if let Some(frames) = frames {
            frames_by_state.insert(state, frames);
        }
    }

    // Derive running-left from the approved running-right row.
    if let Some(right) = frames_by_state.get("running-right").cloned() {
        done += 1;
        progress("row", &format!("running-left:{done}:{total_rows}"));
        frames_by_state.insert(
            "running-left".to_string(),
            crate::pets_atlas::mirror_frames(&right),
        );
    }

    // Idle is the resting state the renderer falls back to — guarantee it.
    if !frames_by_state.contains_key("idle") {
        progress("row", "idle-fallback");
        frames_by_state.insert(
            "idle".to_string(),
            vec![crate::pets_atlas::single_frame(base_image, false)],
        );
    }

    progress("compose", "");
    let sheet = crate::pets_atlas::compose_atlas(&crate::pets_atlas::normalize_cells(
        &frames_by_state,
        crate::pets_atlas::NORMALIZE_PAD,
    ));
    let validation = crate::pets_atlas::validate_atlas(&sheet);
    if !validation.ok {
        return Err(validation.errors.join("; "));
    }
    let filled: std::collections::HashSet<&str> = validation
        .filled_states
        .iter()
        .map(|s| s.as_str())
        .collect();
    let missing: Vec<&str> = REQUIRED_STATES
        .iter()
        .filter(|state| !filled.contains(*state))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required animation row(s): {}",
            missing.join(", ")
        ));
    }
    if validation.filled_states.len() < MIN_FILLED_STATES {
        return Err(format!(
            "only {}/{} animation rows were usable; regenerate",
            validation.filled_states.len(),
            crate::pets_atlas::ROW_SPECS.len()
        ));
    }

    progress("save", slug);
    let sheet_bytes = crate::pets_atlas::atlas_to_sheet_bytes(&sheet)?;
    let pet = crate::pets::register_local_pet(home, &sheet_bytes, slug, display_name, description)
        .map_err(|e| e.to_string())?;
    std::fs::remove_file(&base_path).ok();
    Ok(HatchResult {
        slug: pet.slug,
        display_name: pet.display_name,
        spritesheet: pet.spritesheet,
        states: validation.filled_states.clone(),
        validation,
    })
}

/// Derive a display name from a concept: title-case the first three words,
/// cap at 28 chars (hermes desktop/CLI hatch naming).
pub fn derive_pet_name(concept: &str) -> String {
    let words: Vec<String> = concept
        .split_whitespace()
        .take(3)
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    let joined = words.join(" ");
    if joined.is_empty() {
        "Pet".to_string()
    } else {
        joined.chars().take(28).collect()
    }
}

/// Convenience: end-to-end hatch from a text concept — one base draft, then
/// the full pipeline (hermes REPL `/hatch` flow).
pub fn run_hatch_flow(
    home: &Path,
    concept: &str,
    style: Option<&str>,
    display_name: &str,
    on_progress: Option<&dyn Fn(&str, &str)>,
) -> Result<HatchResult, String> {
    let endpoint = resolve_image_endpoint()?;
    let drafts = generate_base_drafts(&endpoint, concept, 1, style, None, None)?;
    let Some(base) = drafts.into_iter().next() else {
        return Err("no base draft came back — try again".to_string());
    };
    let name = if display_name.trim().is_empty() {
        derive_pet_name(concept)
    } else {
        display_name.trim().to_string()
    };
    let slug = crate::pets::slugify(&name);
    hatch_pet(
        home,
        &endpoint,
        &base,
        &slug,
        &name,
        concept,
        concept,
        style,
        on_progress,
        None,
    )
}

// =========================================================================
// CLI surface (hermes `/hatch` in cli_commands_mixin.py)
// =========================================================================

/// CLI entry: `ulnclaw pets hatch <description>` (hermes `/hatch`).
/// `drafts_only > 0` generates N base looks, saves them, and stops;
/// `base_path` skips draft generation and hatches from an existing image.
pub fn cmd_hatch(
    home: &Path,
    concept: &str,
    style: Option<&str>,
    display_name: Option<&str>,
    base_path: Option<&str>,
    drafts_only: usize,
) -> i32 {
    let mut concept = concept.trim().to_string();
    if concept.is_empty() && base_path.is_none() {
        print!("(o_o) Describe your pet: ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            println!("(x_x) Could not read a description.");
            return 1;
        }
        concept = line.trim().to_string();
    }
    if concept.is_empty() && base_path.is_none() {
        println!("(o_o) Usage: ulnclaw pets hatch <description>  (e.g. a tiny cyber fox)");
        return 1;
    }

    let endpoint = match resolve_image_endpoint() {
        Ok(endpoint) => endpoint,
        Err(err) => {
            println!("(x_x) {err}");
            return 1;
        }
    };

    // --drafts N: save N candidate base looks and stop (hermes desktop
    // overlay shows them for picking; the CLI writes PNGs to choose from).
    if drafts_only > 0 {
        println!(
            "(o_o) Designing '{}'… ({} draft{}, one image-model call each)",
            concept,
            drafts_only,
            if drafts_only == 1 { "" } else { "s" }
        );
        match generate_base_drafts(&endpoint, &concept, drafts_only, style, None, None) {
            Ok(drafts) => {
                let scratch = home.join("pets").join(".hatch");
                if std::fs::create_dir_all(&scratch).is_err() {
                    println!("(x_x) Could not create {}", scratch.display());
                    return 1;
                }
                for (i, img) in drafts.iter().enumerate() {
                    let path = scratch.join(format!("draft-{}.png", i + 1));
                    if img.save(&path).is_err() {
                        println!("(x_x) Could not save {}", path.display());
                        return 1;
                    }
                    println!("  ┊ saved {}", path.display());
                }
                println!(
                    "(^_^) Pick one, then: ulnclaw pets hatch --base <path> '{}'",
                    concept
                );
                0
            }
            Err(err) => {
                println!("(x_x) Couldn't generate base drafts: {err}");
                1
            }
        }
    } else {
        let base_image = match base_path {
            Some(path) => {
                println!("(o_o) Using {} as the base look…", path);
                match std::fs::read(path) {
                    Ok(bytes) => harden_transparency(&bytes),
                    Err(err) => {
                        println!("(x_x) Could not read {path}: {err}");
                        return 1;
                    }
                }
            }
            None => {
                println!("(o_o) Designing '{concept}'… (a minute of image-model calls)");
                match generate_base_drafts(&endpoint, &concept, 1, style, None, None) {
                    Ok(drafts) => match drafts.into_iter().next() {
                        Some(img) => img,
                        None => {
                            println!("(x_x) No base draft came back — try again.");
                            return 1;
                        }
                    },
                    Err(err) => {
                        println!("(x_x) Couldn't generate a base look: {err}");
                        return 1;
                    }
                }
            }
        };

        let name = match display_name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => name.to_string(),
            None => derive_pet_name(&concept),
        };
        let slug = crate::pets::slugify(&name);

        let progress = |event: &str, detail: &str| match event {
            "row" => {
                let state = detail.split(':').next().unwrap_or(detail);
                println!("  ┊ drawing {state}…");
            }
            "compose" => println!("  ┊ composing spritesheet…"),
            "save" => println!("  ┊ saving…"),
            _ => {}
        };

        match hatch_pet(
            home,
            &endpoint,
            &base_image,
            &slug,
            &name,
            &concept,
            &concept,
            style,
            Some(&progress),
            None,
        ) {
            Ok(result) => {
                if let Err(err) = crate::pets::set_active(&result.slug) {
                    println!("(o_o) Hatched, but could not set it active: {err}");
                }
                println!(
                    "(^_^)b {} hatched and adopted — it'll pop in shortly!",
                    result.display_name
                );
                0
            }
            Err(err) => {
                println!("(x_x) Hatch failed: {err}");
                1
            }
        }
    }
}
