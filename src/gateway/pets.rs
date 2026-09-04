//! Petdex mascot HTTP API — lets the desktop app (and any web surface)
//! render the active pet: config poll, installed-pet list, and raw
//! spritesheet bytes. Mirrors hermes' desktop pet overlay data flow
//! (`display.pet.*` config + `<home>/pets/<slug>/spritesheet.*`).

use axum::{
    extract::Path,
    http::header,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Atlas layout shared by every renderer (hermes Codex 9-row sheets).
fn atlas_layout() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = crate::pets_atlas::ROW_SPECS
        .iter()
        .map(|(state, row, frames)| json!({"state": state, "row": row, "frames": frames}))
        .collect();
    json!({
        "frame_w": crate::pets_atlas::CELL_WIDTH,
        "frame_h": crate::pets_atlas::CELL_HEIGHT,
        "columns": crate::pets_atlas::COLUMNS,
        "rows": rows,
    })
}

/// `GET /api/pets/config` — active `display.pet` settings + atlas layout.
pub async fn config() -> Response {
    let cfg = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let pet = &cfg.display.pet;
    Json(json!({
        "object": "ulnclaw.pet.config",
        "enabled": pet.enabled,
        "slug": pet.slug,
        "scale": pet.scale,
        "render_mode": pet.render_mode,
        "atlas": atlas_layout(),
    }))
    .into_response()
}

/// `GET /api/pets` — installed pets.
pub async fn list() -> Response {
    let home = crate::config::ulnclaw_home();
    let pets = crate::pets::installed_pets(&home);
    let active = crate::config::UlncLawConfig::load(None)
        .unwrap_or_default()
        .display
        .pet
        .slug;
    let data: Vec<serde_json::Value> = pets
        .iter()
        .map(|p| {
            json!({
                "slug": p.slug,
                "display_name": p.display_name,
                "description": p.description,
                "created_by": p.created_by,
                "active": active.as_deref() == Some(p.slug.as_str()),
                "spritesheet": format!("/api/pets/{}/spritesheet", p.slug),
            })
        })
        .collect();
    Json(json!({"object": "ulnclaw.pet.list", "data": data})).into_response()
}

/// `GET /api/pets/:slug/spritesheet` — raw sheet bytes (webp or png).
pub async fn spritesheet(Path(slug): Path<String>) -> Response {
    let home = crate::config::ulnclaw_home();
    let Some(pet) = crate::pets::load_pet(&home, &slug) else {
        return super::not_found(&format!("pet {slug} not installed"));
    };
    if !pet.exists() {
        return super::not_found(&format!("pet {slug} has no spritesheet"));
    }
    let bytes = match std::fs::read(&pet.spritesheet) {
        Ok(b) => b,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else {
        "image/webp"
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "max-age=300"),
        ],
        bytes,
    )
        .into_response()
}

// =========================================================================
// Hatch jobs — long-running backend for the desktop hatch overlay (hermes
// `apps/desktop` pet-generate overlay parity). `POST /api/pets/hatch`
// starts a job that generates base drafts; the user picks one (draft PNGs
// + `POST .../pick`) and the job hatches the full spritesheet, adopting it
// on success. Poll `GET /api/pets/hatch/:id` for status + progress.
// =========================================================================

use crate::pets_generate::ImageGenEndpoint;
use image::RgbaImage;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Job lifecycle (mirrors the hermes overlay stages: generating → pick →
/// hatching → adopted).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HatchStatus {
    GeneratingDrafts,
    AwaitingPick,
    Hatching,
    Done,
    Failed,
    Cancelled,
}

impl HatchStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            HatchStatus::Done | HatchStatus::Failed | HatchStatus::Cancelled
        )
    }
}

struct HatchJob {
    prompt: String,
    style: Option<String>,
    /// Display name (user-supplied or "" until derived at hatch time).
    name: String,
    status: HatchStatus,
    /// PNG-encoded base drafts awaiting a pick.
    drafts: Vec<Vec<u8>>,
    progress: VecDeque<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
    created_at: Instant,
}

fn hatch_registry() -> &'static Mutex<HashMap<String, Arc<Mutex<HatchJob>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Mutex<HatchJob>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

const HATCH_JOB_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_HATCH_JOBS: usize = 16;
const MAX_PROGRESS_EVENTS: usize = 256;

/// Drop expired jobs, then evict the oldest terminal job while over cap.
fn prune_jobs(registry: &mut HashMap<String, Arc<Mutex<HatchJob>>>) {
    registry.retain(|_, job| {
        job.lock()
            .map_or(true, |guard| guard.created_at.elapsed() < HATCH_JOB_TTL)
    });
    while registry.len() > MAX_HATCH_JOBS {
        let oldest = registry
            .iter()
            .min_by_key(|(_, job)| {
                job.lock()
                    .map(|guard| (!guard.status.is_terminal(), guard.created_at))
                    .unwrap_or((true, Instant::now()))
            })
            .map(|(id, _)| id.clone());
        match oldest {
            Some(id) => {
                registry.remove(&id);
            }
            None => break,
        }
    }
}

fn lookup_job(id: &str) -> Option<Arc<Mutex<HatchJob>>> {
    hatch_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
}

fn encode_png(image: &RgbaImage) -> Option<Vec<u8>> {
    let mut buffer: Vec<u8> = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .ok()?;
    Some(buffer)
}

/// Spawn the hatch phase for a job whose base draft is chosen: derives the
/// name/slug, records progress events on the job, adopts the pet on success
/// (hermes CLI parity: hatch ⇒ set active).
fn begin_hatch_phase(job_id: &str, base: RgbaImage, endpoint: ImageGenEndpoint) {
    let Some(job) = lookup_job(job_id) else {
        return;
    };
    let (prompt, style, name, cancel) = {
        let Ok(mut guard) = job.lock() else { return };
        let name = if guard.name.trim().is_empty() {
            crate::pets_generate::derive_pet_name(&guard.prompt)
        } else {
            guard.name.trim().to_string()
        };
        guard.name = name.clone();
        guard.status = HatchStatus::Hatching;
        (
            guard.prompt.clone(),
            guard.style.clone(),
            name,
            guard.cancel.clone(),
        )
    };
    let slug = crate::pets::slugify(&name);
    let job_id = job_id.to_string();
    let job_thread = job.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("hatch-rows-{job_id}"))
        .spawn(move || {
            let job = job_thread;
            let home = crate::config::ulnclaw_home();
            let progress_cb = |event: &str, detail: &str| {
                if let Ok(mut guard) = job.lock() {
                    guard.progress.push_back(json!({
                        "event": event,
                        "detail": detail,
                    }));
                    while guard.progress.len() > MAX_PROGRESS_EVENTS {
                        guard.progress.pop_front();
                    }
                }
            };
            let outcome = crate::pets_generate::hatch_pet(
                &home,
                &endpoint,
                &base,
                &slug,
                &name,
                &prompt,
                &prompt,
                style.as_deref(),
                Some(&progress_cb),
                Some(&cancel),
            );
            let Ok(mut guard) = job.lock() else { return };
            match outcome {
                Ok(result) => {
                    let _ = crate::pets::set_active(&result.slug);
                    guard.status = HatchStatus::Done;
                    guard.result = Some(json!({
                        "slug": result.slug,
                        "display_name": result.display_name,
                        "states": result.states,
                        "spritesheet": format!("/api/pets/{}/spritesheet", result.slug),
                    }));
                }
                Err(err) => {
                    if cancel.load(Ordering::SeqCst) {
                        guard.status = HatchStatus::Cancelled;
                    } else {
                        guard.status = HatchStatus::Failed;
                        guard.error = Some(err);
                    }
                }
            }
        });
    if spawn.is_err() {
        if let Ok(mut guard) = job.lock() {
            guard.status = HatchStatus::Failed;
            guard.error = Some("could not spawn hatch worker thread".to_string());
        }
    }
}

/// Body of `POST /api/pets/hatch`.
#[derive(Deserialize)]
pub struct StartHatchRequest {
    pub prompt: String,
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Number of base drafts (1–4, default 2 — hermes overlay shows a grid).
    #[serde(default)]
    pub drafts: Option<usize>,
    /// Skip the pick step: hatch draft 0 automatically (REPL `/hatch` mode).
    #[serde(default)]
    pub auto: bool,
}

/// `POST /api/pets/hatch` — start a hatch job.
pub async fn start_hatch(Json(body): Json<StartHatchRequest>) -> Response {
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return super::bad_request("prompt is required", None);
    }
    let draft_count = body.drafts.unwrap_or(2).clamp(1, 4);
    let endpoint = match crate::pets_generate::resolve_image_endpoint() {
        Ok(endpoint) => endpoint,
        Err(err) => return super::bad_request(&err, None),
    };

    let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = Arc::new(Mutex::new(HatchJob {
        prompt: prompt.clone(),
        style: body.style.clone(),
        name: body.name.as_deref().unwrap_or("").trim().to_string(),
        status: HatchStatus::GeneratingDrafts,
        drafts: Vec::new(),
        progress: VecDeque::new(),
        result: None,
        error: None,
        cancel: cancel.clone(),
        created_at: Instant::now(),
    }));
    {
        let mut registry = hatch_registry().lock().unwrap_or_else(|e| e.into_inner());
        prune_jobs(&mut registry);
        registry.insert(id.clone(), job.clone());
    }

    let job_id = id.clone();
    let style = body.style.clone();
    let auto = body.auto;
    let job_thread = job.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("hatch-drafts-{job_id}"))
        .spawn(move || {
            let job = job_thread;
            let outcome = crate::pets_generate::generate_base_drafts(
                &endpoint,
                &prompt,
                draft_count,
                style.as_deref(),
                None,
                Some(&cancel),
            );
            let encoded: Vec<Vec<u8>> = match &outcome {
                Ok(images) => images.iter().filter_map(encode_png).collect(),
                Err(_) => Vec::new(),
            };
            let Ok(mut guard) = job.lock() else { return };
            if guard.status != HatchStatus::GeneratingDrafts {
                return; // cancelled or otherwise settled while we worked
            }
            match outcome {
                Ok(images) if !encoded.is_empty() => {
                    if cancel.load(Ordering::SeqCst) {
                        guard.status = HatchStatus::Cancelled;
                        return;
                    }
                    guard.drafts = encoded;
                    if auto {
                        let Some(base) = images.into_iter().next() else {
                            guard.status = HatchStatus::Failed;
                            guard.error = Some("no base draft came back".to_string());
                            return;
                        };
                        drop(guard);
                        begin_hatch_phase(&job_id, base, endpoint);
                    } else {
                        guard.status = HatchStatus::AwaitingPick;
                    }
                }
                Ok(_) => {
                    guard.status = HatchStatus::Failed;
                    guard.error = Some("draft images could not be encoded".to_string());
                }
                Err(err) => {
                    if cancel.load(Ordering::SeqCst) {
                        guard.status = HatchStatus::Cancelled;
                    } else {
                        guard.status = HatchStatus::Failed;
                        guard.error = Some(err);
                    }
                }
            }
        });
    if spawn.is_err() {
        if let Ok(mut guard) = job.lock() {
            guard.status = HatchStatus::Failed;
            guard.error = Some("could not spawn draft worker thread".to_string());
        }
    }
    Json(json!({
        "object": "ulnclaw.pet.hatch.job",
        "job_id": id,
        "status": "generating_drafts",
    }))
    .into_response()
}

/// `GET /api/pets/hatch/:id` — poll job status/progress/result.
pub async fn hatch_status(Path(id): Path<String>) -> Response {
    let Some(job) = lookup_job(&id) else {
        return super::not_found(&format!("hatch job {id} not found"));
    };
    let Ok(guard) = job.lock() else {
        return super::server_error("hatch job lock poisoned");
    };
    let drafts: Vec<String> = (0..guard.drafts.len())
        .map(|i| format!("/api/pets/hatch/{id}/draft/{i}"))
        .collect();
    Json(json!({
        "object": "ulnclaw.pet.hatch.job",
        "job_id": id,
        "status": guard.status,
        "prompt": guard.prompt,
        "style": guard.style,
        "name": guard.name,
        "drafts": drafts,
        "progress": guard.progress,
        "result": guard.result,
        "error": guard.error,
    }))
    .into_response()
}

/// `GET /api/pets/hatch/:id/draft/:index` — PNG bytes of a base draft.
pub async fn draft_image(Path((id, index)): Path<(String, usize)>) -> Response {
    let Some(job) = lookup_job(&id) else {
        return super::not_found(&format!("hatch job {id} not found"));
    };
    let Ok(guard) = job.lock() else {
        return super::server_error("hatch job lock poisoned");
    };
    let Some(bytes) = guard.drafts.get(index).cloned() else {
        return super::not_found(&format!("draft {index} does not exist"));
    };
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

/// Body of `POST /api/pets/hatch/:id/pick`.
#[derive(Deserialize)]
pub struct PickDraftRequest {
    pub draft: usize,
    /// Optional display-name override for the hatched pet.
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST /api/pets/hatch/:id/pick` — choose a draft and start hatching.
pub async fn pick_draft(Path(id): Path<String>, Json(body): Json<PickDraftRequest>) -> Response {
    let Some(job) = lookup_job(&id) else {
        return super::not_found(&format!("hatch job {id} not found"));
    };
    let endpoint = match crate::pets_generate::resolve_image_endpoint() {
        Ok(endpoint) => endpoint,
        Err(err) => return super::bad_request(&err, None),
    };
    let draft_bytes = {
        let Ok(mut guard) = job.lock() else {
            return super::server_error("hatch job lock poisoned");
        };
        if guard.status != HatchStatus::AwaitingPick {
            return super::bad_request(
                &format!(
                    "job {id} is not awaiting a draft pick (status: {:?})",
                    guard.status
                ),
                None,
            );
        }
        let Some(bytes) = guard.drafts.get(body.draft).cloned() else {
            return super::bad_request(
                &format!(
                    "draft {} does not exist ({} available)",
                    body.draft,
                    guard.drafts.len()
                ),
                None,
            );
        };
        if let Some(name) = body
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
        {
            guard.name = name;
        }
        bytes
    };
    match image::load_from_memory(&draft_bytes) {
        Ok(img) => {
            begin_hatch_phase(&id, img.to_rgba8(), endpoint);
            hatch_status(Path(id)).await
        }
        Err(err) => {
            if let Ok(mut guard) = job.lock() {
                guard.status = HatchStatus::Failed;
                guard.error = Some(format!("decode picked draft: {err}"));
            }
            super::server_error("picked draft could not be decoded")
        }
    }
}

/// `POST /api/pets/hatch/:id/cancel` — ask a job to stop. Running phases
/// observe the flag at their next checkpoint; a job waiting for a pick is
/// cancelled immediately.
pub async fn cancel_hatch(Path(id): Path<String>) -> Response {
    let Some(job) = lookup_job(&id) else {
        return super::not_found(&format!("hatch job {id} not found"));
    };
    {
        let Ok(mut guard) = job.lock() else {
            return super::server_error("hatch job lock poisoned");
        };
        guard.cancel.store(true, Ordering::SeqCst);
        if guard.status == HatchStatus::AwaitingPick {
            guard.status = HatchStatus::Cancelled;
        }
    }
    hatch_status(Path(id)).await
}

#[cfg(test)]
mod hatch_tests {
    use super::*;
    use axum::extract::Path;

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 255, 255]));
        encode_png(&img).expect("png encodes")
    }

    /// Insert a synthetic job straight into the registry (no image calls).
    fn inject_job(status: HatchStatus, with_drafts: bool) -> String {
        let id = format!("test-{}", uuid::Uuid::new_v4().to_string()[..6].to_string());
        let job = Arc::new(Mutex::new(HatchJob {
            prompt: "a tiny cyber fox".to_string(),
            style: Some("pixel".to_string()),
            name: String::new(),
            status,
            drafts: if with_drafts {
                vec![tiny_png(), tiny_png()]
            } else {
                Vec::new()
            },
            progress: VecDeque::new(),
            result: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            created_at: Instant::now(),
        }));
        hatch_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), job);
        id
    }

    #[tokio::test]
    async fn start_hatch_requires_prompt() {
        let response = start_hatch(Json(StartHatchRequest {
            prompt: "   ".to_string(),
            style: None,
            name: None,
            drafts: None,
            auto: false,
        }))
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn hatch_status_reports_draft_urls_and_progress() {
        let id = inject_job(HatchStatus::AwaitingPick, true);
        let response = hatch_status(Path(id.clone())).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "awaiting_pick");
        assert_eq!(value["prompt"], "a tiny cyber fox");
        assert_eq!(value["drafts"][0], format!("/api/pets/hatch/{id}/draft/0"));
        assert_eq!(value["drafts"].as_array().unwrap().len(), 2);
        assert!(value["result"].is_null());
    }

    #[tokio::test]
    async fn draft_image_roundtrips_png_bytes() {
        let id = inject_job(HatchStatus::AwaitingPick, true);
        let response = draft_image(Path((id.clone(), 1))).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
        let missing = draft_image(Path((id, 9))).await;
        assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_settles_awaiting_pick_and_blocks_pick() {
        let id = inject_job(HatchStatus::AwaitingPick, true);
        let response = cancel_hatch(Path(id.clone())).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "cancelled");

        // A pick after cancel is rejected regardless of endpoint config.
        let response = pick_draft(
            Path(id),
            Json(PickDraftRequest {
                draft: 0,
                name: None,
            }),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_job_ids_404() {
        for response in [
            hatch_status(Path("nope-1".to_string())).await,
            draft_image(Path(("nope-1".to_string(), 0))).await,
            cancel_hatch(Path("nope-1".to_string())).await,
            pick_draft(
                Path("nope-1".to_string()),
                Json(PickDraftRequest {
                    draft: 0,
                    name: None,
                }),
            )
            .await,
        ] {
            assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn prune_jobs_prefers_terminal_eviction() {
        let mut registry: HashMap<String, Arc<Mutex<HatchJob>>> = HashMap::new();
        for i in 0..(MAX_HATCH_JOBS + 2) {
            let terminal = i % 2 == 0;
            let job = Arc::new(Mutex::new(HatchJob {
                prompt: format!("pet {i}"),
                style: None,
                name: String::new(),
                status: if terminal {
                    HatchStatus::Done
                } else {
                    HatchStatus::Hatching
                },
                drafts: Vec::new(),
                progress: VecDeque::new(),
                result: None,
                error: None,
                cancel: Arc::new(AtomicBool::new(false)),
                created_at: Instant::now() - Duration::from_secs(i as u64),
            }));
            registry.insert(format!("job-{i}"), job);
        }
        prune_jobs(&mut registry);
        assert_eq!(registry.len(), MAX_HATCH_JOBS);
        // The two oldest were evicted, and every evicted candidate was
        // terminal-or-oldest — no running job newer than a terminal one
        // may be dropped first.
        let surviving_terminal = registry
            .values()
            .filter(|job| {
                job.lock()
                    .map(|guard| guard.status.is_terminal())
                    .unwrap_or(false)
            })
            .count();
        assert!(surviving_terminal <= MAX_HATCH_JOBS / 2);
    }
}
