//! `/init` — build the prompt that generates or updates a project
//! AGENTS.md (P665).
//!
//! Port of hermes `init_command.py` (v2026.8.3). Hermes already *loads*
//! AGENTS.md as project context; `/init` bootstraps one by handing the
//! live agent ONE guidance-laden prompt instructing it to inspect the
//! project with its own read-only tools and write a concise AGENTS.md
//! (or merge-update an existing one). No engine, no model-tool
//! footprint: every surface (CLI REPL, gateway) calls
//! [`build_init_prompt`] and feeds the result to the agent as a normal
//! user turn — the same prompt-injection pattern as `/blueprint`.

/// The quality bar, embedded in every prompt so the generated file
/// reads like a maintainer wrote it — concrete and command-exact, not
/// generic advice (hermes `_QUALITY_BAR`).
const QUALITY_BAR: &str =
    "Quality bar for the file you write (this is what separates a useful AGENTS.md\n\
from noise):\n\
- CONCISE: target under 100 lines. Agents load this file every session — every\n\
  line costs context. No essays, no marketing prose, no filler.\n\
- Commands must be EXACT invocations you verified from the repo (package.json\n\
  scripts, Makefile targets, pyproject/tox/CI config, existing docs). Write\n\
  `npm run test:unit` or `scripts/run_tests.sh tests/foo`, never \"run the\n\
  tests\". NEVER invent a command you didn't see evidence for.\n\
- No generic advice. \"Write tests for new code\" and \"follow best practices\"\n\
  are banned — if a line would be true of any repo, cut it.\n\
- Conventions must be OBSERVED, not assumed: naming patterns, module layout,\n\
  error-handling style, commit-message format — only what the code actually\n\
  shows.\n\
- Include pitfalls that would genuinely trip up a newcomer or an agent\n\
  (required env vars, generated files not to hand-edit, slow test suites,\n\
  ports already in use), if you found any. Skip the section if you found none.\n\
- Markdown structure: a short title + one-paragraph overview, then focused\n\
  sections (e.g. \"Dev environment\", \"Build & test\", \"Conventions\",\n\
  \"Pitfalls\"). Flat and scannable — no deep nesting.";

/// Build the agent prompt for an `/init` request (hermes
/// `build_init_prompt`).
///
/// * `cwd` — the project directory to scan and write `AGENTS.md` into.
/// * `existing_file` — current `AGENTS.md` content when one exists;
///   switches the prompt to update-and-merge discipline.
/// * `extra` — free text after `/init` (emphasis or notes to honor).
pub fn build_init_prompt(
    cwd: &std::path::Path,
    existing_file: Option<&str>,
    extra: &str,
) -> String {
    let extra = extra.trim();
    let cwd_display = cwd.to_string_lossy();
    let cwd_trimmed = cwd_display.trim_end_matches('/');

    let mut parts: Vec<String> = vec![format!(
        "[/init] The user wants you to {} for the project at: {cwd_display}\n",
        if existing_file.is_some() {
            "UPDATE the existing AGENTS.md project-instructions file"
        } else {
            "generate an AGENTS.md project-instructions file"
        }
    )];
    parts.push(
        "AGENTS.md is the instruction file coding agents (ulnclaw included) \
         load as project context every session. It should teach an agent how \
         to work in THIS repo: what the project is, how to set up, the exact \
         build/test/lint commands, the conventions the code actually follows, \
         and the pitfalls that waste time.\n"
            .to_string(),
    );
    parts.push(format!(
        "Do this:\n\
         1. Inspect the project with your read-only tools (`read_file`, \
         `search_files`) — start with manifests and toolchain files \
         (package.json, pyproject.toml, Cargo.toml, go.mod, Makefile, \
         CI workflow configs, lockfiles), then the directory layout, existing \
         README/docs, and test/lint configuration. Learn the real commands, \
         don't guess them.\n\
         2. Write the file to {cwd_trimmed}/AGENTS.md with `write_file`{}\n\
         3. Confirm to the user the exact path you wrote and summarize in one \
         or two lines what the file covers.\n",
        if existing_file.is_some() {
            " — but this is an UPDATE, so follow the merge discipline below."
        } else {
            "."
        }
    ));

    if let Some(existing) = existing_file {
        parts.push(format!(
            "MERGE DISCIPLINE — an AGENTS.md already exists (its current \
             content is below). Do NOT overwrite or regenerate it from \
             scratch. Preserve the user's existing content — their wording, \
             their sections, their rules — and merge in only what is missing \
             or verifiably stale (e.g. a command that no longer exists in the \
             repo). When existing content conflicts with what you observed, \
             prefer minimal surgical edits over rewrites, and keep the \
             user's intent. The result must still meet the quality bar.\n\n\
             CURRENT AGENTS.md CONTENT:\n\
             <<<EXISTING_AGENTS_MD\n\
             {existing}\n\
             EXISTING_AGENTS_MD\n"
        ));
    }

    parts.push(QUALITY_BAR.to_string());

    if !extra.is_empty() {
        parts.push(format!(
            "\nUSER NOTES — honor these while authoring (they override the \
             defaults above where they conflict):\n{extra}"
        ));
    }

    parts.join("\n")
}

/// Convenience wrapper used by the dispatch surfaces (hermes
/// `build_init_prompt_for_cwd`): resolves `cwd` (defaults to the process
/// working directory), reads an existing `AGENTS.md` there if present,
/// and returns the full prompt.
pub fn build_init_prompt_for_cwd(cwd: Option<&std::path::Path>, extra: &str) -> String {
    let resolved = match cwd {
        Some(path) => std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    };
    let agents_path = resolved.join("AGENTS.md");
    let existing = std::fs::read_to_string(&agents_path).ok();
    build_init_prompt(&resolved, existing.as_deref(), extra)
}

/// True when the built prompt is the update variant (dispatch surfaces
/// echo a different status line).
pub fn is_update_prompt(prompt: &str) -> bool {
    prompt.contains("UPDATE the existing AGENTS.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_prompt_targets_cwd_and_quality_bar() {
        let cwd = std::path::Path::new("/tmp/demo-project");
        let prompt = build_init_prompt(cwd, None, "");
        assert!(prompt.contains("generate an AGENTS.md"), "{prompt}");
        assert!(
            prompt.contains("for the project at: /tmp/demo-project"),
            "{prompt}"
        );
        assert!(
            prompt.contains("/tmp/demo-project/AGENTS.md with `write_file`."),
            "{prompt}"
        );
        assert!(prompt.contains("Quality bar"), "{prompt}");
        assert!(!prompt.contains("MERGE DISCIPLINE"), "{prompt}");
        assert!(!is_update_prompt(&prompt));
    }

    #[test]
    fn update_prompt_embeds_existing_content_and_merge_rules() {
        let cwd = std::path::Path::new("/tmp/demo-project");
        let prompt = build_init_prompt(cwd, Some("# My rules\nBe nice."), "focus on tests");
        assert!(prompt.contains("UPDATE the existing AGENTS.md"), "{prompt}");
        assert!(prompt.contains("MERGE DISCIPLINE"), "{prompt}");
        assert!(prompt.contains("<<<EXISTING_AGENTS_MD"), "{prompt}");
        assert!(prompt.contains("# My rules\nBe nice."), "{prompt}");
        assert!(prompt.contains("USER NOTES"), "{prompt}");
        assert!(prompt.contains("focus on tests"), "{prompt}");
        assert!(is_update_prompt(&prompt));
    }

    #[test]
    fn for_cwd_reads_existing_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# existing\n").unwrap();
        let prompt = build_init_prompt_for_cwd(Some(dir.path()), "");
        assert!(is_update_prompt(&prompt));
        assert!(prompt.contains("# existing"), "{prompt}");

        let dir2 = tempfile::tempdir().unwrap();
        let prompt = build_init_prompt_for_cwd(Some(dir2.path()), "");
        assert!(!is_update_prompt(&prompt));
    }
}
