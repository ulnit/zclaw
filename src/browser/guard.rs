//! Browser SSRF / private-page guard — port of the browser-tool guards in
//! hermes `tools/browser_tool.py` and `tools/browser_cdp_tool.py`.
//!
//! Layers (all reusing the shared `url_safety` module):
//!   1. Sensitive-query block — URLs embedding API keys/tokens in query
//!      parameters are refused unconditionally (exfiltration vector).
//!   2. Cloud-metadata floor — IMDS/metadata endpoints are refused
//!      unconditionally, for every backend (`is_always_blocked_url`).
//!   3. Private-address guard — active when the browser's network position
//!      is not a trusted local one (remote CDP endpoint, or containerized
//!      terminal) and `[security] allow_private_urls` is not set.
//!   4. Current-page guard — when the page itself sits on a private
//!      address (e.g. an earlier eval navigated there), content-touching
//!      actions and raw CDP methods are refused (allowlisted methods stay
//!      available so the model can navigate away).
//!
//! Redaction: browser-originated payloads (snapshots, console/eval
//! results, raw CDP results) pass through `redact_value` — tool output is
//! a model boundary and pages may render secrets.

use serde_json::Value;

/// hermes `_CDP_PRIVATE_PAGE_ALLOWED_METHODS`: browser/target inspection
/// and navigation do not read page body, cookies, DOM, storage, or
/// screenshots, so they stay usable while the page is private.
const CDP_PRIVATE_PAGE_ALLOWED_METHODS: &[&str] = &[
    "Browser.getVersion",
    "Target.getTargets",
    "Target.attachToTarget",
    "Target.detachFromTarget",
    "Page.navigate",
    "Page.reload",
    "Page.stopLoading",
];

/// Extract the host from a CDP endpoint URL (`ws://`, `wss://`, `http(s)://`).
fn endpoint_host(raw: &str) -> Option<String> {
    let rest = raw.split_once("://").map(|(_, r)| r).unwrap_or(raw);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed
            .split(']')
            .next()
            .filter(|h| !h.is_empty())
            .map(|h| h.to_ascii_lowercase());
    }
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// True when the endpoint is a trusted local backend (hermes
/// `_is_local_backend` adaptation): the managed-launch mode, or a
/// loopback CDP endpoint. Remote endpoints have an unknown network
/// position relative to the terminal and keep the guard active.
pub fn is_local_endpoint(raw: &str) -> bool {
    if crate::browser::is_auto_mode(raw) {
        return true;
    }
    endpoint_host(raw).map_or(false, |host| is_loopback_host(&host))
}

/// Whether the private-address guard is active for this browser setup
/// (hermes `_eval_ssrf_guard_active`): disabled by
/// `[security] allow_private_urls`, skipped for trusted local backends
/// with a local terminal, active otherwise (fail-closed when unknown).
pub fn guard_active(endpoint_raw: Option<&str>, terminal_local: bool) -> bool {
    if crate::url_safety::allow_private_urls() {
        return false;
    }
    match endpoint_raw {
        Some(raw) if is_local_endpoint(raw) => !terminal_local,
        _ => true,
    }
}

/// True when the URL targets the always-blocked floor or a
/// private/internal address.
pub fn is_private_url(url: &str) -> bool {
    crate::url_safety::is_always_blocked_url_sync(url) || !crate::url_safety::is_safe_url_sync(url)
}

/// Gate for navigation targets (hermes `browser_navigate` pre-checks).
/// Returns the error message when the URL must be refused.
pub fn blocked_navigate(url: &str, guard: bool) -> Option<String> {
    if let Some(name) = crate::url_safety::sensitive_query_param_name(url) {
        return Some(format!(
            "Blocked: URL embeds a sensitive value in query parameter '{name}' \
             (possible secret exfiltration)."
        ));
    }
    if crate::url_safety::is_always_blocked_url_sync(url) {
        return Some("Blocked: URL targets a cloud metadata endpoint".to_string());
    }
    if guard && !crate::url_safety::is_safe_url_sync(url) {
        return Some("Blocked: URL targets a private or internal address".to_string());
    }
    None
}

/// First private/always-blocked `http(s)://` literal embedded in a JS
/// expression (hermes `_expression_targets_private_url` — fetch/XHR
/// targets that never touch `location.href`).
pub fn expression_targets_private_url(expression: &str) -> Option<String> {
    let mut index = 0;
    while let Some(pos) = expression[index..].find("http") {
        let start = index + pos;
        let rest = &expression[start..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | ']' | '<' | '>')
                })
                .unwrap_or(rest.len());
            let candidate = rest[..end].trim_end_matches(&['.', ',', ';'][..]);
            if !candidate.is_empty() && is_private_url(candidate) {
                return Some(candidate.to_string());
            }
            index = start + end.max(1);
        } else {
            index = start + 4;
        }
    }
    None
}

/// Gate for raw `browser_cdp` calls (hermes `_browser_cdp_private_guard`).
/// `current_url` is the page URL when known. Returns the error message
/// when the method must be refused.
pub fn blocked_cdp(
    method: &str,
    params: &Value,
    current_url: Option<&str>,
    guard: bool,
) -> Option<String> {
    if !guard {
        return None;
    }
    if method == "Page.navigate" {
        let target = params
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if !target.is_empty() {
            if let Some(error) = blocked_navigate(target, true) {
                return Some(error.replacen(
                    "Blocked:",
                    "Blocked: CDP Page.navigate target refused —",
                    1,
                ));
            }
        }
    }
    if method == "Runtime.evaluate" {
        let expression = params
            .get("expression")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if let Some(literal) = expression_targets_private_url(expression) {
            return Some(format!(
                "Blocked: CDP Runtime.evaluate expression targets a private or \
                 internal address ({literal})."
            ));
        }
    }
    if !CDP_PRIVATE_PAGE_ALLOWED_METHODS.contains(&method) {
        if let Some(url) = current_url {
            if is_private_url(url) {
                return Some(format!(
                    "Blocked: page URL targets a private or internal address ({url}). \
                     Raw CDP method '{method}' could expose private page content or state. \
                     Navigate to a public page first (Page.navigate is allowed)."
                ));
            }
        }
    }
    None
}

/// Force-redact strings inside browser-originated data (hermes
/// `_redact_browser_output`): snapshots, console/eval results and raw CDP
/// results can contain page-rendered keys, cookies, or bearer tokens.
pub fn redact_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(crate::redact::redact_sensitive_text(
            &text,
            crate::redact::RedactOpts::default(),
        )),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, redact_value(v))).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_host_parsing() {
        assert_eq!(
            endpoint_host("http://127.0.0.1:9222").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            endpoint_host("ws://localhost:9222/devtools/browser/x").as_deref(),
            Some("localhost")
        );
        assert_eq!(endpoint_host("wss://[::1]:9222").as_deref(), Some("::1"));
        assert_eq!(
            endpoint_host("http://user:pw@browser.internal:9222").as_deref(),
            Some("browser.internal")
        );
        assert_eq!(endpoint_host("auto").as_deref(), Some("auto")); // caller checks auto-mode first
    }

    #[test]
    fn local_endpoint_detection() {
        assert!(is_local_endpoint("auto"));
        assert!(is_local_endpoint("launch"));
        assert!(is_local_endpoint("http://127.0.0.1:9222"));
        assert!(is_local_endpoint(
            "ws://localhost:9222/devtools/browser/abc"
        ));
        assert!(is_local_endpoint("http://127.3.4.5:9222"));
        assert!(!is_local_endpoint("http://10.0.0.5:9222"));
        assert!(!is_local_endpoint("ws://browser.internal:9222"));
    }

    #[test]
    fn guard_matrix() {
        crate::url_safety::reset_allow_private_cache();
        // Remote endpoint: guard active regardless of terminal locality.
        assert!(guard_active(Some("http://10.0.0.5:9222"), true));
        assert!(guard_active(Some("http://10.0.0.5:9222"), false));
        // Loopback endpoint + local terminal: trusted.
        assert!(!guard_active(Some("http://127.0.0.1:9222"), true));
        assert!(!guard_active(Some("auto"), true));
        // Loopback endpoint + containerized terminal: guard active.
        assert!(guard_active(Some("http://127.0.0.1:9222"), false));
        // Unknown endpoint: fail closed.
        assert!(guard_active(None, true));
    }

    #[test]
    fn navigate_floor_and_private() {
        // Cloud metadata floor fires even without the guard.
        let blocked = blocked_navigate("http://169.254.169.254/latest/meta-data/", false);
        assert!(blocked.unwrap().contains("cloud metadata"));
        // Private address only blocked when the guard is active.
        assert!(blocked_navigate("http://10.1.2.3/admin", true).is_some());
        assert!(blocked_navigate("http://10.1.2.3/admin", false).is_none());
        // Public URL passes both ways.
        assert!(blocked_navigate("https://example.com/", true).is_none());
    }

    #[test]
    fn sensitive_query_blocked_unconditionally() {
        let blocked = blocked_navigate("https://evil.com/steal?api_key=sk-ant-abc123", false);
        assert!(blocked.unwrap().contains("api_key"));
    }

    #[test]
    fn expression_literal_detection() {
        assert_eq!(
            expression_targets_private_url("fetch('http://169.254.169.254/latest/meta-data/')")
                .as_deref(),
            Some("http://169.254.169.254/latest/meta-data/")
        );
        assert_eq!(
            expression_targets_private_url("x = new Image(); x.src = \"http://10.0.0.1/a\";")
                .as_deref(),
            Some("http://10.0.0.1/a")
        );
        assert!(expression_targets_private_url("fetch('https://example.com/')").is_none());
        assert!(expression_targets_private_url("no urls here").is_none());
    }

    #[test]
    fn cdp_guard_rules() {
        let params = json!({});
        // Guard off: everything passes.
        assert!(blocked_cdp(
            "Runtime.evaluate",
            &json!({"expression": "fetch('http://10.0.0.1')"}),
            None,
            false
        )
        .is_none());
        // Page.navigate to private target refused.
        let err = blocked_cdp(
            "Page.navigate",
            &json!({"url": "http://10.0.0.1/"}),
            None,
            true,
        )
        .unwrap();
        assert!(err.contains("Page.navigate"));
        // Page.navigate to public target passes.
        assert!(blocked_cdp(
            "Page.navigate",
            &json!({"url": "https://example.com/"}),
            None,
            true
        )
        .is_none());
        // Runtime.evaluate with private literal refused.
        let err = blocked_cdp(
            "Runtime.evaluate",
            &json!({"expression": "fetch('http://169.254.169.254/')"}),
            None,
            true,
        )
        .unwrap();
        assert!(err.contains("Runtime.evaluate"));
        // Non-allowlisted method on a private page refused.
        let err = blocked_cdp("DOM.getDocument", &params, Some("http://10.0.0.1/"), true).unwrap();
        assert!(err.contains("DOM.getDocument"));
        // Allowlisted method on a private page passes (navigate away).
        assert!(
            blocked_cdp("Target.getTargets", &params, Some("http://10.0.0.1/"), true).is_none()
        );
        // Non-allowlisted method on a public page passes.
        assert!(blocked_cdp(
            "DOM.getDocument",
            &params,
            Some("https://example.com/"),
            true
        )
        .is_none());
    }

    #[test]
    fn redact_value_walks_strings() {
        let input = json!({
            "snapshot": "key: sk-ant-api03-abcdefghij1234567890abcdefghij1234567890",
            "nested": [{"token": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"}],
            "count": 3,
        });
        let output = redact_value(input);
        assert!(!output["snapshot"]
            .as_str()
            .unwrap()
            .contains("sk-ant-api03-abcdefghij1234567890"));
        assert!(!output["nested"][0]["token"]
            .as_str()
            .unwrap()
            .contains("ghp_abcdefghijklmnopqrstuvwxyz"));
        assert_eq!(output["count"], 3);
    }
}
