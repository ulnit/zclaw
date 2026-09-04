//! `ulnclaw memory` — inspect/reset persistent memory (hermes `cmd_memory`
//! port: default shows status, `reset [all|memory|user]` erases the stores).

use std::path::{Path, PathBuf};

/// Which store(s) a reset targets (hermes `target` arg: all|memory|user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetTarget {
    All,
    Memory,
    User,
}

impl ResetTarget {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "memory" => Some(Self::Memory),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// One memory file plus its live stats.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    /// File name (`MEMORY.md` / `USER.md`).
    pub file: &'static str,
    /// Human description (`agent notes` / `user profile`).
    pub desc: &'static str,
    pub path: PathBuf,
    pub exists: bool,
    pub bytes: u64,
    pub entries: usize,
}

pub fn memory_dir(home: &Path) -> PathBuf {
    home.join("memory")
}

/// Both stores in hermes order (MEMORY.md first, then USER.md).
pub fn memory_files(home: &Path) -> Vec<MemoryFile> {
    let dir = memory_dir(home);
    [("MEMORY.md", "agent notes"), ("USER.md", "user profile")]
        .into_iter()
        .map(|(file, desc)| {
            let path = dir.join(file);
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let exists = path.exists();
            let entries = if exists {
                crate::tools::builtin::memory::read_entries(&content).len()
            } else {
                0
            };
            MemoryFile {
                file,
                desc,
                path,
                exists,
                bytes: content.len() as u64,
                entries,
            }
        })
        .collect()
}

/// Status rendering for the bare `ulnclaw memory` invocation.
pub fn memory_status(home: &Path) -> String {
    let mut out = format!("Memory store: {}\n", memory_dir(home).display());
    for file in memory_files(home) {
        if file.exists {
            out.push_str(&format!(
                "  {:<10} ({})  {} entries, {} bytes\n",
                file.file, file.desc, file.entries, file.bytes
            ));
        } else {
            out.push_str(&format!(
                "  {:<10} ({})  not created yet\n",
                file.file, file.desc
            ));
        }
    }
    out.push_str(
        "Entries are injected into every turn's system prompt; manage them via the memory tool \
         or edit the files directly.\n",
    );
    out
}

/// Existing files a reset would erase (hermes `existing` filter).
pub fn reset_candidates(home: &Path, target: ResetTarget) -> Vec<MemoryFile> {
    memory_files(home)
        .into_iter()
        .filter(|file| file.exists)
        .filter(|file| match target {
            ResetTarget::All => true,
            ResetTarget::Memory => file.file == "MEMORY.md",
            ResetTarget::User => file.file == "USER.md",
        })
        .collect()
}

/// Hermes-style confirmation banner listing what will be erased.
pub fn reset_preview(candidates: &[MemoryFile]) -> String {
    let mut out = String::from("\n  This will permanently erase the following memory files:\n");
    for file in candidates {
        out.push_str(&format!(
            "    ◆ {} ({}) — {} bytes\n",
            file.file, file.desc, file.bytes
        ));
    }
    out
}

/// CLI entry: `ulnclaw memory [reset [all|memory|user]] [--yes]`.
pub fn handle_memory_command(home: &Path, args: &[String], yes: bool) -> Result<(), String> {
    if args.is_empty() {
        print!("{}", memory_status(home));
        return Ok(());
    }
    if args[0] != "reset" {
        return Err("usage: ulnclaw memory [reset [all|memory|user]] [--yes]".to_string());
    }
    let target = match args.get(1).map(String::as_str) {
        None | Some("all") => ResetTarget::All,
        Some(raw) => ResetTarget::parse(raw)
            .ok_or_else(|| format!("unknown reset target '{raw}' (expected all|memory|user)"))?,
    };

    let candidates = reset_candidates(home, target);
    if candidates.is_empty() {
        println!(
            "\n  Nothing to reset — no memory files found in {}/\n",
            memory_dir(home).display()
        );
        return Ok(());
    }

    print!("{}", reset_preview(&candidates));
    if !yes {
        use std::io::Write;
        print!("\n  Type 'yes' to confirm: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| format!("failed to read confirmation: {e}"))?;
        if answer.trim().to_ascii_lowercase() != "yes" {
            println!("  Cancelled.");
            return Ok(());
        }
    }

    for file in &candidates {
        std::fs::remove_file(&file.path)
            .map_err(|e| format!("✗ failed to delete {}: {e}", file.path.display()))?;
        println!("  ✓ Deleted {} ({})", file.file, file.desc);
    }
    println!("\n  Memory reset complete. New sessions will start with a blank slate.");
    println!("  Files were in: {}/\n", memory_dir(home).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_home(dir: &Path) {
        let memory = dir.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("MEMORY.md"), "- env note\n- convention note\n").unwrap();
        std::fs::write(memory.join("USER.md"), "- prefers concise answers\n").unwrap();
    }

    #[test]
    fn reset_target_parse() {
        assert_eq!(ResetTarget::parse("all"), Some(ResetTarget::All));
        assert_eq!(ResetTarget::parse("MEMORY"), Some(ResetTarget::Memory));
        assert_eq!(ResetTarget::parse("User"), Some(ResetTarget::User));
        assert_eq!(ResetTarget::parse("bogus"), None);
    }

    #[test]
    fn memory_files_reports_stats() {
        let dir = tempfile::tempdir().unwrap();
        seed_home(dir.path());
        let files = memory_files(dir.path());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file, "MEMORY.md");
        assert!(files[0].exists);
        assert_eq!(files[0].entries, 2);
        assert_eq!(files[1].file, "USER.md");
        assert_eq!(files[1].entries, 1);
    }

    #[test]
    fn status_handles_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = memory_status(dir.path());
        assert!(out.contains("Memory store:"), "{out}");
        assert!(
            out.contains("MEMORY.md  (agent notes)  not created yet"),
            "{out}"
        );
        assert!(
            out.contains("USER.md    (user profile)  not created yet"),
            "{out}"
        );
    }

    #[test]
    fn status_shows_entry_counts() {
        let dir = tempfile::tempdir().unwrap();
        seed_home(dir.path());
        let out = memory_status(dir.path());
        assert!(out.contains("MEMORY.md  (agent notes)  2 entries"), "{out}");
        assert!(
            out.contains("USER.md    (user profile)  1 entries"),
            "{out}"
        );
    }

    #[test]
    fn reset_candidates_filter_by_target() {
        let dir = tempfile::tempdir().unwrap();
        seed_home(dir.path());
        assert_eq!(reset_candidates(dir.path(), ResetTarget::All).len(), 2);
        let memory_only = reset_candidates(dir.path(), ResetTarget::Memory);
        assert_eq!(memory_only.len(), 1);
        assert_eq!(memory_only[0].file, "MEMORY.md");
        let user_only = reset_candidates(dir.path(), ResetTarget::User);
        assert_eq!(user_only.len(), 1);
        assert_eq!(user_only[0].file, "USER.md");
    }

    #[test]
    fn reset_with_yes_deletes_files() {
        let dir = tempfile::tempdir().unwrap();
        seed_home(dir.path());
        handle_memory_command(dir.path(), &["reset".to_string()], true).unwrap();
        assert!(!dir.path().join("memory/MEMORY.md").exists());
        assert!(!dir.path().join("memory/USER.md").exists());
    }

    #[test]
    fn reset_single_target_keeps_other() {
        let dir = tempfile::tempdir().unwrap();
        seed_home(dir.path());
        handle_memory_command(dir.path(), &["reset".to_string(), "user".to_string()], true)
            .unwrap();
        assert!(dir.path().join("memory/MEMORY.md").exists());
        assert!(!dir.path().join("memory/USER.md").exists());
    }

    #[test]
    fn reset_nothing_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        // No memory dir at all — must succeed with the "nothing to reset" path.
        handle_memory_command(dir.path(), &["reset".to_string()], true).unwrap();
    }

    #[test]
    fn rejects_unknown_action_and_target() {
        let dir = tempfile::tempdir().unwrap();
        let err = handle_memory_command(dir.path(), &["wipe".to_string()], true).unwrap_err();
        assert!(err.contains("usage:"), "{err}");
        let err = handle_memory_command(
            dir.path(),
            &["reset".to_string(), "bogus".to_string()],
            true,
        )
        .unwrap_err();
        assert!(err.contains("unknown reset target"), "{err}");
    }
}
