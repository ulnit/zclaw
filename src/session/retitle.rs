//! Skill-scaffold title recovery (hermes `agent/skill_commands.py`
//! describe/extract surface, backing `sessions retitle-skills`).
//!
//! A `/skill` invocation expands into a scaffolded user turn that embeds
//! the whole skill body, so auto-generated titles describe the skill
//! rather than the request. This module re-derives what the user typed
//! (`"/work — fix the title leak"`) from the scaffolded content.

/// Hermes `_SKILL_INVOCATION_PREFIX` — every scaffolded turn starts here.
pub const SKILL_INVOCATION_PREFIX: &str = "[IMPORTANT: The user has invoked the ";
/// Hermes `_BUNDLE_MARKER`.
pub const BUNDLE_MARKER: &str = " skill bundle,";
/// Hermes `_SINGLE_SKILL_MARKER`.
pub const SINGLE_SKILL_MARKER: &str = "The full skill content is loaded below.]";
/// Hermes `_SINGLE_SKILL_INSTRUCTION`.
pub const SINGLE_SKILL_INSTRUCTION: &str =
    "The user has provided the following instruction alongside the skill invocation: ";
/// Hermes `_BUNDLE_USER_INSTRUCTION`.
pub const BUNDLE_USER_INSTRUCTION: &str = "\nUser instruction: ";
/// Hermes `_BUNDLE_FIRST_SKILL_BLOCK`.
pub const BUNDLE_FIRST_SKILL_BLOCK: &str = "\n\n[Loaded as part of the ";
/// Hermes `_RUNTIME_NOTE`.
pub const RUNTIME_NOTE: &str = "\n\n[Runtime note:";
/// Hermes `SKILL_EXCERPT_JOINT` — head+tail excerpt join marker.
pub const SKILL_EXCERPT_JOINT: &str = "\u{001e}";
/// SQL LIKE pattern matching a skill-expanded turn (hermes
/// `SKILL_SCAFFOLD_SQL_LIKE`).
pub const SKILL_SCAFFOLD_SQL_LIKE: &str = "[IMPORTANT: The user has invoked the %";

/// Render a slash-skill-expanded turn the way the user typed it (hermes
/// `describe_skill_invocation`). Returns `"/work — fix the title leak"`,
/// `"/work"` for a bare invocation, or `None` when the content is not
/// skill scaffolding.
pub fn describe_skill_invocation(content: &str) -> Option<String> {
    if !content.starts_with(SKILL_INVOCATION_PREFIX) {
        return None;
    }
    let rest = &content[SKILL_INVOCATION_PREFIX.len()..];
    let name = rest
        .strip_prefix('"')
        .and_then(|r| r.find('"').map(|end| r[..end].trim().to_string()))
        .unwrap_or_default();
    let label = if name.starts_with('/') {
        name.clone()
    } else {
        format!("/{name}")
    };
    match extract_user_instruction(content) {
        Some(instruction) if !instruction.is_empty() => {
            // An excerpted message (head + tail joined by SKILL_EXCERPT_JOINT)
            // can put the joint inside the matched span — keep only the side
            // the instruction marker was found on.
            let instruction = instruction.split(SKILL_EXCERPT_JOINT).next().unwrap_or("");
            let instruction = instruction.split_whitespace().collect::<Vec<_>>().join(" ");
            if instruction.is_empty() {
                Some(label)
            } else if name.is_empty() {
                Some(instruction)
            } else {
                Some(format!("{label} — {instruction}"))
            }
        }
        _ => {
            if name.is_empty() {
                None
            } else {
                Some(label)
            }
        }
    }
}

/// Recover the user's instruction from a slash-skill-expanded turn
/// (hermes `extract_user_instruction_from_skill_message`). `None` means
/// scaffolding with no instruction (a bare `/skill` invocation); ordinary
/// non-scaffold content is returned unchanged.
pub fn extract_user_instruction(content: &str) -> Option<String> {
    if !content.starts_with(SKILL_INVOCATION_PREFIX) {
        return Some(content.to_string());
    }
    if content.contains(BUNDLE_MARKER) {
        return extract_bundle_user_instruction(content);
    }
    if content.contains(SINGLE_SKILL_MARKER) {
        return extract_single_skill_user_instruction(content);
    }
    None
}

/// Bundle format puts the user instruction before the loaded skills, so
/// the FIRST occurrence is the user-provided one (hermes
/// `_extract_bundle_user_instruction`).
fn extract_bundle_user_instruction(message: &str) -> Option<String> {
    let marker_idx = message.find(BUNDLE_USER_INSTRUCTION)?;
    let mut instruction = &message[marker_idx + BUNDLE_USER_INSTRUCTION.len()..];
    if let Some(first_skill_idx) = instruction.find(BUNDLE_FIRST_SKILL_BLOCK) {
        instruction = &instruction[..first_skill_idx];
    }
    let instruction = instruction.trim();
    if instruction.is_empty() {
        None
    } else {
        Some(instruction.to_string())
    }
}

/// Single-skill format appends the user instruction after the skill body,
/// so the LAST occurrence is the user-provided one (the body may quote
/// this text) — hermes `_extract_single_skill_user_instruction`.
fn extract_single_skill_user_instruction(message: &str) -> Option<String> {
    let marker_idx = message.rfind(SINGLE_SKILL_INSTRUCTION)?;
    let mut instruction = &message[marker_idx + SINGLE_SKILL_INSTRUCTION.len()..];
    if let Some(runtime_idx) = instruction.find(RUNTIME_NOTE) {
        instruction = &instruction[..runtime_idx];
    }
    let instruction = instruction.trim();
    if instruction.is_empty() {
        None
    } else {
        Some(instruction.to_string())
    }
}

/// Reject a title candidate that isn't a title at all (hermes
/// `_is_titlelike`): an auxiliary model occasionally answers the prompt
/// instead of titling it; replacing a serviceable title with command
/// output would make things worse.
pub fn is_titlelike(candidate: &str) -> bool {
    candidate
        .chars()
        .next()
        .map(|c| c.is_alphanumeric())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_message(name: &str, instruction: &str) -> String {
        let mut parts = vec![format!(
            "[IMPORTANT: The user has invoked the \"{name}\" skill bundle, loading 1 skills \
             together. Treat every skill below as active guidance for this turn.]\n\nBundle: {name}\nSkills loaded: work"
        )];
        if !instruction.is_empty() {
            parts.push(format!("\nUser instruction: {instruction}"));
        }
        parts.push(format!(
            "\n\n[Loaded as part of the \"{name}\" skill bundle.]\n\n## Skill: work\n<body>"
        ));
        parts.join("")
    }

    #[test]
    fn describe_bundle_with_instruction() {
        let content = bundle_message("work", "fix  the   title leak");
        assert_eq!(
            describe_skill_invocation(&content).as_deref(),
            Some("/work — fix the title leak")
        );
    }

    #[test]
    fn describe_bundle_bare_and_slash_names() {
        let content = bundle_message("work", "");
        assert_eq!(
            describe_skill_invocation(&content).as_deref(),
            Some("/work")
        );
        // Bundle headers already carry their typed "/a /b" keys.
        let content = bundle_message("/a /b", "do it");
        assert_eq!(
            describe_skill_invocation(&content).as_deref(),
            Some("/a /b — do it")
        );
    }

    #[test]
    fn describe_single_skill_format() {
        let content = format!(
            "{SKILL_INVOCATION_PREFIX}\"deploy\" skill. {SINGLE_SKILL_MARKER}\n\n<body>\n\n\
             {SINGLE_SKILL_INSTRUCTION}ship the hotfix{RUNTIME_NOTE} ignore me"
        );
        assert_eq!(
            describe_skill_invocation(&content).as_deref(),
            Some("/deploy — ship the hotfix")
        );
        // Bare single-skill invocation.
        let content =
            format!("{SKILL_INVOCATION_PREFIX}\"deploy\" skill. {SINGLE_SKILL_MARKER}\n\n<body>");
        assert_eq!(
            describe_skill_invocation(&content).as_deref(),
            Some("/deploy")
        );
    }

    #[test]
    fn non_scaffold_passes_through_or_none() {
        assert_eq!(describe_skill_invocation("plain question"), None);
        assert_eq!(
            extract_user_instruction("plain question").as_deref(),
            Some("plain question")
        );
        // Scaffold without any known marker -> no instruction.
        let content = format!("{SKILL_INVOCATION_PREFIX}\"x\" skill. unknown format");
        assert_eq!(extract_user_instruction(&content), None);
        assert_eq!(describe_skill_invocation(&content).as_deref(), Some("/x"));
    }

    #[test]
    fn excerpt_joint_keeps_marker_side() {
        let instruction = "left side\u{001e}right side";
        let content = format!(
            "{SKILL_INVOCATION_PREFIX}\"work\" skill bundle, loading 1 skills.\nUser instruction: {instruction}\n\n[Loaded as part of the \"work\" skill bundle.]"
        );
        assert_eq!(
            describe_skill_invocation(&content).as_deref(),
            Some("/work — left side")
        );
    }

    #[test]
    fn titlelike_gate() {
        assert!(is_titlelike("Fix the parser"));
        assert!(is_titlelike("修复标题"));
        assert!(!is_titlelike("$ df -h /"));
        assert!(!is_titlelike(""));
        assert!(!is_titlelike(" leading space"));
    }
}
