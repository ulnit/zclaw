//! Binary file extensions — port of hermes' `tools/binary_extensions.py`.
//!
//! Files that can't be meaningfully compared as text and are often large;
//! text-based operations (read_file) skip them by extension with no I/O.
//! Ported from free-code src/constants/files.ts. `.pdf` is deliberately
//! excluded — text-based, agents may want to inspect it.

use std::collections::HashSet;
use std::sync::OnceLock;

pub fn binary_extensions() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        HashSet::from([
            // Images
            ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".webp", ".tiff", ".tif",
            // Videos
            ".mp4", ".mov", ".avi", ".mkv", ".webm", ".wmv", ".flv", ".m4v", ".mpeg", ".mpg",
            // Audio
            ".mp3", ".wav", ".ogg", ".flac", ".aac", ".m4a", ".wma", ".aiff", ".opus",
            // Archives
            ".zip", ".tar", ".gz", ".bz2", ".7z", ".rar", ".xz", ".z", ".tgz", ".iso",
            // Executables/binaries
            ".exe", ".dll", ".so", ".dylib", ".bin", ".o", ".a", ".obj", ".lib", ".app", ".msi",
            ".deb", ".rpm", // Documents (exclude .pdf — text-based)
            ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".ods", ".odp",
            // Fonts
            ".ttf", ".otf", ".woff", ".woff2", ".eot", // Bytecode / VM artifacts
            ".pyc", ".pyo", ".class", ".jar", ".war", ".ear", ".node", ".wasm", ".rlib",
            // Database files
            ".sqlite", ".sqlite3", ".db", ".mdb", ".idx", // Design / 3D
            ".psd", ".ai", ".eps", ".sketch", ".fig", ".xd", ".blend", ".3ds", ".max",
            // Flash
            ".swf", ".fla", // Lock/profiling data
            ".lockb", ".dat", ".data",
        ])
    })
}

/// True when `path` ends in a known binary extension. Pure string check,
/// no I/O.
pub fn has_binary_extension(path: &str) -> bool {
    let Some(dot) = path.rfind('.') else {
        return false;
    };
    binary_extensions().contains(path[dot..].to_ascii_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_binaries() {
        for p in [
            "logo.PNG",
            "/a/b/video.mp4",
            "archive.tar.gz",
            "app.exe",
            "model.wasm",
            "store.sqlite3",
            "font.WOFF2",
        ] {
            assert!(has_binary_extension(p), "should be binary: {p}");
        }
    }

    #[test]
    fn passes_text_files() {
        for p in [
            "main.rs",
            "notes.md",
            "doc.pdf",
            "script.py",
            "data.json",
            "noext",
            ".env",
        ] {
            assert!(!has_binary_extension(p), "should NOT be binary: {p}");
        }
    }

    #[test]
    fn extension_not_in_middle() {
        // rfind semantics: only the final extension counts.
        assert!(!has_binary_extension("archive.tar.readme"));
        assert!(has_binary_extension("archive.readme.tar"));
    }
}
