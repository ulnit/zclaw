//! `ulnclaw mtls` — outbound mTLS identity management and diagnostics.
//!
//! Four subcommands, all backed by [`crate::outbound_tls`]:
//!
//! * `status` — resolve and print the egress identity policy (fail-closed: a
//!   partially configured policy is reported as an error, never as "off").
//! * `probe <url>` — two live handshakes (with/without the client cert) telling
//!   us whether the endpoint enforces mTLS and whether our identity is accepted.
//! * `csr` — generate a private key + CSR to hand to the gateway's CA.
//! * `keygen` — generate a self-signed keypair for staging/test.
//!
//! Secrets handling: private keys are written `0600` and never echoed. Only the
//! leaf certificate fingerprint (SHA-256) is printed, which is derived from
//! public material.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::outbound_tls::{self, OutboundTls};

/// Default probe timeout in seconds.
pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 10;

/// Default validity for `keygen` (days).
pub const DEFAULT_KEYGEN_DAYS: u32 = 825;

/// `ulnclaw mtls status`
pub fn handle_status(json: bool) -> Result<(), String> {
    let resolved = OutboundTls::from_env();
    match resolved {
        Ok(None) => {
            if json {
                let payload = serde_json::json!({
                    "enabled": false,
                    "config": outbound_tls::config_env_vars(),
                    "wired_paths": outbound_tls::WIRED_PATHS,
                    "hint": "将所有四个必填变量写入环境变量或 ~/.ulnclaw/.env 后生效",
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
                );
            } else {
                println!("出站 mTLS: 未启用（未发现 {}）", outbound_tls::ENV_CERT);
                println!();
                println!("配置项（环境变量或 ~/.ulnclaw/.env）：");
                for (name, note) in outbound_tls::config_env_vars() {
                    println!("  {name:<32} {note}");
                }
                println!();
                println!("已接线到策略的出站路径：");
                for path in outbound_tls::WIRED_PATHS {
                    println!("  {path}");
                }
                println!();
                println!("生成客户端凭据：ulnclaw mtls csr --cn <name> --out ~/.ulnclaw/mtls");
                println!("校验端点：      ulnclaw mtls probe https://<gateway-host>/healthz");
            }
            Ok(())
        }
        Ok(Some(policy)) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&policy.status_json()).map_err(|e| e.to_string())?
                );
            } else {
                println!("{}", policy.status_text());
            }
            Ok(())
        }
        // Fail-closed: a half-configured identity is an error, not "disabled".
        // Silently dropping the client certificate would send unauthenticated
        // traffic to an mTLS-fronted gateway.
        Err(err) => Err(format!(
            "出站 mTLS 配置无效，已按 fail-closed 拒绝继续使用该身份：{err}"
        )),
    }
}

/// `ulnclaw mtls probe <url>`
pub async fn handle_probe(url: String, timeout_secs: Option<u64>, json: bool) -> Result<(), String> {
    let timeout = Duration::from_secs(
        timeout_secs
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_PROBE_TIMEOUT_SECS),
    );
    let report = outbound_tls::probe(&url, timeout).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.to_json()).map_err(|e| e.to_string())?
        );
    } else {
        println!("{}", report.to_text());
    }
    // Exit non-zero when the identity is not usable, so probes can gate CI /
    // rollout steps.
    if !report.ok() {
        return Err(format!(
            "探测结论：{} —— {}",
            report.verdict.label(),
            report.verdict.remediation()
        ));
    }
    Ok(())
}

/// `ulnclaw mtls csr` — key + CSR for the gateway's CA to sign.
pub fn handle_csr(
    cn: String,
    sans: Vec<String>,
    out_dir: PathBuf,
    force: bool,
) -> Result<(), String> {
    let cn = validate_cn(&cn)?;
    let sans = validate_sans(&sans)?;
    let dir = prepare_out_dir(&out_dir)?;
    let key_path = dir.join("client.key");
    let csr_path = dir.join("client.csr");
    guard_overwrite(&[&key_path, &csr_path], force)?;

    let mut args: Vec<String> = vec![
        "req".into(),
        "-new".into(),
        "-newkey".into(),
        "ec".into(),
        "-pkeyopt".into(),
        "ec_paramgen_curve:prime256v1".into(),
        "-noenc".into(),
        "-keyout".into(),
        path_arg(&key_path),
        "-out".into(),
        path_arg(&csr_path),
        "-subj".into(),
        format!("/CN={cn}"),
    ];
    if !sans.is_empty() {
        args.push("-addext".into());
        args.push(format!("subjectAltName={}", sans.join(",")));
    }
    run_openssl(&args)?;
    restrict_key_permissions(&key_path)?;

    println!("已生成客户端私钥与 CSR（私钥权限 0600，内容不外显）：");
    println!("  私钥: {}", key_path.display());
    println!("  CSR : {}", csr_path.display());
    println!();
    println!("下一步：把 CSR 交给网关/控制面的 CA 签发，得到链证书后写入 {ENV_CERT}：", ENV_CERT = outbound_tls::ENV_CERT);
    println!("  cat signed-chain.pem > ~/.ulnclaw/mtls/client.crt");
    println!("  并用 `ulnclaw mtls status` 复核指纹（SHA-256）。");
    Ok(())
}

/// `ulnclaw mtls keygen` — self-signed keypair, for staging or local checks.
pub fn handle_keygen(
    cn: String,
    days: Option<u32>,
    sans: Vec<String>,
    out_dir: PathBuf,
    force: bool,
) -> Result<(), String> {
    let cn = validate_cn(&cn)?;
    let sans = validate_sans(&sans)?;
    let days = days.filter(|d| *d > 0).unwrap_or(DEFAULT_KEYGEN_DAYS);
    let dir = prepare_out_dir(&out_dir)?;
    let key_path = dir.join("client.key");
    let cert_path = dir.join("client.crt");
    guard_overwrite(&[&key_path, &cert_path], force)?;

    let args: Vec<String> = vec![
        "req".into(),
        "-x509".into(),
        "-newkey".into(),
        "ec".into(),
        "-pkeyopt".into(),
        "ec_paramgen_curve:prime256v1".into(),
        "-noenc".into(),
        "-keyout".into(),
        path_arg(&key_path),
        "-out".into(),
        path_arg(&cert_path),
        "-days".into(),
        days.to_string(),
        "-subj".into(),
        format!("/CN={cn}"),
        "-addext".into(),
        format!(
            "subjectAltName={}",
            if sans.is_empty() {
                format!("DNS:{cn}")
            } else {
                sans.join(",")
            }
        ),
    ];
    run_openssl(&args)?;
    restrict_key_permissions(&key_path)?;

    println!("已生成自签客户端身份（仅用于 staging / 自测；生产请用 `ulnclaw mtls csr`）：");
    println!("  证书: {}", cert_path.display());
    println!("  私钥: {} (0600)", key_path.display());
    println!("  有效期: {days} 天");
    println!();
    println!("接入示例（~/.ulnclaw/.env）：");
    println!("  {}=~/.../client.crt", outbound_tls::ENV_CERT);
    println!("  {}=~/.../client.key", outbound_tls::ENV_KEY);
    println!("  {}=<gateway-host>", outbound_tls::ENV_HOSTS);
    Ok(())
}

fn validate_cn(cn: &str) -> Result<String, String> {
    let trimmed = cn.trim();
    if trimmed.is_empty() {
        return Err("--cn 不能为空".to_string());
    }
    if trimmed.contains('/') || trimmed.contains('\n') || trimmed.contains('\r') {
        return Err("--cn 不能包含 '/' 或换行符".to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_sans(sans: &[String]) -> Result<Vec<String>, String> {
    sans.iter()
        .map(|raw| {
            let entry = raw.trim();
            if entry.is_empty() {
                return Err("--san 不能为空字符串".to_string());
            }
            if !(entry.starts_with("DNS:") || entry.starts_with("IP:") || entry.starts_with("email:")) {
                return Err(format!(
                    "--san 必须以 DNS:/IP:/email: 开头（收到 {entry:?}）"
                ));
            }
            Ok(entry.to_string())
        })
        .collect()
}

/// Default output directory for `keygen`/`csr`: `<ulnclaw home>/mtls`.
pub fn default_dir() -> PathBuf {
    crate::config::ulnclaw_home().join("mtls")
}

fn prepare_out_dir(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("无法创建目录 {}: {e}", dir.display()))?;
    Ok(dir.to_path_buf())
}

fn guard_overwrite(paths: &[&PathBuf], force: bool) -> Result<(), String> {
    if force {
        return Ok(());
    }
    let existing: Vec<String> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if existing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "目标已存在，拒绝覆盖：{}（如需覆盖请加 --force）",
        existing.join(", ")
    ))
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Run the system `openssl` (same dependency the iron-proxy egress path uses).
fn run_openssl(args: &[String]) -> Result<(), String> {
    let output = std::process::Command::new("openssl")
        .args(args)
        .output()
        .map_err(|e| format!("无法执行 openssl（请先安装 openssl CLI）: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "openssl {} 失败（exit {}）：{}",
            args.first().cloned().unwrap_or_default(),
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    Ok(())
}

fn restrict_key_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("无法设置 {} 权限为 0600: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cn_validation_rejects_injection_shapes() {
        assert_eq!(validate_cn("  gw-client  ").unwrap(), "gw-client");
        assert!(validate_cn("   ").is_err());
        assert!(validate_cn("bad/name").is_err());
        assert!(validate_cn("bad\nname").is_err());
    }

    #[test]
    fn san_validation_requires_type_prefix() {
        assert_eq!(
            validate_sans(&["DNS:gw.example.com".into(), "IP:10.0.0.7".into()]).unwrap(),
            vec!["DNS:gw.example.com".to_string(), "IP:10.0.0.7".to_string()]
        );
        assert!(validate_sans(&["gw.example.com".into()]).is_err());
        assert!(validate_sans(&["  ".into()]).is_err());
    }

    #[test]
    fn overwrite_guard_blocks_silent_clobber() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-mtls-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("client.key");
        std::fs::write(&key, "x").unwrap();
        let err = guard_overwrite(&[&key], false).unwrap_err();
        assert!(err.contains("--force"));
        assert!(guard_overwrite(&[&key], true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}