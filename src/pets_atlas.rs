//! Deterministic spritesheet assembly — generated row strips → Hermes atlas.
//! Port of hermes `agent/pet/generate/atlas.py` (v2026.8.3).
//!
//! Image-generation models are good at *drawing* a row of poses but bad at
//! exact grid geometry, so the model never owns the atlas layout: it produces
//! one loose horizontal strip per state, and these deterministic ops slice
//! that strip into clean, centered, transparent 192×208 cells and pack them
//! into the sheet the renderer reads.
//!
//! The atlas follows the petdex/Codex standard: 8 columns × 9 rows of
//! 192×208 cells (1536×1872), with the row order + per-row frame counts from
//! OpenAI's `hatch-pet` skill. Rows shorter than 8 columns leave the trailing
//! cells fully transparent.
//!
//! The frame-segmentation, fit-to-cell, and transparency-residue logic is
//! adapted from OpenAI's `hatch-pet` skill (openai/skills, Apache-2.0), as in
//! hermes.

use std::collections::VecDeque;

use image::{Rgba, RgbaImage};

pub const CELL_WIDTH: u32 = crate::pets::FRAME_W;
pub const CELL_HEIGHT: u32 = crate::pets::FRAME_H;

/// (state, row index, frame count). Order/row indices MUST match
/// `pets::CODEX_STATE_ROWS` so the renderer crops the right row for each
/// driven state.
pub const ROW_SPECS: &[(&str, u32, u32)] = &[
    ("idle", 0, 6),
    ("running-right", 1, 8),
    ("running-left", 2, 8),
    ("waving", 3, 4),
    ("jumping", 4, 5),
    ("failed", 5, 8),
    ("waiting", 6, 6),
    ("running", 7, 6),
    ("review", 8, 6),
];

pub const ROWS: usize = ROW_SPECS.len();
pub const COLUMNS: u32 = 8;
pub const ATLAS_WIDTH: u32 = COLUMNS * CELL_WIDTH;
pub const ATLAS_HEIGHT: u32 = ROWS as u32 * CELL_HEIGHT;

/// Frame count per driven state.
pub fn frame_count_for(state: &str) -> Option<u32> {
    ROW_SPECS
        .iter()
        .find(|(name, _, _)| *name == state)
        .map(|(_, _, count)| *count)
}

/// Alpha at/below which a pixel is "background" for component detection.
const ALPHA_FLOOR: u8 = 16;
/// Cell padding kept around a fitted sprite so poses never touch the edge.
const CELL_PAD: u32 = 10;
/// Margin for the normalized pass (hermes `_NORMALIZE_PAD`).
pub const NORMALIZE_PAD: u32 = 14;
/// Side-lobe cutoff for fitted frames (hermes `_SIDE_LOBE_RATIO`).
const SIDE_LOBE_RATIO: f64 = 0.18;

type Box4 = (u32, u32, u32, u32);

fn alpha_bbox(img: &RgbaImage) -> Option<Box4> {
    let mut left = u32::MAX;
    let mut top = u32::MAX;
    let mut right = 0u32;
    let mut bottom = 0u32;
    for (x, y, pixel) in img.enumerate_pixels() {
        if pixel[3] > 0 {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    if right == 0 || bottom == 0 {
        None
    } else {
        Some((left, top, right, bottom))
    }
}

fn crop(img: &RgbaImage, box4: Box4) -> RgbaImage {
    let (l, t, r, b) = box4;
    image::imageops::crop_imm(img, l, t, r - l, b - t).to_image()
}

// =========================================================================
// Background removal (hermes atlas background section)
// =========================================================================

fn color_distance(r: u8, g: u8, b: u8, key: (u8, u8, u8)) -> f64 {
    let dr = r as f64 - key.0 as f64;
    let dg = g as f64 - key.1 as f64;
    let db = b as f64 - key.2 as f64;
    (dr * dr + dg * dg + db * db).sqrt()
}

/// True if the strip already carries a real alpha background.
fn has_transparency(img: &RgbaImage) -> bool {
    let total = (img.width() * img.height()) as u64;
    let mut min_alpha = 255u8;
    let mut transparent = 0u64;
    for pixel in img.pixels() {
        let a = pixel[3];
        min_alpha = min_alpha.min(a);
        if a <= ALPHA_FLOOR {
            transparent += 1;
        }
    }
    if min_alpha > ALPHA_FLOOR {
        return false;
    }
    transparent * 20 > total // > 5%
}

/// Sample the four corners and return the most common opaque color.
fn dominant_corner_color(img: &RgbaImage) -> (u8, u8, u8) {
    let w = img.width();
    let h = img.height();
    let mut counts: Vec<((u8, u8, u8), usize)> = Vec::new();
    for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
        let pixel = img.get_pixel(x, y);
        if pixel[3] > ALPHA_FLOOR {
            let key = (pixel[0], pixel[1], pixel[2]);
            match counts.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 += 1,
                None => counts.push((key, 1)),
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(key, _)| key)
        .unwrap_or((0, 255, 0))
}

/// Per-pixel mask: true where a pixel is within `tol` per-channel of `key`.
fn near_key_mask(img: &RgbaImage, key: (u8, u8, u8), tol: i32) -> Vec<bool> {
    img.pixels()
        .map(|pixel| {
            (pixel[0] as i32 - key.0 as i32).abs() <= tol
                && (pixel[1] as i32 - key.1 as i32).abs() <= tol
                && (pixel[2] as i32 - key.2 as i32).abs() <= tol
        })
        .collect()
}

/// Shave the 1px antialiased edge ring left after keying (3×3 alpha min
/// filter — hermes `_defringe`).
fn defringe(img: &mut RgbaImage) {
    let w = img.width();
    let h = img.height();
    let alphas: Vec<u8> = img.pixels().map(|p| p[3]).collect();
    let mut eroded = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let mut min_a = 255u8;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    let a = if nx >= 0 && ny >= 0 && nx < w as i32 && ny < h as i32 {
                        alphas[(ny as u32 * w + nx as u32) as usize]
                    } else {
                        0
                    };
                    min_a = min_a.min(a);
                }
            }
            eroded[(y * w + x) as usize] = min_a;
        }
    }
    for (i, pixel) in img.pixels_mut().enumerate() {
        pixel[3] = eroded[i];
    }
}

/// Flood one transparent component from `start`; returns its pixel indices.
fn flood_transparent(img: &RgbaImage, start: usize, visited: &mut [bool]) -> Vec<usize> {
    let w = img.width() as usize;
    let h = img.height() as usize;
    let mut out = Vec::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    visited[start] = true;
    queue.push_back(start);
    while let Some(idx) = queue.pop_front() {
        out.push(idx);
        let x = idx % w;
        let y = idx / w;
        for (nx, ny) in [
            (x + 1, y),
            (x.wrapping_sub(1), y),
            (x, y + 1),
            (x, y.wrapping_sub(1)),
        ] {
            if nx < w && ny < h {
                let nidx = ny * w + nx;
                if !visited[nidx] && img.as_raw()[nidx * 4 + 3] <= ALPHA_FLOOR {
                    visited[nidx] = true;
                    queue.push_back(nidx);
                }
            }
        }
    }
    out
}

/// Fill transparent islands fully enclosed by opaque sprite pixels (hermes
/// `_repair_internal_alpha_holes`).
fn repair_internal_alpha_holes(img: &mut RgbaImage) {
    let w = img.width() as usize;
    let h = img.height() as usize;
    let mut visited = vec![false; w * h];

    // True background: transparent pixels reachable from the edge.
    for x in 0..w {
        for y in [0, h - 1] {
            let idx = y * w + x;
            if img.as_raw()[idx * 4 + 3] <= ALPHA_FLOOR && !visited[idx] {
                flood_transparent(img, idx, &mut visited);
            }
        }
    }
    for y in 0..h {
        for x in [0, w - 1] {
            let idx = y * w + x;
            if img.as_raw()[idx * 4 + 3] <= ALPHA_FLOOR && !visited[idx] {
                flood_transparent(img, idx, &mut visited);
            }
        }
    }

    // Remaining transparent components are enclosed holes — fill each with
    // the average color of its opaque neighbours.
    for start in 0..w * h {
        if visited[start] || img.as_raw()[start * 4 + 3] > ALPHA_FLOOR {
            continue;
        }
        let hole = flood_transparent(img, start, &mut visited);
        let hole_set: std::collections::HashSet<usize> = hole.iter().copied().collect();
        let mut sums = [0u64; 3];
        let mut samples = 0u64;
        for &idx in &hole {
            let x = idx % w;
            let y = idx / w;
            for (nx, ny) in [
                (x + 1, y),
                (x.wrapping_sub(1), y),
                (x, y + 1),
                (x, y.wrapping_sub(1)),
            ] {
                if nx < w && ny < h {
                    let nidx = ny * w + nx;
                    if !hole_set.contains(&nidx) {
                        let raw = &img.as_raw()[nidx * 4..nidx * 4 + 4];
                        if raw[3] > ALPHA_FLOOR {
                            sums[0] += raw[0] as u64;
                            sums[1] += raw[1] as u64;
                            sums[2] += raw[2] as u64;
                            samples += 1;
                        }
                    }
                }
            }
        }
        let fill: [u8; 4] = if samples == 0 {
            [0, 0, 0, 255]
        } else {
            [
                (sums[0] / samples) as u8,
                (sums[1] / samples) as u8,
                (sums[2] / samples) as u8,
                255,
            ]
        };
        for &idx in &hole {
            img.as_mut()[idx * 4..idx * 4 + 4].copy_from_slice(&fill);
        }
    }
}

/// Return `img` with its flat background keyed out to transparent (hermes
/// `remove_background`). Border flood-fill keeps interior pixels that merely
/// match the backdrop color; strongly-saturated chroma keys take the fast
/// near-key-mask path.
pub fn remove_background(img: &RgbaImage, chroma_key: Option<(u8, u8, u8)>) -> RgbaImage {
    let mut rgba = img.clone();
    if has_transparency(&rgba) {
        repair_internal_alpha_holes(&mut rgba);
        return rgba;
    }

    let key = chroma_key.unwrap_or_else(|| dominant_corner_color(&rgba));
    let w = rgba.width() as usize;
    let h = rgba.height() as usize;
    let threshold = 90.0f64;

    let is_bg = |idx: usize| {
        let raw = &rgba.as_raw()[idx * 4..idx * 4 + 4];
        raw[3] > ALPHA_FLOOR && color_distance(raw[0], raw[1], raw[2], key) <= threshold
    };

    let key_spread = key.0.max(key.1).max(key.2) as i32 - key.0.min(key.1).min(key.2) as i32;

    if key_spread >= 120 {
        // Fast path for strongly-saturated chroma keys: remove every near-key
        // opaque pixel (clears border backdrop + enclosed chroma pockets).
        let mask = near_key_mask(&rgba, key, 48);
        for (i, pixel) in rgba.pixels_mut().enumerate() {
            if mask[i] && pixel[3] > ALPHA_FLOOR {
                *pixel = Rgba([0, 0, 0, 0]);
            }
        }
        defringe(&mut rgba);
        return rgba;
    }

    // Border flood-fill for desaturated keys.
    let mut visited = vec![false; w * h];
    let mut remove = vec![false; w * h];
    let mut queue: VecDeque<usize> = VecDeque::new();

    for x in 0..w {
        for y in [0, h - 1] {
            let idx = y * w + x;
            if is_bg(idx) && !visited[idx] {
                visited[idx] = true;
                queue.push_back(idx);
            }
        }
    }
    for y in 0..h {
        for x in [0, w - 1] {
            let idx = y * w + x;
            if is_bg(idx) && !visited[idx] {
                visited[idx] = true;
                queue.push_back(idx);
            }
        }
    }

    while let Some(idx) = queue.pop_front() {
        remove[idx] = true;
        let x = idx % w;
        let y = idx / w;
        for (nx, ny) in [
            (x + 1, y),
            (x.wrapping_sub(1), y),
            (x, y + 1),
            (x, y.wrapping_sub(1)),
        ] {
            if nx < w && ny < h {
                let nidx = ny * w + nx;
                if !visited[nidx] {
                    visited[nidx] = true;
                    if is_bg(nidx) {
                        queue.push_back(nidx);
                    }
                }
            }
        }
    }

    for (i, pixel) in rgba.pixels_mut().enumerate() {
        if remove[i] {
            *pixel = Rgba([0, 0, 0, 0]);
        }
    }
    defringe(&mut rgba);
    rgba
}

// =========================================================================
// Frame extraction (hermes atlas extraction section)
// =========================================================================

/// Per-column alpha mass (hermes `_column_profile` — PIL resizes the alpha
/// channel to a 1px-tall strip, i.e. the per-column mean).
fn column_profile(img: &RgbaImage) -> Vec<i64> {
    let w = img.width() as usize;
    let h = img.height().max(1) as u64;
    let mut sums = vec![0u64; w];
    for (x, _y, pixel) in img.enumerate_pixels() {
        sums[x as usize] += pixel[3] as u64;
    }
    sums.into_iter().map(|sum| (sum / h) as i64).collect()
}

/// Contiguous column spans whose alpha mass exceeds `threshold` (hermes
/// `_content_runs`).
fn content_runs(profile: &[i64], threshold: i64) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (x, value) in profile.iter().enumerate() {
        if *value > threshold {
            if start.is_none() {
                start = Some(x);
            }
        } else if let Some(s) = start.take() {
            runs.push((s, x));
        }
    }
    if let Some(s) = start {
        runs.push((s, profile.len()));
    }
    runs
}

/// Remove tiny separated left/right lobes before fitting a frame (hermes
/// `_drop_side_bleed`).
fn drop_side_bleed(img: &RgbaImage) -> RgbaImage {
    let profile = column_profile(img);
    let runs = content_runs(&profile, 2);
    if runs.len() < 2 {
        return img.clone();
    }
    let masses: Vec<i64> = runs
        .iter()
        .map(|(l, r)| profile[*l..*r].iter().sum())
        .collect();
    let max_mass = *masses.iter().max().unwrap_or(&0);
    let keep_mass = (max_mass as f64 * SIDE_LOBE_RATIO) as i64;
    let keep: Vec<(usize, usize)> = runs
        .iter()
        .zip(masses.iter())
        .filter(|(_, mass)| **mass >= keep_mass)
        .map(|(run, _)| *run)
        .collect();
    if keep.len() == runs.len() {
        return img.clone();
    }
    let mut out = img.clone();
    let w = img.width();
    let h = img.height();
    let mut prev = 0usize;
    for (left, right) in &keep {
        for x in prev..(*left).min(w as usize) {
            for y in 0..h {
                *out.get_pixel_mut(x as u32, y) = Rgba([0, 0, 0, 0]);
            }
        }
        prev = *right;
    }
    for x in prev..w as usize {
        for y in 0..h {
            *out.get_pixel_mut(x as u32, y) = Rgba([0, 0, 0, 0]);
        }
    }
    out
}

/// Crop to content, scale to fit a padded cell, and center on transparent
/// (hermes `_fit_to_cell`). NEAREST resample keeps generated pixel-art crisp.
fn fit_to_cell(img: &RgbaImage) -> RgbaImage {
    let mut target = RgbaImage::from_pixel(CELL_WIDTH, CELL_HEIGHT, Rgba([0, 0, 0, 0]));
    let cleaned = drop_side_bleed(img);
    let Some(bbox) = alpha_bbox(&cleaned) else {
        return target;
    };
    let sprite = crop(&cleaned, bbox);
    let max_w = CELL_WIDTH - CELL_PAD;
    let max_h = CELL_HEIGHT - CELL_PAD;
    let scale = (max_w as f64 / sprite.width() as f64)
        .min(max_h as f64 / sprite.height() as f64)
        .min(1.0);
    let sprite = if scale != 1.0 {
        image::imageops::resize(
            &sprite,
            ((sprite.width() as f64 * scale).round() as u32).max(1),
            ((sprite.height() as f64 * scale).round() as u32).max(1),
            image::imageops::FilterType::Nearest,
        )
    } else {
        sprite
    };
    let left = (CELL_WIDTH - sprite.width()) / 2;
    let top = (CELL_HEIGHT - sprite.height()) / 2;
    image::imageops::overlay(&mut target, &sprite, left as i64, top as i64);
    target
}

/// Remove thin slot-spanning guide/floor/divider lines (hermes
/// `_erase_long_axis_lines`).
fn erase_long_axis_lines(img: &RgbaImage) -> RgbaImage {
    let mut rgba = img.clone();
    let w = rgba.width();
    let h = rgba.height();

    let thin_groups = |indices: Vec<u32>| -> Vec<(u32, u32)> {
        let mut groups = Vec::new();
        let mut start: Option<u32> = None;
        let mut prev: Option<u32> = None;
        for idx in indices {
            match (start, prev) {
                (None, _) => {
                    start = Some(idx);
                    prev = Some(idx);
                }
                (Some(s), Some(p)) if idx == p + 1 => prev = Some(idx),
                (Some(s), Some(p)) => {
                    if p - s + 1 <= 4 {
                        groups.push((s, p + 1));
                    }
                    start = Some(idx);
                    prev = Some(idx);
                }
                _ => {}
            }
        }
        if let (Some(s), Some(p)) = (start, prev) {
            if p - s + 1 <= 4 {
                groups.push((s, p + 1));
            }
        }
        groups
    };

    let alpha_at = |x: u32, y: u32| rgba.get_pixel(x, y)[3] > ALPHA_FLOOR;
    let wide_rows: Vec<u32> = (0..h)
        .filter(|y| (0..w).filter(|x| alpha_at(*x, *y)).count() as f64 >= w as f64 * 0.85)
        .collect();
    let tall_cols: Vec<u32> = (0..w)
        .filter(|x| (0..h).filter(|y| alpha_at(*x, *y)).count() as f64 >= h as f64 * 0.85)
        .collect();

    for (top, bottom) in thin_groups(wide_rows) {
        for y in top..bottom {
            for x in 0..w {
                *rgba.get_pixel_mut(x, y) = Rgba([0, 0, 0, 0]);
            }
        }
    }
    for (left, right) in thin_groups(tall_cols) {
        for x in left..right {
            for y in 0..h {
                *rgba.get_pixel_mut(x, y) = Rgba([0, 0, 0, 0]);
            }
        }
    }
    rgba
}

/// Connected opaque components as `[(bbox, mass)]` (hermes
/// `_component_boxes`).
fn component_boxes(img: &RgbaImage) -> Vec<(Box4, u64)> {
    let Some((l0, t0, r0, b0)) = alpha_bbox(img) else {
        return Vec::new();
    };
    let w = (r0 - l0) as usize;
    let h = (b0 - t0) as usize;
    let mut visited = vec![false; w * h];
    let mut out = Vec::new();
    let mut queue: VecDeque<usize> = VecDeque::new();

    for start in 0..w * h {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let sx = start % w;
        let sy = start / w;
        if img.get_pixel(l0 + sx as u32, t0 + sy as u32)[3] <= ALPHA_FLOOR {
            continue;
        }
        queue.clear();
        queue.push_back(start);
        let mut mass = 0u64;
        let (mut min_x, mut min_y) = (u32::MAX, u32::MAX);
        let (mut max_x, mut max_y) = (0u32, 0u32);
        while let Some(idx) = queue.pop_front() {
            mass += 1;
            let x = idx % w;
            let y = idx / w;
            let ax = l0 + x as u32;
            let ay = t0 + y as u32;
            min_x = min_x.min(ax);
            min_y = min_y.min(ay);
            max_x = max_x.max(ax + 1);
            max_y = max_y.max(ay + 1);
            for (nx, ny) in [
                (x + 1, y),
                (x.wrapping_sub(1), y),
                (x, y + 1),
                (x, y.wrapping_sub(1)),
            ] {
                if nx < w && ny < h {
                    let nidx = ny * w + nx;
                    if !visited[nidx] {
                        visited[nidx] = true;
                        if img.get_pixel(l0 + nx as u32, t0 + ny as u32)[3] > ALPHA_FLOOR {
                            queue.push_back(nidx);
                        }
                    }
                }
            }
        }
        out.push(((min_x, min_y, max_x, max_y), mass));
    }
    out
}

/// Merge disconnected parts that clearly belong to one subject (hermes
/// `_merge_related_boxes`).
fn merge_related_boxes(boxes: Vec<Box4>) -> Vec<Box4> {
    let mut boxes = boxes;
    let mut changed = true;
    while changed {
        changed = false;
        let mut merged: Vec<Box4> = Vec::new();
        let mut used = vec![false; boxes.len()];
        for i in 0..boxes.len() {
            if used[i] {
                continue;
            }
            let (mut al, mut at, mut ar, mut ab) = boxes[i];
            used[i] = true;
            for j in i + 1..boxes.len() {
                if used[j] {
                    continue;
                }
                let (bl, bt, br, bb) = boxes[j];
                let v_overlap = ab.min(bb).saturating_sub(at.max(bt));
                let min_h = ((ab - at).min(bb - bt)).max(1);
                let gap = al.max(bl).saturating_sub(ar.min(br));
                let min_w = ((ar - al).min(br - bl)).max(1);
                if v_overlap as f64 >= min_h as f64 * 0.45
                    && gap as f64 <= (14.0f64.max(min_w as f64 * 0.22))
                {
                    al = al.min(bl);
                    at = at.min(bt);
                    ar = ar.max(br);
                    ab = ab.max(bb);
                    used[j] = true;
                    changed = true;
                }
            }
            merged.push((al, at, ar, ab));
        }
        boxes = merged;
    }
    boxes
}

/// Group component boxes into visual rows, then sort left→right (hermes
/// `_group_component_rows`).
fn group_component_rows(boxes: &[Box4]) -> Vec<Vec<Box4>> {
    if boxes.is_empty() {
        return Vec::new();
    }
    let mut heights: Vec<u32> = boxes.iter().map(|b| (b.3 - b.1).max(1)).collect();
    heights.sort_unstable();
    let row_tol = (12.0f64).max(heights[heights.len() / 2] as f64 * 0.55);

    let mut rows: Vec<Vec<Box4>> = Vec::new();
    let mut centers: Vec<f64> = Vec::new();
    let mut sorted: Vec<Box4> = boxes.to_vec();
    sorted.sort_by(|a, b| {
        let ca = (a.1 + a.3) as f64 / 2.0;
        let cb = (b.1 + b.3) as f64 / 2.0;
        ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
    });
    for box4 in sorted {
        let cy = (box4.1 + box4.3) as f64 / 2.0;
        let mut placed = false;
        for (i, center) in centers.iter_mut().enumerate() {
            if (cy - *center).abs() <= row_tol {
                rows[i].push(box4);
                *center = rows[i]
                    .iter()
                    .map(|b| (b.1 + b.3) as f64 / 2.0)
                    .sum::<f64>()
                    / rows[i].len() as f64;
                placed = true;
                break;
            }
        }
        if !placed {
            rows.push(vec![box4]);
            centers.push(cy);
        }
    }
    let mut ordered: Vec<(f64, Vec<Box4>)> = centers.into_iter().zip(rows).collect();
    ordered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<Vec<Box4>> = ordered.into_iter().map(|(_, row)| row).collect();
    for row in &mut out {
        row.sort_by(|a, b| {
            let ca = (a.0 + a.2) as f64 / 2.0;
            let cb = (b.0 + b.2) as f64 / 2.0;
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    out
}

/// Keep the slot's real subject; drop detached effects/noise (hermes
/// `_isolate_slot_subject`).
fn isolate_slot_subject(img: &RgbaImage) -> RgbaImage {
    let rgba = erase_long_axis_lines(img);
    let comps = component_boxes(&rgba);
    if comps.is_empty() {
        return rgba;
    }
    let (main_box, main_mass) = comps.iter().max_by_key(|(_, mass)| *mass).copied().unwrap();
    let (ml, _mt, mr, _mb) = main_box;
    let mw = (mr - ml).max(1);
    let mut out = RgbaImage::from_pixel(rgba.width(), rgba.height(), Rgba([0, 0, 0, 0]));
    for (box4, mass) in &comps {
        let keep = if *box4 == main_box {
            true
        } else {
            let (left, _top, right, _bottom) = box4;
            let overlap = mr.min(*right).saturating_sub(ml.max(*left));
            let center_x = (*left + *right) as f64 / 2.0;
            let near_main = center_x >= (ml as f64 - mw as f64 * 0.25)
                && center_x <= (mr as f64 + mw as f64 * 0.25);
            (*mass as f64) >= (24.0f64).max(main_mass as f64 * 0.035)
                && ((overlap as f64) >= mw as f64 * 0.3 || near_main)
        };
        if keep {
            let piece = crop(&rgba, *box4);
            image::imageops::overlay(&mut out, &piece, box4.0 as i64, box4.1 as i64);
        }
    }
    out
}

/// True when content has empty room on all four slot edges (hermes
/// `_has_slot_padding`).
fn has_slot_padding(img: &RgbaImage) -> bool {
    let Some((left, top, right, bottom)) = alpha_bbox(img) else {
        return false;
    };
    let w = img.width();
    let h = img.height();
    let min_x = (4.0f64).max((12.0f64).min((w as f64 * 0.025).round()));
    let min_y = (4.0f64).max((16.0f64).min((h as f64 * 0.02).round()));
    left as f64 >= min_x
        && top as f64 >= min_y
        && (w - right) as f64 >= min_x
        && (h - bottom) as f64 >= min_y
}

fn slot_bounds(width: u32, frame_count: u32) -> Vec<(u32, u32)> {
    (0..frame_count)
        .map(|i| {
            (
                ((i * width) as f64 / frame_count as f64).round() as u32,
                (((i + 1) * width) as f64 / frame_count as f64).round() as u32,
            )
        })
        .collect()
}

/// Extract frame subjects as connected non-background objects (hermes
/// `_component_crops`).
fn component_crops(
    strip: &RgbaImage,
    frame_count: u32,
    require_padding: bool,
) -> Option<Vec<RgbaImage>> {
    let attempt = |source: &RgbaImage| -> Option<Vec<RgbaImage>> {
        let comps = component_boxes(source);
        if comps.is_empty() {
            return None;
        }
        let max_mass = comps.iter().map(|(_, mass)| *mass).max().unwrap_or(0);
        let subjects = merge_related_boxes(
            comps
                .iter()
                .filter(|(_, mass)| *mass >= (64u64).max((max_mass as f64 * 0.12) as u64))
                .map(|(box4, _)| *box4)
                .collect(),
        );
        if (subjects.len() as u32) < frame_count {
            return None;
        }
        let rows = group_component_rows(&subjects);
        let ordered: Vec<Box4> = rows
            .iter()
            .flatten()
            .copied()
            .take(frame_count as usize)
            .collect();
        if (ordered.len() as u32) < frame_count {
            return None;
        }
        if require_padding {
            let min_x = (4.0f64).max((12.0f64).min((source.width() as f64 * 0.01).round())) as u32;
            let min_y =
                (4.0f64).max((16.0f64).min((source.height() as f64 * 0.015).round())) as u32;
            for (left, top, right, bottom) in &ordered {
                if *left < min_x
                    || *top < min_y
                    || source.width() - right < min_x
                    || source.height() - bottom < min_y
                {
                    return None;
                }
            }
        }
        let multirow = rows.len() > 1;
        let mut frames = Vec::new();
        for (left, top, right, bottom) in &ordered {
            let pad_x = (8.0f64).max((right - left) as f64 * 0.08).round() as u32;
            let pad_y = (8.0f64).max((bottom - top) as f64 * 0.08).round() as u32;
            let crop_box = if multirow {
                (
                    left.saturating_sub(pad_x),
                    top.saturating_sub(pad_y),
                    (right + pad_x).min(source.width()),
                    (bottom + pad_y).min(source.height()),
                )
            } else if frame_count == 1 {
                (0, 0, source.width(), source.height())
            } else {
                // Preserve vertical motion for true one-row strips while
                // narrowing X around the object.
                (
                    left.saturating_sub(pad_x),
                    0,
                    (right + pad_x).min(source.width()),
                    source.height(),
                )
            };
            let mut frame = RgbaImage::from_pixel(
                crop_box.2 - crop_box.0,
                crop_box.3 - crop_box.1,
                Rgba([0, 0, 0, 0]),
            );
            let piece = crop(source, (*left, *top, *right, *bottom));
            image::imageops::overlay(
                &mut frame,
                &piece,
                (left - crop_box.0) as i64,
                (top - crop_box.1) as i64,
            );
            frames.push(frame);
        }
        Some(frames)
    };

    attempt(strip).or_else(|| attempt(&erase_long_axis_lines(strip)))
}

/// Cut thin vertical gutters at expected frame boundaries before labeling
/// (hermes `_sever_expected_gutters`).
fn sever_expected_gutters(strip: &RgbaImage, frame_count: u32) -> RgbaImage {
    if frame_count <= 1 {
        return strip.clone();
    }
    let mut out = strip.clone();
    let slot = out.width() as f64 / frame_count as f64;
    let half = (3.0f64).max((18.0f64).min((slot * 0.06).round())) as u32;
    for i in 1..frame_count {
        let x = (i as f64 * slot).round() as i64;
        let left = (x - half as i64).max(0) as u32;
        let right = ((x + half as i64 + 1) as u32).min(out.width());
        for gx in left..right {
            for gy in 0..out.height() {
                let pixel = out.get_pixel_mut(gx, gy);
                pixel[3] = 0;
            }
        }
    }
    out
}

/// Slice `strip` into `frame_count` uniform columns, each cleaned
/// independently (hermes `_slot_crops`).
fn slot_crops(
    strip: &RgbaImage,
    frame_count: u32,
    require_padding: bool,
) -> Option<Vec<RgbaImage>> {
    let h = strip.height();
    let mut frames = Vec::new();
    for (left, right) in slot_bounds(strip.width(), frame_count) {
        let slot_img = crop(strip, (left, 0, right, h));
        let slot_img = drop_side_bleed(&isolate_slot_subject(&slot_img));
        if require_padding && !has_slot_padding(&slot_img) {
            return None;
        }
        frames.push(slot_img);
    }
    Some(frames)
}

/// Per-frame `(left, right)` column ranges from the row's empty gutters
/// (hermes `_frame_x_ranges`).
fn frame_x_ranges(strip: &RgbaImage, frame_count: u32) -> Option<Vec<(u32, u32)>> {
    let profile = column_profile(strip);
    let mut runs = content_runs(&profile, 2);
    if runs.is_empty() {
        return None;
    }
    let masses: Vec<i64> = runs
        .iter()
        .map(|(l, r)| profile[*l..*r].iter().sum())
        .collect();
    let max_mass = *masses.iter().max().unwrap_or(&0);
    let floor = (max_mass as f64 * 0.02) as i64;
    runs = runs
        .into_iter()
        .zip(masses.iter())
        .filter(|(_, mass)| **mass >= floor)
        .map(|(run, _)| run)
        .collect();
    if (runs.len() as u32) < frame_count {
        return None;
    }
    let mut groups: Vec<[usize; 2]> = runs.iter().map(|(l, r)| [*l, *r]).collect();
    while groups.len() as u32 > frame_count {
        let gi = (0..groups.len() - 1)
            .min_by(|i, j| {
                (groups[*i + 1][0] - groups[*i][1]).cmp(&(groups[*j + 1][0] - groups[*j][1]))
            })
            .unwrap();
        groups[gi][1] = groups[gi + 1][1];
        groups.remove(gi + 1);
    }
    Some(groups.iter().map(|g| (g[0] as u32, g[1] as u32)).collect())
}

fn significant_subject_boxes(img: &RgbaImage) -> Vec<Box4> {
    let comps = component_boxes(img);
    if comps.is_empty() {
        return Vec::new();
    }
    let max_mass = comps.iter().map(|(_, mass)| *mass).max().unwrap_or(0);
    merge_related_boxes(
        comps
            .iter()
            .filter(|(_, mass)| *mass >= (32u64).max((max_mass as f64 * 0.12) as u64))
            .map(|(box4, _)| *box4)
            .collect(),
    )
}

/// Reject rows where one "frame" is really multiple poses (hermes
/// `_validate_extracted_frames`).
fn validate_extracted_frames(frames: &[RgbaImage], frame_count: u32) -> Result<(), String> {
    if frames.len() as u32 != frame_count {
        return Err(format!(
            "expected {} frames, got {}",
            frame_count,
            frames.len()
        ));
    }
    let mut boxes = Vec::new();
    for (i, frame) in frames.iter().enumerate() {
        let Some(bbox) = alpha_bbox(frame) else {
            return Err(format!("frame {i} is empty"));
        };
        if significant_subject_boxes(frame).len() >= 3 {
            return Err(format!("frame {i} contains multiple separated subjects"));
        }
        boxes.push(bbox);
    }
    if frame_count <= 1 {
        return Ok(());
    }
    let mut widths: Vec<u32> = boxes.iter().map(|b| b.2 - b.0).collect();
    let mut heights: Vec<u32> = boxes.iter().map(|b| b.3 - b.1).collect();
    widths.sort_unstable();
    heights.sort_unstable();
    let med_w = widths[widths.len() / 2].max(1);
    let med_h = heights[heights.len() / 2].max(1);
    for (i, (left, top, right, bottom)) in boxes.iter().enumerate() {
        let width = right - left;
        let height = bottom - top;
        if width as f64 > (med_w as f64 * 3.0).max(med_w as f64 + 96.0)
            && height as f64 <= med_h as f64 * 1.6
        {
            return Err(format!("frame {i} is a multi-pose width outlier"));
        }
    }
    Ok(())
}

/// Turn one generated row strip into `frame_count` frames (hermes
/// `extract_strip_frames`). Strict slot/component extraction first; gutter
/// salvage for `auto`. `fit` places each frame into a 192×208 cell;
/// `fit=false` keeps raw coordinate-aligned crops for `normalize_cells`.
pub fn extract_strip_frames(
    strip: &RgbaImage,
    frame_count: u32,
    chroma_key: Option<(u8, u8, u8)>,
    method: &str,
    fit: bool,
) -> Result<Vec<RgbaImage>, String> {
    let strip = remove_background(strip, chroma_key);

    let mut frames = component_crops(&strip, frame_count, true);
    if frames.is_none() {
        frames = slot_crops(&strip, frame_count, true);
    }
    if frames.is_none() {
        if method == "components" {
            return Err(format!(
                "could not segment {frame_count} padded sprites from strip"
            ));
        }
        frames = component_crops(&strip, frame_count, false);
    }
    if frames.is_none() {
        let mut source = &strip;
        let severed;
        let mut ranges = frame_x_ranges(source, frame_count);
        if ranges.is_none() {
            severed = sever_expected_gutters(&strip, frame_count);
            source = &severed;
            ranges = frame_x_ranges(source, frame_count);
        }
        frames = match ranges {
            None => Some(slot_crops(source, frame_count, false).unwrap_or_default()),
            Some(ranges) => {
                let h = source.height();
                let pad = (2.0f64)
                    .max(16.0f64.min((source.width() as f64 / frame_count.max(1) as f64) * 0.04))
                    .round() as u32;
                Some(
                    ranges
                        .iter()
                        .map(|(left, right)| {
                            let box4 = (
                                left.saturating_sub(pad),
                                0,
                                (*right + pad).min(source.width()),
                                h,
                            );
                            drop_side_bleed(&isolate_slot_subject(&crop(source, box4)))
                        })
                        .collect(),
                )
            }
        };
    }
    let frames = frames.unwrap_or_default();
    validate_extracted_frames(&frames, frame_count)?;
    Ok(if fit {
        frames.iter().map(fit_to_cell).collect()
    } else {
        frames
    })
}

// =========================================================================
// Normalization + composition (hermes atlas composition section)
// =========================================================================

/// Integer dx that best aligns `profile` onto `reference` by
/// cross-correlation (hermes `_best_shift` — 1-D phase correlation).
fn best_shift(reference: &[i64], profile: &[i64], window: i64) -> i64 {
    let n = reference.len() as i64;
    let mut best_score: Option<i128> = None;
    let mut best = 0i64;
    for d in -window..=window {
        let mut score: i128 = 0;
        let lo = 0i64.max(d);
        let hi = n.min(n + d);
        for x in lo..hi {
            score += reference[x as usize] as i128 * profile[(x - d) as usize] as i128;
        }
        if best_score.map_or(true, |s| score > s) {
            best_score = Some(score);
            best = d;
        }
    }
    best
}

/// Register every frame into a 192×208 cell — the deterministic anti-jitter
/// math (hermes `normalize_cells`): per-state xcorr registration, union-crop
/// through one shared window, single global scale keyed to median pose
/// height.
pub fn normalize_cells(
    frames_by_state: &std::collections::HashMap<String, Vec<RgbaImage>>,
    pad: u32,
) -> std::collections::HashMap<String, Vec<RgbaImage>> {
    let blank = || RgbaImage::from_pixel(CELL_WIDTH, CELL_HEIGHT, Rgba([0, 0, 0, 0]));
    let median = |values: &mut Vec<u32>| -> u32 {
        values.sort_unstable();
        values[values.len() / 2]
    };

    let mut out = std::collections::HashMap::new();
    // (aligned frames, union bbox, (median pose w, median pose h))
    let mut prepared: Vec<(String, Vec<RgbaImage>, Box4, (u32, u32))> = Vec::new();

    let target_w = CELL_WIDTH.saturating_sub(pad);
    let target_h = CELL_HEIGHT.saturating_sub(pad);

    for (state, frames) in frames_by_state {
        if frames.is_empty() {
            continue;
        }
        let any_content = frames.iter().any(|f| alpha_bbox(f).is_some());
        if !any_content {
            out.insert(state.clone(), vec![blank(); frames.len()]);
            continue;
        }

        // Pad every frame to a common canvas so profiles are comparable.
        let w0 = frames.iter().map(|f| f.width()).max().unwrap_or(1);
        let h0 = frames.iter().map(|f| f.height()).max().unwrap_or(1);
        let canvas: Vec<RgbaImage> = frames
            .iter()
            .map(|f| {
                if f.width() == w0 && f.height() == h0 {
                    f.clone()
                } else {
                    let mut padded = RgbaImage::from_pixel(w0, h0, Rgba([0, 0, 0, 0]));
                    image::imageops::overlay(&mut padded, f, 0, 0);
                    padded
                }
            })
            .collect();

        // Register horizontally: shift each frame to lock the body (xcorr).
        let profiles: Vec<Vec<i64>> = canvas.iter().map(column_profile).collect();
        let reference: Vec<i64> = (0..w0 as usize)
            .map(|x| {
                let mut column: Vec<i64> = profiles.iter().map(|p| p[x]).collect();
                column.sort_unstable();
                column[column.len() / 2]
            })
            .collect();
        let window = (8i64).max(w0 as i64 / 5);
        let margin = window;
        let mut aligned = Vec::new();
        for (frame, profile) in canvas.iter().zip(profiles.iter()) {
            let shift = best_shift(&reference, profile, window);
            let mut shifted = RgbaImage::from_pixel(w0 + 2 * margin as u32, h0, Rgba([0, 0, 0, 0]));
            image::imageops::overlay(&mut shifted, frame, margin + shift, 0);
            aligned.push(shifted);
        }

        let boxes: Vec<Box4> = aligned.iter().filter_map(alpha_bbox).collect();
        if boxes.is_empty() {
            out.insert(state.clone(), vec![blank(); frames.len()]);
            continue;
        }
        let left = boxes.iter().map(|b| b.0).min().unwrap();
        let top = boxes.iter().map(|b| b.1).min().unwrap();
        let right = boxes.iter().map(|b| b.2).max().unwrap();
        let bottom = boxes.iter().map(|b| b.3).max().unwrap();
        let pose_w = median(&mut boxes.iter().map(|b| b.2 - b.0).collect());
        let pose_h = median(&mut boxes.iter().map(|b| b.3 - b.1).collect());
        prepared.push((
            state.clone(),
            aligned,
            (left, top, right, bottom),
            (pose_w, pose_h),
        ));
    }

    if prepared.is_empty() {
        return out;
    }

    // Uniform apparent size: K caps the tallest/widest envelope in the cell.
    let mut k = target_h as f64;
    for (_state, _aligned, (left, top, right, bottom), (_pose_w, pose_h)) in &prepared {
        let uw = (right - left).max(1) as f64;
        let uh = (bottom - top).max(1) as f64;
        k = k.min(target_h as f64 * *pose_h as f64 / uh);
        k = k.min(target_w as f64 * *pose_h as f64 / uw);
    }

    for (state, aligned, (left, top, right, bottom), (_pose_w, pose_h)) in prepared {
        let uw = right - left;
        let uh = bottom - top;
        let scale = k / pose_h.max(1) as f64;
        let sw = ((uw as f64 * scale).round() as u32).max(1);
        let sh = ((uh as f64 * scale).round() as u32).max(1);
        let px = ((CELL_WIDTH as i64 - sw as i64) / 2) as i64;
        let py = ((CELL_HEIGHT as i64 - (pad / 2) as i64) - sh as i64) as i64;

        let mut cells = Vec::new();
        for frame in &aligned {
            let cropped = crop(frame, (left, top, right, bottom));
            let resized = if cropped.width() != sw || cropped.height() != sh {
                image::imageops::resize(&cropped, sw, sh, image::imageops::FilterType::Nearest)
            } else {
                cropped
            };
            let mut cell = blank();
            image::imageops::overlay(&mut cell, &resized, px, py);
            cells.push(cell);
        }
        out.insert(state, cells);
    }
    out
}

/// One frame from a standalone image (e.g. the base look) — hermes
/// `single_frame`.
pub fn single_frame(img: &RgbaImage, fit: bool) -> RgbaImage {
    let keyed = remove_background(img, None);
    if fit {
        fit_to_cell(&keyed)
    } else {
        drop_side_bleed(&keyed)
    }
}

/// Zero the RGB of fully-transparent pixels (no colored-halo residue).
pub fn clear_transparent_rgb(img: &mut RgbaImage) {
    for pixel in img.pixels_mut() {
        if pixel[3] == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
        }
    }
}

/// Horizontally flip each frame (derive `running-left` from
/// `running-right`, preserving frame order/timing).
pub fn mirror_frames(frames: &[RgbaImage]) -> Vec<RgbaImage> {
    frames
        .iter()
        .map(image::imageops::flip_horizontal)
        .collect()
}

/// Pack per-state frame lists into the Hermes atlas (RGBA, residue-cleared).
pub fn compose_atlas(
    frames_by_state: &std::collections::HashMap<String, Vec<RgbaImage>>,
) -> RgbaImage {
    let mut atlas = RgbaImage::from_pixel(ATLAS_WIDTH, ATLAS_HEIGHT, Rgba([0, 0, 0, 0]));
    for (state, row, count) in ROW_SPECS {
        let frames = frames_by_state.get(*state).cloned().unwrap_or_default();
        for (col, frame) in frames.iter().take(*count as usize).enumerate() {
            let cell = if frame.width() != CELL_WIDTH || frame.height() != CELL_HEIGHT {
                fit_to_cell(frame)
            } else {
                frame.clone()
            };
            image::imageops::overlay(
                &mut atlas,
                &cell,
                (col as u32 * CELL_WIDTH) as i64,
                (*row * CELL_HEIGHT) as i64,
            );
        }
    }
    clear_transparent_rgb(&mut atlas);
    atlas
}

/// Encode an atlas to sprite-sheet bytes. Known diff: the `image` crate has
/// no WebP encoder, so hatched sheets are PNG-encoded (format is sniffed at
/// decode time, so the store/renderer are unaffected).
pub fn atlas_to_sheet_bytes(atlas: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut buffer: Vec<u8> = Vec::new();
    atlas
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .map_err(|e| format!("atlas encode: {e}"))?;
    Ok(buffer)
}

/// Atlas validation result (hermes `validate_atlas` return shape).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AtlasValidation {
    pub ok: bool,
    pub width: u32,
    pub height: u32,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub filled_states: Vec<String>,
}

/// Check geometry, per-cell occupancy, and transparency invariants.
pub fn validate_atlas(atlas: &RgbaImage) -> AtlasValidation {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if atlas.width() != ATLAS_WIDTH || atlas.height() != ATLAS_HEIGHT {
        errors.push(format!(
            "expected {}x{}, got {}x{}",
            ATLAS_WIDTH,
            ATLAS_HEIGHT,
            atlas.width(),
            atlas.height()
        ));
        return AtlasValidation {
            ok: false,
            width: atlas.width(),
            height: atlas.height(),
            errors,
            warnings,
            filled_states: Vec::new(),
        };
    }

    let mut filled_states = Vec::new();
    let mut cell_boxes_by_state: Vec<(String, Vec<Box4>)> = Vec::new();
    for (state, row, count) in ROW_SPECS {
        let mut row_pixels = 0u64;
        let mut boxes = Vec::new();
        for col in 0..*count {
            let left = col * CELL_WIDTH;
            let top = row * CELL_HEIGHT;
            let cell = crop(atlas, (left, top, left + CELL_WIDTH, top + CELL_HEIGHT));
            let nonblank = cell.pixels().filter(|p| p[3] > 0).count() as u64;
            row_pixels += nonblank;
            if let Some(bbox) = alpha_bbox(&cell) {
                boxes.push(bbox);
            }
        }
        if row_pixels > 0 {
            filled_states.push(state.to_string());
            cell_boxes_by_state.push((state.to_string(), boxes));
        } else {
            warnings.push(format!("state '{state}' has no frames"));
        }
    }

    if filled_states.is_empty() {
        errors.push("atlas is empty — no state produced any frames".to_string());
    }

    let mut all_widths: Vec<u32> = Vec::new();
    let mut all_heights: Vec<u32> = Vec::new();
    for (_state, boxes) in &cell_boxes_by_state {
        for (left, top, right, bottom) in boxes {
            all_widths.push(right - left);
            all_heights.push(bottom - top);
        }
    }
    all_widths.sort_unstable();
    all_heights.sort_unstable();
    let mut global_med_w = 0u32;
    let mut global_med_h = 0u32;
    if !all_widths.is_empty() && !all_heights.is_empty() {
        global_med_w = all_widths[all_widths.len() / 2];
        let median_h = all_heights[all_heights.len() / 2];
        global_med_h = median_h;
        let min_h = (56.0f64).max((CELL_HEIGHT as f64 * 0.28).round()) as u32;
        if median_h < min_h {
            errors.push(format!(
                "atlas sprites are too small after normalization (median frame height {median_h}px)"
            ));
        }
    }

    for (state, boxes) in &cell_boxes_by_state {
        if boxes.len() <= 1 {
            continue;
        }
        let mut widths: Vec<u32> = boxes.iter().map(|b| b.2 - b.0).collect();
        let mut heights: Vec<u32> = boxes.iter().map(|b| b.3 - b.1).collect();
        widths.sort_unstable();
        heights.sort_unstable();
        let med_w = widths[widths.len() / 2].max(1);
        let med_h = heights[heights.len() / 2].max(1);
        let max_w = *widths.last().unwrap();
        let max_h = *heights.last().unwrap();
        if max_w as f64 > (med_w as f64 * 3.0).max(med_w as f64 + 96.0)
            && max_h as f64 <= med_h as f64 * 1.6
        {
            errors.push(format!(
                "state '{state}' contains a multi-pose frame outlier"
            ));
        }
        if global_med_w > 0 && global_med_h > 0 {
            let min_state_w = (32.0f64).max((global_med_w as f64 * 0.42).round()) as u32;
            let min_state_h = (40.0f64).max((global_med_h as f64 * 0.50).round()) as u32;
            if med_w < min_state_w || med_h < min_state_h {
                errors.push(format!(
                    "state '{state}' appears collapsed (median {med_w}x{med_h}px, global median {global_med_w}x{global_med_h}px)"
                ));
            }
        }
    }

    // Transparent pixels must carry zero RGB (no halo residue).
    let residue = atlas
        .pixels()
        .filter(|p| p[3] == 0 && (p[0] != 0 || p[1] != 0 || p[2] != 0))
        .count();
    if residue > 0 {
        errors.push(format!("{residue} transparent pixels retain RGB residue"));
    }

    AtlasValidation {
        ok: errors.is_empty(),
        width: atlas.width(),
        height: atlas.height(),
        errors,
        warnings,
        filled_states,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(color))
    }

    /// A synthetic strip: `count` separated square subjects on a magenta bg.
    fn synthetic_strip(count: u32) -> RgbaImage {
        let w = 128 * count;
        let h = 160u32;
        let mut img = solid(w, h, [255, 0, 255, 255]);
        for i in 0..count {
            let cx = i * 128 + 40;
            for y in 30..130 {
                for x in cx..cx + 48 {
                    *img.get_pixel_mut(x, y) = Rgba([20, 200, 20, 255]);
                }
            }
        }
        img
    }

    #[test]
    fn background_removal_keys_magenta() {
        let strip = synthetic_strip(2);
        let keyed = remove_background(&strip, None);
        // Corners keyed out, subject kept.
        assert_eq!(*keyed.get_pixel(0, 0), Rgba([0, 0, 0, 0]));
        assert_eq!(keyed.get_pixel(64, 80)[3], 255);
        let transparent = keyed.pixels().filter(|p| p[3] == 0).count();
        assert!(transparent as u32 > keyed.width() * keyed.height() / 2);
    }

    #[test]
    fn background_removal_respects_existing_alpha() {
        let mut img = solid(64, 64, [10, 10, 10, 255]);
        for y in 0..64 {
            for x in 0..32 {
                *img.get_pixel_mut(x, y) = Rgba([0, 0, 0, 0]);
            }
        }
        let out = remove_background(&img, None);
        assert_eq!(out.get_pixel(16, 32)[3], 0);
        assert_eq!(out.get_pixel(48, 32)[3], 255);
    }

    #[test]
    fn internal_alpha_holes_get_filled() {
        // A ring with a transparent hole in the middle (already-transparent
        // image → repair path).
        let mut img = solid(60, 60, [0, 0, 0, 0]);
        for y in 10..50 {
            for x in 10..50 {
                let in_hole = x >= 25 && x < 35 && y >= 25 && y < 35;
                if !in_hole {
                    *img.get_pixel_mut(x, y) = Rgba([100, 100, 100, 255]);
                }
            }
        }
        let out = remove_background(&img, None);
        // Hole filled with neighbour average, outside stays transparent.
        assert!(out.get_pixel(30, 30)[3] == 255);
        assert!(out.get_pixel(0, 0)[3] == 0);
    }

    #[test]
    fn extract_frames_from_clean_strip() {
        let strip = synthetic_strip(4);
        let frames = extract_strip_frames(&strip, 4, None, "components", true).unwrap();
        assert_eq!(frames.len(), 4);
        for frame in &frames {
            assert_eq!(frame.width(), CELL_WIDTH);
            assert_eq!(frame.height(), CELL_HEIGHT);
            assert!(alpha_bbox(frame).is_some());
        }
        // auto method works too.
        let frames = extract_strip_frames(&strip, 4, None, "auto", false).unwrap();
        assert_eq!(frames.len(), 4);
    }

    #[test]
    fn extract_components_rejects_when_missing_padding() {
        // Subjects flush against the strip edge → strict padding fails,
        // components method raises.
        let mut img = solid(256, 100, [255, 0, 255, 255]);
        for y in 0..100 {
            for x in 0..60 {
                *img.get_pixel_mut(x, y) = Rgba([10, 10, 200, 255]);
            }
            for x in 128..188 {
                *img.get_pixel_mut(x, y) = Rgba([10, 10, 200, 255]);
            }
        }
        let result = extract_strip_frames(&img, 2, None, "components", true);
        assert!(result.is_err());
    }

    #[test]
    fn fit_to_cell_centers_and_caps() {
        let big = solid(400, 400, [9, 9, 9, 255]);
        let cell = fit_to_cell(&big);
        assert_eq!(cell.width(), CELL_WIDTH);
        assert_eq!(cell.height(), CELL_HEIGHT);
        let bbox = alpha_bbox(&cell).unwrap();
        assert!(bbox.2 - bbox.0 <= CELL_WIDTH - CELL_PAD);
        assert!(bbox.3 - bbox.1 <= CELL_HEIGHT - CELL_PAD);
    }

    #[test]
    fn mirror_and_single_frame() {
        let mut img = solid(60, 40, [0, 0, 0, 0]);
        for y in 5..35 {
            for x in 5..15 {
                *img.get_pixel_mut(x, y) = Rgba([200, 20, 20, 255]);
            }
        }
        let frames = vec![img.clone()];
        let mirrored = mirror_frames(&frames);
        let bbox = alpha_bbox(&mirrored[0]).unwrap();
        assert!(bbox.0 > 40); // content moved to the right side
        let single = single_frame(&img, true);
        assert_eq!(single.width(), CELL_WIDTH);
    }

    #[test]
    fn compose_and_validate_roundtrip() {
        let cell = fit_to_cell(&solid(100, 120, [50, 60, 70, 255]));
        let mut frames = std::collections::HashMap::new();
        for (state, _row, count) in ROW_SPECS {
            frames.insert(state.to_string(), vec![cell.clone(); *count as usize]);
        }
        let atlas = compose_atlas(&frames);
        assert_eq!(atlas.width(), ATLAS_WIDTH);
        assert_eq!(atlas.height(), ATLAS_HEIGHT);
        let validation = validate_atlas(&atlas);
        assert!(validation.ok, "errors: {:?}", validation.errors);
        assert_eq!(validation.filled_states.len(), ROW_SPECS.len());
    }

    #[test]
    fn validate_rejects_wrong_geometry() {
        let small = solid(100, 100, [0, 0, 0, 0]);
        let validation = validate_atlas(&small);
        assert!(!validation.ok);
        assert!(validation.errors[0].contains("expected"));
    }

    #[test]
    fn normalize_cells_registers_shared_scale() {
        // Two frames in one state at different x offsets → normalization
        // must produce identically-sized cells.
        let mut a = solid(200, 100, [0, 0, 0, 0]);
        for y in 20..80 {
            for x in 30..90 {
                *a.get_pixel_mut(x, y) = Rgba([200, 100, 0, 255]);
            }
        }
        let mut b = solid(200, 100, [0, 0, 0, 0]);
        for y in 20..80 {
            for x in 110..170 {
                *b.get_pixel_mut(x, y) = Rgba([200, 100, 0, 255]);
            }
        }
        let mut frames = std::collections::HashMap::new();
        frames.insert("idle".to_string(), vec![a, b]);
        let normalized = normalize_cells(&frames, NORMALIZE_PAD);
        let cells = &normalized["idle"];
        assert_eq!(cells.len(), 2);
        let box_a = alpha_bbox(&cells[0]).unwrap();
        let box_b = alpha_bbox(&cells[1]).unwrap();
        // Same size after shared registration/scale.
        assert_eq!(box_a.2 - box_a.0, box_b.2 - box_b.0);
        assert_eq!(box_a.3 - box_a.1, box_b.3 - box_b.1);
    }
}
