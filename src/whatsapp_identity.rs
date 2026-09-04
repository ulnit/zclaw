//! Shared helpers for canonicalising WhatsApp sender identity — port
//! of hermes `gateway/whatsapp_identity.py`.
//!
//! WhatsApp's bridge can surface the same human under two different
//! JID shapes within a single conversation:
//! - LID form: `999999999999999@lid`
//! - Phone form: `15551234567@s.whatsapp.net`
//!
//! Both the authorisation path and the session-key path need to
//! collapse these aliases to a single stable identity; this module is
//! the single source of truth so the two paths never drift apart.

use std::collections::HashSet;
use std::path::Path;

/// WhatsApp JIDs are ASCII alphanumerics with `@`, `.`, `+`, `-`
/// separators. Anything else (path separators, traversal segments) is
/// rejected before it can reach a `lid-mapping-{id}.json` filename.
fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '+' | '-'))
}

/// Strip WhatsApp JID/LID syntax down to the stable identifier:
/// `"60123456789@s.whatsapp.net"`, `"60123456789:47@s.whatsapp.net"`,
/// `"60123456789@lid"`, or a bare `"+601****6789"` / `"60123456789"`
/// all reduce to `"60123456789"`.
pub fn normalize_whatsapp_identifier(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let no_plus = trimmed.strip_prefix('+').unwrap_or(trimmed);
    let no_device = no_plus.split(':').next().unwrap_or(no_plus);
    no_device.split('@').next().unwrap_or(no_device).to_string()
}

/// True when the value is "just a phone number": optional leading `+`
/// then digits and the usual human separators (spaces, dots, dashes,
/// parens). Anything carrying an `@` is a fully-qualified JID and
/// passes through untouched.
fn is_bare_phone(value: &str) -> bool {
    let mut chars = value.chars();
    let mut seen_digit = false;
    for (idx, c) in chars.by_ref().enumerate() {
        if c == '+' {
            if idx != 0 {
                return false;
            }
        } else if c.is_ascii_digit() {
            seen_digit = true;
        } else if !matches!(c, ' ' | '(' | ')' | '.' | '-') {
            return false;
        }
    }
    seen_digit
}

/// Normalize an *outbound* WhatsApp target to a bridge-safe JID.
///
/// Baileys' `jidDecode` crashes on a bare phone number — it expects a
/// fully-qualified JID such as `50766715226@s.whatsapp.net`. This is
/// the inverse of [`normalize_whatsapp_identifier`]: instead of
/// stripping a JID down for comparison, it *builds* the JID a send
/// must use.
///
/// - `"+50766715226"` / `"50766715226"` → `"50766715226@s.whatsapp.net"`
/// - `"50766715226@s.whatsapp.net"` → unchanged
/// - `"group-id@g.us"` / `"130631430344750@lid"` → unchanged
/// - `"user:device@s.whatsapp.net"` → `"user@s.whatsapp.net"`
/// - anything unrecognizable → unchanged so the bridge surfaces a
///   meaningful error rather than a mangled target.
pub fn to_whatsapp_jid(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut normalized = trimmed.to_string();
    if normalized.contains(':') && normalized.contains('@') {
        if let Some(at) = normalized.find('@') {
            let prefix = &normalized[..at];
            let domain = &normalized[at + 1..];
            normalized = format!("{}@{}", prefix.split(':').next().unwrap_or(prefix), domain);
        }
    }
    if normalized.contains('@') {
        return normalized;
    }
    if is_bare_phone(&normalized) {
        let digits: String = normalized.chars().filter(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return format!("{digits}@s.whatsapp.net");
        }
    }
    normalized
}

/// Group/broadcast/newsroom JIDs are chat addresses, not user
/// identities — canonicalisation must leave them alone.
pub fn is_user_identifier(value: &str) -> bool {
    let lower = value.to_lowercase();
    !(lower.ends_with("@g.us") || lower.ends_with("@broadcast") || lower.ends_with("@newsletter"))
}

/// Resolve WhatsApp phone/LID aliases via the bridge's
/// `<session_dir>/lid-mapping-<id>.json` (+ `_reverse`) files, walking
/// the mapping transitively. The result always includes the normalized
/// input itself; empty when the input normalizes to empty.
pub fn expand_whatsapp_aliases(identifier: &str, session_dir: &Path) -> HashSet<String> {
    let normalized = normalize_whatsapp_identifier(identifier);
    let mut resolved: HashSet<String> = HashSet::new();
    if normalized.is_empty() {
        return resolved;
    }
    let mut queue = vec![normalized];
    while let Some(current) = queue.pop() {
        if current.is_empty() || resolved.contains(&current) {
            continue;
        }
        if !is_safe_identifier(&current) {
            continue;
        }
        resolved.insert(current.clone());
        for suffix in ["", "_reverse"] {
            let mapping_path = session_dir.join(format!("lid-mapping-{current}{suffix}.json"));
            if !mapping_path.exists() {
                continue;
            }
            let mapped = std::fs::read_to_string(&mapping_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<String>(&raw).ok())
                .map(|mapped| normalize_whatsapp_identifier(&mapped));
            if let Some(mapped) = mapped {
                if !mapped.is_empty() && !resolved.contains(&mapped) {
                    queue.push(mapped);
                }
            }
        }
    }
    resolved
}

/// Stable WhatsApp sender identity across phone-JID/LID variants:
/// walk the bridge's lid-mapping files transitively and pick the
/// shortest (numeric-preferred) alias. Session keys, authorisation
/// and pairing should all use this so their bookkeeping lines up even
/// when the bridge reshuffles aliases.
///
/// Returns `""` when the input normalizes to empty; without mapping
/// files (fresh bridge install) the normalized input itself.
pub fn canonical_whatsapp_identifier(identifier: &str, session_dir: &Path) -> String {
    let normalized = normalize_whatsapp_identifier(identifier);
    if normalized.is_empty() {
        return String::new();
    }
    expand_whatsapp_aliases(&normalized, session_dir)
        .into_iter()
        .min_by_key(|candidate| (candidate.len(), candidate.clone()))
        .unwrap_or(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_jid_lid_device_plus() {
        assert_eq!(
            normalize_whatsapp_identifier("60123456789@s.whatsapp.net"),
            "60123456789"
        );
        assert_eq!(
            normalize_whatsapp_identifier("60123456789:47@s.whatsapp.net"),
            "60123456789"
        );
        assert_eq!(normalize_whatsapp_identifier("99999@lid"), "99999");
        assert_eq!(normalize_whatsapp_identifier("+60123456789"), "60123456789");
        assert_eq!(normalize_whatsapp_identifier("60123456789"), "60123456789");
        assert_eq!(normalize_whatsapp_identifier("  "), "");
    }

    #[test]
    fn to_jid_builds_and_preserves() {
        assert_eq!(
            to_whatsapp_jid("+50766715226"),
            "50766715226@s.whatsapp.net"
        );
        assert_eq!(to_whatsapp_jid("50766715226"), "50766715226@s.whatsapp.net");
        assert_eq!(
            to_whatsapp_jid("(507) 667-15226"),
            "50766715226@s.whatsapp.net"
        );
        assert_eq!(
            to_whatsapp_jid("50766715226@s.whatsapp.net"),
            "50766715226@s.whatsapp.net"
        );
        assert_eq!(to_whatsapp_jid("group-id@g.us"), "group-id@g.us");
        assert_eq!(
            to_whatsapp_jid("130631430344750@lid"),
            "130631430344750@lid"
        );
        assert_eq!(
            to_whatsapp_jid("50766715226:3@s.whatsapp.net"),
            "50766715226@s.whatsapp.net"
        );
        assert_eq!(to_whatsapp_jid(""), "");
        // Unrecognizable text passes through for a meaningful bridge error.
        assert_eq!(to_whatsapp_jid("not-a-phone"), "not-a-phone");
    }

    #[test]
    fn user_identifier_detection() {
        assert!(is_user_identifier("60123456789@s.whatsapp.net"));
        assert!(is_user_identifier("999@lid"));
        assert!(!is_user_identifier("123-456@g.us"));
        assert!(!is_user_identifier("status@broadcast"));
        assert!(!is_user_identifier("channel@newsletter"));
    }

    #[test]
    fn aliases_walk_mappings_transitively() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        // phone → lid, lid → phone (reverse), plus a second hop lid2.
        std::fs::write(dir.join("lid-mapping-111.json"), "\"222@lid\"").unwrap();
        std::fs::write(dir.join("lid-mapping-222_reverse.json"), "\"333\"").unwrap();
        std::fs::write(dir.join("lid-mapping-333.json"), "\"111\"").unwrap();

        let aliases = expand_whatsapp_aliases("111@s.whatsapp.net", dir);
        assert!(aliases.contains("111"), "{aliases:?}");
        assert!(aliases.contains("222"), "{aliases:?}");
        assert!(aliases.contains("333"), "{aliases:?}");

        // Canonical picks the shortest alias.
        assert_eq!(canonical_whatsapp_identifier("222@lid", dir), "111");
        // No mapping files → normalized input.
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            canonical_whatsapp_identifier("444@s.whatsapp.net", empty.path()),
            "444"
        );
        assert_eq!(canonical_whatsapp_identifier("", empty.path()), "");
    }

    #[test]
    fn unsafe_identifiers_never_reach_the_filesystem() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        // The identifier charset admits dots (".." produces the
        // harmless fixed-prefix file `lid-mapping-...json`); the
        // defense is against path SEPARATORS sneaking into the
        // mapping filename.
        let aliases = expand_whatsapp_aliases("..", dir);
        assert!(aliases.iter().all(|a| !a.contains('/')), "{aliases:?}");
        let aliases = expand_whatsapp_aliases("../../etc/passwd", dir);
        assert!(aliases.iter().all(|a| is_safe_identifier(a)), "{aliases:?}");
        assert!(!aliases.iter().any(|a| a.contains('/')), "{aliases:?}");
    }
}
