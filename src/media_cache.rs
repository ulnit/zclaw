//! Media cache — content-addressed storage for platform downloads
//! (hermes `gateway/platforms/media_cache.py` port).
//!
//! Downloaded attachments land under `<home>/media-cache/` named by the
//! SHA-256 of their bytes, so identical files dedupe across platforms and
//! restarts. The mime→extension table is hermes' `DEFAULT_MIME_TO_EXT`
//! (common-in-the-wild spellings, e.g. `audio/ogg` → `.ogg`).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Hermes `DEFAULT_MIME_TO_EXT` union table.
const MIME_TO_EXT: &[(&str, &str)] = &[
    // images
    ("image/jpeg", ".jpg"),
    ("image/png", ".png"),
    ("image/gif", ".gif"),
    ("image/webp", ".webp"),
    // audio
    ("audio/ogg", ".ogg"),
    ("audio/x-opus+ogg", ".ogg"),
    ("audio/opus", ".ogg"),
    ("audio/mpeg", ".mp3"),
    ("audio/mp3", ".mp3"),
    ("audio/wav", ".wav"),
    ("audio/mp4", ".m4a"),
    ("audio/x-m4a", ".m4a"),
    ("audio/aac", ".aac"),
    // video / documents
    ("video/mp4", ".mp4"),
    ("video/webm", ".webm"),
    ("video/quicktime", ".mov"),
    ("application/pdf", ".pdf"),
    ("application/zip", ".zip"),
    ("text/plain", ".txt"),
    ("application/json", ".json"),
];

/// Lowercase + strip `; charset=...` parameters (hermes `_normalize_mime`).
pub fn normalize_mime(mime: &str) -> String {
    mime.split(';').next().unwrap_or("").trim().to_lowercase()
}

/// Resolve a mime type to a file extension (including the dot), falling
/// back to `fallback` when unknown (hermes `ext_for_mime`).
pub fn ext_for_mime(mime: &str, fallback: &str) -> String {
    let primary = normalize_mime(mime);
    if primary.is_empty() {
        return fallback.to_string();
    }
    for (candidate, ext) in MIME_TO_EXT {
        if *candidate == primary {
            return ext.to_string();
        }
    }
    // Guess from the subtype: image/foo → .foo covers the long tail.
    if let Some((kind, sub)) = primary.split_once('/') {
        if matches!(kind, "image" | "audio" | "video")
            && sub.chars().all(|c| c.is_ascii_alphanumeric())
            && sub.len() <= 5
        {
            return format!(".{sub}");
        }
    }
    fallback.to_string()
}

/// Hermes `DEFAULT_EXT_TO_MIME` canonical inverse.
const EXT_TO_MIME: &[(&str, &str)] = &[
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".png", "image/png"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".ogg", "audio/ogg"),
    (".mp3", "audio/mpeg"),
    (".wav", "audio/wav"),
    (".m4a", "audio/mp4"),
    (".aac", "audio/aac"),
    (".mp4", "video/mp4"),
    (".pdf", "application/pdf"),
    (".zip", "application/zip"),
];

/// Guess the mime type from a file extension (hermes `mime_for_ext`);
/// falls back to `application/octet-stream`.
pub fn mime_for_ext(path: &Path) -> String {
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    for (candidate, mime) in EXT_TO_MIME {
        if *candidate == ext {
            return mime.to_string();
        }
    }
    "application/octet-stream".to_string()
}

/// Media kind for routing decisions (hermes image/audio/document classes).
pub fn media_kind(mime: &str) -> &'static str {
    let primary = normalize_mime(mime);
    if primary.starts_with("image/") {
        "image"
    } else if primary.starts_with("audio/") {
        "audio"
    } else if primary.starts_with("video/") {
        "video"
    } else {
        "document"
    }
}

fn cache_dir(home: &Path) -> PathBuf {
    let dir = home.join("media-cache");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Cache downloaded media bytes; returns the local path (hermes
/// `cache_media_bytes`). Content-addressed: re-caching identical bytes is
/// a no-op. `filename_hint` is preserved for documents via a sidecar-free
/// scheme — the extension resolves from mime, documents keep their hint in
/// the returned file name when it has none of its own.
pub fn cache_media_bytes(
    home: &Path,
    data: &[u8],
    mime: &str,
    filename_hint: &str,
) -> std::io::Result<PathBuf> {
    let hash = format!("{:x}", Sha256::digest(data));
    let kind = media_kind(mime);
    let ext = match kind {
        "image" => ext_for_mime(mime, ".jpg"),
        "audio" => ext_for_mime(mime, ".ogg"),
        "video" => ext_for_mime(mime, ".mp4"),
        _ => ext_for_mime(mime, ".bin"),
    };
    let dir = cache_dir(home);
    let file_name = if kind == "document" && !filename_hint.is_empty() {
        // Documents keep a recognizable name, disambiguated by hash prefix.
        let stem = sanitize_file_name(filename_hint);
        format!("{stem}.{}{ext}", &hash[..8])
    } else {
        format!("{hash}{ext}")
    };
    let path = dir.join(file_name);
    if !path.exists() {
        std::fs::write(&path, data)?;
    }
    Ok(path)
}

/// Strip path separators and control chars from an inbound filename.
fn sanitize_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        out = "file".to_string();
    }
    // Drop any leading dots so names can't hide or climb directories.
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() {
        out = "file".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-media-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn mime_normalization_and_ext_lookup() {
        assert_eq!(normalize_mime("Image/JPEG; charset=binary"), "image/jpeg");
        assert_eq!(ext_for_mime("image/jpeg", ".bin"), ".jpg");
        assert_eq!(ext_for_mime("audio/ogg", ".bin"), ".ogg");
        assert_eq!(ext_for_mime("audio/x-opus+ogg", ".bin"), ".ogg");
        assert_eq!(ext_for_mime("application/pdf", ".bin"), ".pdf");
        // Long-tail subtype guess.
        assert_eq!(ext_for_mime("image/heic", ".bin"), ".heic");
        // Unknown falls back.
        assert_eq!(ext_for_mime("application/x-mystery", ".bin"), ".bin");
        assert_eq!(ext_for_mime("", ".bin"), ".bin");
    }

    #[test]
    fn media_kind_routing() {
        assert_eq!(media_kind("image/png"), "image");
        assert_eq!(media_kind("audio/mpeg"), "audio");
        assert_eq!(media_kind("video/mp4"), "video");
        assert_eq!(media_kind("application/pdf"), "document");
        assert_eq!(media_kind(""), "document");
    }

    #[test]
    fn cache_is_content_addressed_and_dedupes() {
        let home = temp_home("dedupe");
        let data = b"hello media";
        let first = cache_media_bytes(&home, data, "image/png", "").unwrap();
        let second = cache_media_bytes(&home, data, "image/png", "").unwrap();
        assert_eq!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".png"));
        assert_eq!(std::fs::read(&first).unwrap(), data);
        // Different bytes → different path.
        let other = cache_media_bytes(&home, b"other", "image/png", "").unwrap();
        assert_ne!(first, other);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn documents_keep_sanitized_names() {
        let home = temp_home("docnames");
        let path =
            cache_media_bytes(&home, b"%PDF-1.4", "application/pdf", "../../evil .pdf").unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with(".pdf"));
        assert!(!name.contains(".."));
        assert!(!name.contains(' '));
        assert!(name.starts_with("evil.pdf."));
        std::fs::remove_dir_all(&home).ok();
    }
}
