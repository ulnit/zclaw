//! Timeline renderer for the learning graph — port of hermes
//! `agent/learning_graph_render.py` (v2026.8.3).
//!
//! The desktop app paints a GPU radial constellation; a terminal can't, so
//! this is a *rendition* of the same data as a timeline bar chart — date
//! rows, proportional skill/memory bars colored by the day's dominant
//! category, and a cumulative trajectory sparkline. The age gradient and
//! complementary memory ink are ported from the desktop source, not
//! guessed.
//!
//! Grids are emitted as style runs — `(text, style, alpha, hex?)` — so
//! each consumer maps the semantic style + brightness onto its own
//! palette; the optional hex overrides the base color (category heatmap).

use serde_json::Value;

/// time-axis.ts LEAD_IN: the oldest node sits just off recency 0.
pub const LEAD_IN: f64 = 0.06;

/// constants.ts AGE_GRADIENT — old quiet, recent bright.
const AGE_OLD_INK: f64 = 0.42;
const AGE_MID_INK: f64 = 0.74;
const AGE_NEW_INK: f64 = 0.95;
const AGE_MID: f64 = 0.52;

/// Style keys consumers map to base colors (brightness = the run alpha).
pub const STYLE_BG: &str = "bg";
pub const STYLE_SKILL: &str = "skill";
pub const STYLE_MEMORY: &str = "memory";
pub const STYLE_LABEL: &str = "label";
pub const STYLE_DIM: &str = "dim";

/// Legend glyphs mirror NODE_SHAPE (skill = circle, memory = diamond).
pub const SKILL_GLYPH: &str = "●";
pub const MEMORY_GLYPH: &str = "◆";
const LABEL_KEYS: &[char] = &['1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c'];

/// One style run: `(text, style, alpha, optional hex override)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    pub style: String,
    pub alpha: f64,
    pub hex: Option<String>,
}

pub type Row = Vec<Run>;
pub type Grid = Vec<Row>;

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

fn smoothstep(p: f64) -> f64 {
    let p = clamp(p, 0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

/// Port of geometry.ts `recencyInk` — smoothstep age → ink alpha.
pub fn recency_ink(rec: f64) -> f64 {
    let t = clamp(rec, 0.0, 1.0);
    if t <= AGE_MID {
        AGE_OLD_INK + (AGE_MID_INK - AGE_OLD_INK) * smoothstep(t / AGE_MID)
    } else {
        AGE_MID_INK + (AGE_NEW_INK - AGE_MID_INK) * smoothstep((t - AGE_MID) / (1.0 - AGE_MID))
    }
}

pub fn format_date(ts: Option<f64>) -> String {
    let Some(ts) = ts else {
        return "unknown".to_string();
    };
    match chrono::DateTime::from_timestamp(ts as i64, 0) {
        Some(dt) => dt.format("%-d %b %Y").to_string(),
        None => "unknown".to_string(),
    }
}

fn to_ts(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Port of time-axis.ts `computeRecency` (id → recency ratio, timed flag).
pub fn compute_recency(nodes: &[Value]) -> Recency {
    let known: Vec<f64> = nodes
        .iter()
        .filter_map(|n| n.get("timestamp"))
        .filter_map(to_ts)
        .collect();
    let min_ts = known.iter().cloned().reduce(f64::min);
    let max_ts = known.iter().cloned().reduce(f64::max);
    let timed = match (min_ts, max_ts) {
        (Some(lo), Some(hi)) => hi > lo,
        _ => false,
    };

    let mut ordered: Vec<&Value> = nodes.iter().collect();
    ordered.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(to_ts).unwrap_or(f64::INFINITY);
        let tb = b.get("timestamp").and_then(to_ts).unwrap_or(f64::INFINITY);
        ta.partial_cmp(&tb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| node_id(a).cmp(&node_id(b)))
    });
    let last = ordered.len().saturating_sub(1).max(1) as f64;
    let mut ord_ratio: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for (i, node) in ordered.iter().enumerate() {
        let ratio = if nodes.len() > 1 {
            i as f64 / last
        } else {
            0.0
        };
        ord_ratio.insert(node_id(node), ratio);
    }

    let mut rec: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for node in nodes {
        let id = node_id(node);
        let ts = node.get("timestamp").and_then(to_ts);
        let ratio = if timed && ts.is_some() && min_ts.is_some() && max_ts.is_some() {
            (ts.unwrap() - min_ts.unwrap()) / (max_ts.unwrap() - min_ts.unwrap())
        } else {
            ord_ratio.get(&id).copied().unwrap_or(0.0)
        };
        rec.insert(id, LEAD_IN + (1.0 - LEAD_IN) * clamp(ratio, 0.0, 1.0));
    }
    Recency {
        rec,
        timed,
        min_ts,
        max_ts,
    }
}

pub struct Recency {
    pub rec: std::collections::HashMap<String, f64>,
    pub timed: bool,
    pub min_ts: Option<f64>,
    pub max_ts: Option<f64>,
}

fn date_at(rec: &Recency, reveal: f64) -> Option<f64> {
    if !rec.timed {
        return None;
    }
    let (lo, hi) = (rec.min_ts?, rec.max_ts?);
    Some((lo + clamp(reveal, 0.0, 1.0) * (hi - lo)).round())
}

fn node_id(node: &Value) -> String {
    node.get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Color — ported from color.ts so memory ink + age fade match the desktop
// ---------------------------------------------------------------------------

pub fn hex_to_rgb(s: &str) -> (u8, u8, u8) {
    let s = s.trim().trim_start_matches('#');
    let expanded: String = if s.len() == 3 {
        s.chars().flat_map(|c| [c, c]).collect()
    } else {
        s.to_string()
    };
    let bytes: Vec<u8> = (0..3)
        .filter_map(|i| u8::from_str_radix(expanded.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect();
    if bytes.len() == 3 {
        (bytes[0], bytes[1], bytes[2])
    } else {
        (255, 215, 0)
    }
}

pub fn rgb_to_hex(c: (f64, f64, f64)) -> String {
    format!(
        "#{:02X}{:02X}{:02X}",
        clamp(c.0, 0.0, 255.0) as u8,
        clamp(c.1, 0.0, 255.0) as u8,
        clamp(c.2, 0.0, 255.0) as u8
    )
}

pub fn mix_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> (f64, f64, f64) {
    let p = clamp(t, 0.0, 1.0);
    (
        (a.0 as f64 + (b.0 as f64 - a.0 as f64) * p).round(),
        (a.1 as f64 + (b.1 as f64 - a.1 as f64) * p).round(),
        (a.2 as f64 + (b.2 as f64 - a.2 as f64) * p).round(),
    )
}

fn rgb_to_hsl(c: (u8, u8, u8)) -> (f64, f64, f64) {
    let r = c.0 as f64 / 255.0;
    let g = c.1 as f64 / 255.0;
    let b = c.2 as f64 / 255.0;
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let light = (mx + mn) / 2.0;
    let d = mx - mn;
    if d == 0.0 {
        return (0.0, 0.0, light);
    }
    let s = if light > 0.5 {
        d / (2.0 - mx - mn)
    } else {
        d / (mx + mn)
    };
    let h = if mx == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, light)
}

fn hsl_to_rgb(h: f64, s: f64, light: f64) -> (u8, u8, u8) {
    let hue = ((h % 360.0) + 360.0) % 360.0;
    let c = (1.0 - (2.0 * light - 1.0).abs()) * s;
    let x = c * (1.0 - (((hue / 60.0) % 2.0) - 1.0).abs());
    let m = light - c / 2.0;
    let (r, g, b) = if hue < 60.0 {
        (c, x, 0.0)
    } else if hue < 120.0 {
        (x, c, 0.0)
    } else if hue < 180.0 {
        (0.0, c, x)
    } else if hue < 240.0 {
        (0.0, x, c)
    } else if hue < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

fn complementary_ink(c: (u8, u8, u8)) -> (u8, u8, u8) {
    let (h, s, light) = rgb_to_hsl(c);
    hsl_to_rgb(h + 165.0, s.max(0.5), clamp(light, 0.5, 0.7))
}

/// Port of color.ts `computePalette` (the bits a terminal needs).
pub fn derive_palette(primary_hex: &str, dark: bool) -> std::collections::HashMap<String, String> {
    let primary = hex_to_rgb(primary_hex);
    let base = if dark { (255, 255, 255) } else { (0, 0, 0) };
    let bg = if dark { (8, 8, 12) } else { (250, 250, 250) };
    let mut map = std::collections::HashMap::new();
    map.insert("primary".to_string(), primary_hex.to_string());
    // Memories are drillable → primary "clickable" ink; skills are
    // dead-ends → muted complement.
    map.insert(
        "memory".to_string(),
        rgb_to_hex(mix_rgb(primary, base, if dark { 0.12 } else { 0.18 })),
    );
    map.insert(
        "skill".to_string(),
        rgb_to_hex(mix_rgb(complementary_ink(primary), bg, 0.45)),
    );
    map.insert("label".to_string(), rgb_to_hex(mix_rgb(base, bg, 0.35)));
    map.insert("dim".to_string(), rgb_to_hex(mix_rgb(base, bg, 0.7)));
    map.insert(
        "bg".to_string(),
        rgb_to_hex((bg.0 as f64, bg.1 as f64, bg.2 as f64)),
    );
    map
}

/// Fade `base` toward the palette background by `alpha` (rgba-over-bg).
pub fn fade(
    palette: &std::collections::HashMap<String, String>,
    base: Option<&str>,
    alpha: f64,
) -> Option<String> {
    let base = base?;
    if alpha >= 0.999 {
        return Some(base.to_string());
    }
    let bg = hex_to_rgb(palette.get("bg").map(|s| s.as_str()).unwrap_or("#08080C"));
    let fg = hex_to_rgb(base);
    Some(rgb_to_hex(mix_rgb(bg, fg, alpha)))
}

fn node_score(node: &Value, rec: f64) -> f64 {
    if node.get("kind").and_then(|v| v.as_str()) == Some("memory") {
        return 3.5 + rec;
    }
    let use_count = node.get("useCount").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let pinned = node
        .get("pinned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    rec * 2.0 + use_count.max(0.0).sqrt() + if pinned { 2.0 } else { 0.0 }
}

fn node_label(node: &Value) -> String {
    let text = node
        .get("label")
        .or_else(|| node.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .trim()
        .to_string();
    if text.chars().count() <= 26 {
        text
    } else {
        let cut: String = text.chars().take(23).collect();
        format!("{}…", cut.trim_end())
    }
}

fn node_meta(node: &Value) -> String {
    if node.get("kind").and_then(|v| v.as_str()) == Some("memory") {
        let source = if node.get("memorySource").and_then(|v| v.as_str()) == Some("profile") {
            "profile memory"
        } else {
            "memory"
        };
        return format!(
            "{} · {}",
            source,
            format_date(node.get("timestamp").and_then(to_ts))
        );
    }
    let mut bits = vec![
        node.get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("skill")
            .to_string(),
        format_date(node.get("timestamp").and_then(to_ts)),
    ];
    let count = node.get("useCount").and_then(|v| v.as_i64()).unwrap_or(0);
    if count > 0 {
        bits.push(format!("x{}", count));
    }
    if node
        .get("pinned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        bits.push("pinned".to_string());
    }
    bits.join(" · ")
}

// ---------------------------------------------------------------------------
// Timeline chart frame
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ChartBucket {
    label: String,
    ts: f64,
    skills: usize,
    memories: usize,
    nodes: Vec<Value>,
    rec: f64,
}

impl ChartBucket {
    fn new(label: String, ts: f64) -> Self {
        Self {
            label,
            ts,
            skills: 0,
            memories: 0,
            nodes: Vec::new(),
            rec: 1.0,
        }
    }

    fn total(&self) -> usize {
        self.skills + self.memories
    }

    fn add(&mut self, node: Value) {
        if node.get("kind").and_then(|v| v.as_str()) == Some("memory") {
            self.memories += 1;
        } else {
            self.skills += 1;
        }
        self.nodes.push(node);
    }
}

fn period_key(ts: f64, granularity: &str) -> (i32, u32, u32) {
    let dt = chrono::DateTime::from_timestamp(ts as i64, 0)
        .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
    use chrono::Datelike;
    match granularity {
        "day" => (dt.year(), dt.month(), dt.day()),
        "month" => (dt.year(), dt.month(), 0),
        _ => (dt.year(), 0, 0),
    }
}

fn period_label(ts: f64, granularity: &str) -> String {
    let dt = chrono::DateTime::from_timestamp(ts as i64, 0)
        .unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH);
    match granularity {
        "day" => dt.format("%-d %b").to_string(),
        "month" => dt.format("%b %Y").to_string(),
        _ => dt.format("%Y").to_string(),
    }
}

/// Timeline rows: finest date granularity that fits, oldest → newest.
fn build_chart_buckets(nodes: &[Value], rec: &Recency, max_rows: usize) -> Vec<ChartBucket> {
    if nodes.is_empty() {
        return Vec::new();
    }
    if !rec.timed {
        let mut ordered: Vec<&Value> = nodes.iter().collect();
        ordered.sort_by(|a, b| {
            let ra = rec.rec.get(&node_id(a)).copied().unwrap_or(0.0);
            let rb = rec.rec.get(&node_id(b)).copied().unwrap_or(0.0);
            ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
        });
        let n_bins = max_rows.min(ordered.len().max(1));
        let mut buckets: Vec<ChartBucket> = (0..n_bins)
            .map(|i| ChartBucket::new(format!("#{}", i + 1), i as f64))
            .collect();
        for node in ordered {
            let r = rec.rec.get(&node_id(node)).copied().unwrap_or(0.0);
            let idx = ((r * n_bins as f64).floor() as isize).clamp(0, n_bins as isize - 1) as usize;
            buckets[idx].add(node.clone());
        }
        return buckets;
    }

    let mut chosen: Option<Vec<ChartBucket>> = None;
    for granularity in ["day", "month", "year"] {
        let mut groups: std::collections::BTreeMap<(i32, u32, u32), ChartBucket> =
            std::collections::BTreeMap::new();
        for node in nodes {
            let Some(ts) = node.get("timestamp").and_then(to_ts) else {
                continue;
            };
            let key = period_key(ts, granularity);
            let bucket = groups
                .entry(key)
                .or_insert_with(|| ChartBucket::new(period_label(ts, granularity), ts));
            bucket.add(node.clone());
        }
        // For short spans, keep the useful day-by-day graph even when the
        // caller asked for fewer rows; terminal scrollback is better than
        // collapsing a month of activity into one unreadable bar.
        if groups.len() <= max_rows || (granularity == "day" && groups.len() <= 32) {
            chosen = Some(groups.into_values().collect());
            break;
        }
    }

    let mut chosen = match chosen {
        Some(c) => c,
        None => {
            // Even yearly buckets overflow — fall back to even time bins.
            let (min_ts, max_ts) = (rec.min_ts.unwrap_or(0.0), rec.max_ts.unwrap_or(0.0));
            let n_bins = max_rows.max(1);
            let mut buckets = Vec::new();
            for i in 0..n_bins {
                let ts = if min_ts > 0.0 && max_ts > min_ts {
                    min_ts + (i as f64 / (n_bins - 1).max(1) as f64) * (max_ts - min_ts)
                } else {
                    i as f64
                };
                buckets.push(ChartBucket::new(format_date(Some(ts)), ts));
            }
            for node in nodes {
                let r = rec.rec.get(&node_id(node)).copied().unwrap_or(0.0);
                let idx =
                    ((r * n_bins as f64).floor() as isize).clamp(0, n_bins as isize - 1) as usize;
                buckets[idx].add(node.clone());
            }
            buckets
        }
    };

    let span = match (rec.min_ts, rec.max_ts) {
        (Some(lo), Some(hi)) if hi > lo => hi - lo,
        _ => 0.0,
    };
    for bucket in chosen.iter_mut() {
        bucket.rec = if span > 0.0 {
            LEAD_IN + (1.0 - LEAD_IN) * ((bucket.ts - rec.min_ts.unwrap()) / span)
        } else {
            1.0
        };
    }
    chosen
}

fn bucket_label_node(bucket: &ChartBucket) -> Option<&Value> {
    bucket.nodes.iter().max_by(|a, b| {
        let sa = node_score(a, a.get("timestamp").and_then(to_ts).unwrap_or(bucket.ts));
        let sb = node_score(b, b.get("timestamp").and_then(to_ts).unwrap_or(bucket.ts));
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn bucket_category(bucket: &ChartBucket) -> Option<String> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for node in &bucket.nodes {
        if node.get("kind").and_then(|v| v.as_str()) == Some("memory") {
            continue;
        }
        let cat = node
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("skill")
            .to_string();
        *counts.entry(cat).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(cat, _)| cat)
}

fn trajectory_row(buckets: &[ChartBucket], width: usize, reveal: f64) -> Row {
    if buckets.is_empty() {
        return Vec::new();
    }
    let total: usize = buckets.iter().map(|b| b.total()).sum::<usize>().max(1);
    let visible = ((reveal * buckets.len() as f64).ceil() as usize).clamp(0, buckets.len());
    let mut acc = 0usize;
    let mut points: Vec<usize> = Vec::new();
    for bucket in &buckets[..visible] {
        acc += bucket.total();
        points.push(((acc as f64 / total as f64) * (width - 1) as f64).round() as usize);
    }
    let mut cells: Vec<char> = vec![' '; width];
    let mut last = 0usize;
    for &p in &points {
        let lo = last.min(p);
        let hi = last.max(p);
        for x in lo..=hi {
            if x < width && cells[x] == ' ' {
                cells[x] = '·';
            }
        }
        if p < width {
            cells[p] = '✦';
        }
        last = p;
    }
    let text: String = cells.into_iter().collect();
    vec![
        Run {
            text: "trajectory ".to_string(),
            style: STYLE_LABEL.to_string(),
            alpha: 0.55,
            hex: None,
        },
        Run {
            text,
            style: STYLE_SKILL.to_string(),
            alpha: 0.48,
            hex: None,
        },
    ]
}

/// One rendered timeline frame at `reveal` (0→1).
pub struct Frame {
    pub grid: Grid,
    pub date: String,
    pub reveal: f64,
    pub visible: usize,
    pub labels: Vec<Value>,
}

/// Render one timeline frame at `reveal` (0→1).
///
/// Date rows with proportional skill/memory bars colored by the day's
/// dominant category, numbered markers tied to label rows, and a
/// cumulative trajectory sparkline underneath.
pub fn render_graph(payload: &Value, cols: usize, rows: usize, reveal: f64) -> Frame {
    let reveal = clamp(reveal, 0.0, 1.0);
    let cols = cols.max(44);
    let rows = rows.max(14);
    let nodes: Vec<Value> = payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if nodes.is_empty() {
        return Frame {
            grid: vec![vec![Run {
                text: "no learning yet — keep using the agent and it maps out here".to_string(),
                style: STYLE_DIM.to_string(),
                alpha: 0.7,
                hex: None,
            }]],
            date: String::new(),
            reveal,
            visible: 0,
            labels: Vec::new(),
        };
    }

    let rec = compute_recency(&nodes);
    let cmap = category_color_map(payload);
    let buckets = build_chart_buckets(&nodes, &rec, (rows.saturating_sub(3)).max(4));
    let n_buckets = buckets.len();
    let visible_bucket_count = ((reveal * n_buckets as f64).ceil() as usize).clamp(0, n_buckets);
    let max_total = buckets.iter().map(|b| b.total()).max().unwrap_or(1).max(1);
    let label_w = buckets
        .iter()
        .map(|b| b.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(9);
    let bar_w = (cols.saturating_sub(label_w).saturating_sub(16)).max(14);

    let mut grid: Grid = Vec::new();
    let mut labels: Vec<Value> = Vec::new();
    let mut visible = 0usize;
    for (i, bucket) in buckets.iter().enumerate() {
        if i >= visible_bucket_count {
            grid.push(Vec::new());
            continue;
        }
        visible += bucket.total();
        let ink = recency_ink(bucket.rec);
        let bar_len = if bucket.total() > 0 {
            (((bucket.total() as f64 / max_total as f64) * bar_w as f64).round() as usize).max(1)
        } else {
            0
        };
        let mut skill_len = if bucket.total() > 0 {
            ((bucket.skills as f64 / bucket.total() as f64) * bar_len as f64).round() as usize
        } else {
            0
        };
        if bucket.skills > 0 && skill_len == 0 {
            skill_len = 1;
        }
        let mut memory_len = bar_len.saturating_sub(skill_len);
        if bucket.memories > 0 && memory_len == 0 && bar_len > 1 {
            memory_len = 1;
            skill_len = bar_len - 1;
        }

        let node = bucket_label_node(bucket);
        let mut marker = String::new();
        if let Some(node) = node {
            if labels.len() < 6 {
                marker = LABEL_KEYS[labels.len()].to_string();
                let style = if node.get("kind").and_then(|v| v.as_str()) == Some("memory") {
                    STYLE_MEMORY
                } else {
                    STYLE_SKILL
                };
                labels.push(serde_json::json!({
                    "key": marker,
                    "glyph": if node.get("kind").and_then(|v| v.as_str()) == Some("memory") { MEMORY_GLYPH } else { SKILL_GLYPH },
                    "label": node_label(node),
                    "meta": node_meta(node),
                    "style": style,
                    "alpha": (ink * 1000.0).round() / 1000.0,
                }));
            }
        }

        let cat = bucket_category(bucket);
        let cat_hex = cat.as_ref().and_then(|c| cmap.get(c)).cloned();

        let mut row: Row = vec![
            Run {
                text: format!("{:>width$} ", bucket.label, width = label_w),
                style: STYLE_LABEL.to_string(),
                alpha: ink,
                hex: None,
            },
            Run {
                text: "│ ".to_string(),
                style: STYLE_DIM.to_string(),
                alpha: 0.55,
                hex: None,
            },
        ];
        if !marker.is_empty() {
            row.push(Run {
                text: marker,
                style: STYLE_LABEL.to_string(),
                alpha: 0.95,
                hex: None,
            });
        } else if bucket.total() > 0 {
            let head_hex = if bucket.skills > 0 {
                cat_hex.clone()
            } else {
                None
            };
            row.push(Run {
                text: if bucket.skills > 0 {
                    "✦".to_string()
                } else {
                    "◆".to_string()
                },
                style: (if bucket.skills > 0 {
                    STYLE_SKILL
                } else {
                    STYLE_MEMORY
                })
                .to_string(),
                alpha: ink,
                hex: head_hex,
            });
        }
        if skill_len > 0 {
            // Bar colored by the day's dominant category — a learning heatmap.
            row.push(Run {
                text: "━".repeat(skill_len),
                style: STYLE_SKILL.to_string(),
                alpha: ink,
                hex: cat_hex.clone(),
            });
        }
        if memory_len > 0 {
            let mem_trail = if memory_len == 1 {
                "◆".to_string()
            } else {
                format!("◆{}◆", "━".repeat(memory_len - 2))
            };
            row.push(Run {
                text: mem_trail,
                style: STYLE_MEMORY.to_string(),
                alpha: ink.max(0.65),
                hex: None,
            });
        }
        if bar_len < bar_w {
            row.push(Run {
                text: " ".repeat(bar_w - bar_len),
                style: STYLE_BG.to_string(),
                alpha: 1.0,
                hex: None,
            });
        }
        row.push(Run {
            text: "  ".to_string(),
            style: STYLE_BG.to_string(),
            alpha: 1.0,
            hex: None,
        });
        row.push(Run {
            text: bucket.skills.to_string(),
            style: STYLE_SKILL.to_string(),
            alpha: ink.max(0.72),
            hex: None,
        });
        if bucket.memories > 0 {
            row.push(Run {
                text: "+".to_string(),
                style: STYLE_DIM.to_string(),
                alpha: 0.6,
                hex: None,
            });
            row.push(Run {
                text: bucket.memories.to_string(),
                style: STYLE_MEMORY.to_string(),
                alpha: ink.max(0.72),
                hex: None,
            });
        }
        if i == visible_bucket_count - 1 {
            row.push(Run {
                text: "  ◀ now".to_string(),
                style: STYLE_LABEL.to_string(),
                alpha: 0.9,
                hex: None,
            });
        } else if bucket.total() == max_total && max_total > 1 {
            row.push(Run {
                text: "  ☄ peak".to_string(),
                style: STYLE_LABEL.to_string(),
                alpha: 0.75,
                hex: None,
            });
        }
        grid.push(row);
    }

    // Cumulative learning trajectory underneath the rows.
    let mut tail: Row = vec![Run {
        text: " ".repeat(label_w + 2),
        style: STYLE_BG.to_string(),
        alpha: 1.0,
        hex: None,
    }];
    tail.extend(trajectory_row(
        &buckets,
        (cols.saturating_sub(label_w).saturating_sub(13)).max(12),
        reveal,
    ));
    grid.push(tail);

    Frame {
        grid,
        date: format_date(date_at(&rec, reveal)),
        reveal,
        visible,
        labels,
    }
}

// ---------------------------------------------------------------------------
// Trimmings
// ---------------------------------------------------------------------------

pub fn build_legend(payload: &Value) -> Vec<Value> {
    let nodes = payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let skills = nodes
        .iter()
        .filter(|n| n.get("kind").and_then(|v| v.as_str()) != Some("memory"))
        .count();
    let memories = nodes.len() - skills;
    vec![
        serde_json::json!({"glyph": SKILL_GLYPH, "style": STYLE_SKILL, "label": format!("skills ({})", skills)}),
        serde_json::json!({"glyph": MEMORY_GLYPH, "style": STYLE_MEMORY, "label": format!("memories ({})", memories)}),
    ]
}

pub fn axis_labels(payload: &Value) -> (String, String) {
    let nodes = payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rec = compute_recency(&nodes);
    if !rec.timed {
        return ("oldest".to_string(), "now".to_string());
    }
    (format_date(rec.min_ts), format_date(rec.max_ts))
}

fn category_counts(payload: &Value) -> Vec<(String, i64)> {
    let clusters: Vec<(String, i64)> = payload
        .get("clusters")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let cat = c.get("category")?.as_str()?.to_string();
            if cat == "memory" {
                return None;
            }
            Some((cat, c.get("count").and_then(|v| v.as_i64()).unwrap_or(0)))
        })
        .collect();
    if !clusters.is_empty() {
        return clusters;
    }
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let empty: Vec<Value> = Vec::new();
    for node in payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
    {
        if node.get("kind").and_then(|v| v.as_str()) == Some("memory") {
            continue;
        }
        let cat = node
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("skill")
            .to_string();
        *counts.entry(cat).or_insert(0) += 1;
    }
    let mut out: Vec<(String, i64)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// Deterministic, evenly-spread hue per skill category (theme-independent).
pub fn category_color_map(payload: &Value) -> std::collections::HashMap<String, String> {
    let clusters = category_counts(payload);
    // Golden-angle hue spacing so adjacent categories never collide.
    clusters
        .iter()
        .enumerate()
        .map(|(i, (cat, _))| {
            let hue = (i as f64 * 137.508) % 360.0;
            (cat.clone(), rgb_to_hex_hsl(hue))
        })
        .collect()
}

fn rgb_to_hex_hsl(hue: f64) -> String {
    let (r, g, b) = hsl_to_rgb(hue, 0.55, 0.62);
    format!("#{:02X}{:02X}{:02X}", r, g, b)
}

pub fn category_legend(payload: &Value, limit: usize) -> Vec<Value> {
    let cmap = category_color_map(payload);
    let cats = category_counts(payload);
    let shown = &cats[..cats.len().min(limit)];
    let hidden = cats.len().saturating_sub(shown.len());
    let mut out: Vec<Value> = shown
        .iter()
        .map(|(cat, count)| {
            serde_json::json!({"glyph": "●", "color": cmap.get(cat).cloned().unwrap_or_default(), "label": format!("{} ({})", cat, count)})
        })
        .collect();
    if hidden > 0 {
        out.push(serde_json::json!({"glyph": "·", "color": "", "label": format!("+{}", hidden)}));
    }
    out
}

fn peak_day(payload: &Value) -> Option<String> {
    let mut counts: std::collections::HashMap<(i32, u32, u32), usize> =
        std::collections::HashMap::new();
    let mut reps: std::collections::HashMap<(i32, u32, u32), f64> =
        std::collections::HashMap::new();
    let empty: Vec<Value> = Vec::new();
    for node in payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
    {
        let Some(ts) = node.get("timestamp").and_then(to_ts) else {
            continue;
        };
        let key = period_key(ts, "day");
        *counts.entry(key).or_insert(0) += 1;
        reps.insert(key, ts);
    }
    if counts.is_empty() {
        return None;
    }
    let (best, count) = counts.into_iter().max_by(|a, b| a.1.cmp(&b.1))?;
    Some(format!(
        "busiest day {} · {} learned",
        period_label(reps[&best], "day"),
        count
    ))
}

pub fn build_summary(payload: &Value) -> Vec<String> {
    let stats = payload
        .get("stats")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let mut lines = Vec::new();
    let learned = stats
        .get("learned_skills")
        .or_else(|| stats.get("nodes"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mem = stats
        .get("memory_nodes")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let edges = stats
        .get("related_edges")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    lines.push(format!(
        "{} learned skills · {} memories · {} skill links",
        learned, mem, edges
    ));
    let mut extra = Vec::new();
    if let Some(mse) = stats
        .get("memory_skill_edges")
        .and_then(|v| v.as_i64())
        .filter(|n| *n > 0)
    {
        extra.push(format!("{} memory↔skill links", mse));
    }
    if let Some(peak) = peak_day(payload) {
        extra.push(peak);
    }
    if !extra.is_empty() {
        lines.push(extra.join(" · "));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_payload() -> Value {
        json!({
            "nodes": [
                {"id": "skill-a", "label": "skill-a", "kind": "skill", "timestamp": 1_700_000_000, "category": "devops", "useCount": 3, "state": "active", "pinned": false},
                {"id": "skill-b", "label": "skill-b", "kind": "skill", "timestamp": 1_710_000_000, "category": "devops", "useCount": 0, "state": "active", "pinned": true},
                {"id": "memory:memory:0", "label": "a memory", "kind": "memory", "memorySource": "memory", "timestamp": 1_720_000_000, "category": "memory", "useCount": 0, "state": "active", "pinned": false}
            ],
            "edges": [],
            "clusters": [
                {"category": "devops", "count": 2},
                {"category": "memory", "count": 1}
            ],
            "memory": [{"source": "memory", "title": "a memory", "body": "body text", "timestamp": 1_720_000_000}],
            "stats": {"learned_skills": 2, "memory_nodes": 1, "related_edges": 0, "memory_skill_edges": 0, "nodes": 3}
        })
    }

    #[test]
    fn color_roundtrip() {
        assert_eq!(hex_to_rgb("#FFD700"), (255, 215, 0));
        assert_eq!(hex_to_rgb("abc"), (170, 187, 204));
        assert_eq!(hex_to_rgb("zz"), (255, 215, 0)); // fallback gold
        assert_eq!(rgb_to_hex((255.0, 215.0, 0.0)), "#FFD700");
        let mixed = mix_rgb((0, 0, 0), (255, 255, 255), 0.5);
        assert_eq!(mixed, (128.0, 128.0, 128.0));
    }

    #[test]
    fn palette_keys() {
        let palette = derive_palette("#FFD700", true);
        for key in ["primary", "memory", "skill", "label", "dim", "bg"] {
            assert!(palette.contains_key(key), "missing {}", key);
        }
        // Fade toward bg lowers brightness.
        let faded = fade(&palette, Some("#FFFFFF"), 0.5).unwrap();
        assert_ne!(faded, "#FFFFFF");
        assert_eq!(
            fade(&palette, Some("#FFFFFF"), 1.0),
            Some("#FFFFFF".to_string())
        );
        assert_eq!(fade(&palette, None, 0.5), None);
    }

    #[test]
    fn recency_gradient() {
        assert!(recency_ink(0.0) < recency_ink(0.5));
        assert!(recency_ink(0.5) < recency_ink(1.0));
        assert!((recency_ink(0.0) - AGE_OLD_INK).abs() < 1e-9);
        assert!((recency_ink(1.0) - AGE_NEW_INK).abs() < 1e-9);
    }

    #[test]
    fn recency_computation_timed_and_ordinal() {
        let payload = sample_payload();
        let nodes = payload["nodes"].as_array().unwrap().clone();
        let rec = compute_recency(&nodes);
        assert!(rec.timed);
        // Oldest node sits at LEAD_IN; newest at 1.0.
        let lo = rec.rec["skill-a"];
        let hi = rec.rec["memory:memory:0"];
        assert!((lo - LEAD_IN).abs() < 1e-9);
        assert!((hi - 1.0).abs() < 1e-9);

        // No timestamps → ordinal fallback, still timed=false.
        let nodes = vec![
            json!({"id": "x", "timestamp": null}),
            json!({"id": "y", "timestamp": null}),
        ];
        let rec = compute_recency(&nodes);
        assert!(!rec.timed);
        assert!(rec.rec["x"] < rec.rec["y"]);
    }

    #[test]
    fn render_graph_frame() {
        let payload = sample_payload();
        let frame = render_graph(&payload, 90, 20, 1.0);
        assert_eq!(frame.visible, 3);
        assert!(!frame.date.is_empty());
        assert!(!frame.grid.is_empty());
        // Buckets rows + trajectory row.
        let non_empty = frame.grid.iter().filter(|r| !r.is_empty()).count();
        assert!(non_empty >= 2);
        // At most 6 charted labels.
        assert!(frame.labels.len() <= 6);

        // Reveal 0 shows nothing.
        let frame0 = render_graph(&payload, 90, 20, 0.0);
        assert_eq!(frame0.visible, 0);
    }

    #[test]
    fn render_graph_empty_payload() {
        let frame = render_graph(&json!({"nodes": []}), 80, 16, 1.0);
        assert_eq!(frame.visible, 0);
        assert!(frame.grid[0][0].text.contains("no learning yet"));
    }

    #[test]
    fn trimmings() {
        let payload = sample_payload();
        let legend = build_legend(&payload);
        assert_eq!(legend.len(), 2);
        assert!(legend[0]["label"].as_str().unwrap().contains("skills (2)"));
        assert!(legend[1]["label"]
            .as_str()
            .unwrap()
            .contains("memories (1)"));

        let (start, end) = axis_labels(&payload);
        assert!(start.contains("2023"));
        assert!(end.contains("2024"));

        let cmap = category_color_map(&payload);
        assert!(cmap.contains_key("devops"));
        assert!(!cmap.contains_key("memory")); // memory excluded from categories

        let cats = category_legend(&payload, 4);
        assert!(!cats.is_empty());

        let summary = build_summary(&payload);
        assert!(summary[0].contains("2 learned skills"));
        assert!(summary[0].contains("1 memories"));
        // Busiest day line appears when timestamps exist.
        assert!(summary.iter().any(|l| l.contains("busiest day")));
    }

    #[test]
    fn format_date_shape() {
        assert_eq!(format_date(None), "unknown");
        let out = format_date(Some(1_700_000_000.0));
        assert!(out.contains("2023"), "got {}", out);
    }
}
