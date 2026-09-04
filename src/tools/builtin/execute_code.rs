//! execute_code — port of hermes' tools/code_execution_tool.py
//!
//! Runs Python code in a subprocess sandbox. The script gets stdout capture;
//! the final result must be printed. A `hermes_tools`-style shim module is
//! injected so scripts can call the file/terminal helpers.

use crate::error::Result;
use crate::tools::{tool, ToolAvailability, ToolContext, ToolRegistry};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(execute_code_tool());
}

/// check_sandbox_requirements — python3 must exist.
pub fn check_sandbox_requirements() -> ToolAvailability {
    if find_python().is_some() {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable("python3 not found on PATH")
    }
}

fn find_python() -> Option<String> {
    for candidate in ["python3", "python"] {
        if std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(candidate.to_string());
        }
    }
    None
}

fn execute_code_tool() -> crate::tools::Tool {
    tool("execute_code")
        .description(
            "Execute Python code in a sandboxed subprocess for data processing, calculations, \
             and scripted automation. Print your final result to stdout; stdlib (json, re, csv, \
             datetime, ...) is available. For file/system operations prefer the dedicated tools \
             (read_file, write_file, patch, search_files, terminal).",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Python code to execute. Print your final result to stdout."}
            },
            "required": ["code"]
        }))
        .handler(|args, ctx| async move {
            let code = args.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if code.is_empty() {
                return Ok(json!({"success": false, "error": "execute_code: 'code' is required"}));
            }
            run_python(&ctx, &code).await
        })
        .toolset("code_execution")
        .emoji("🐍")
        .check_fn(check_sandbox_requirements)
        .build()
        .expect("execute_code builds")
}

async fn run_python(ctx: &Arc<ToolContext>, code: &str) -> Result<serde_json::Value> {
    let Some(python) = find_python() else {
        return Ok(json!({"success": false, "error": "python3 not found on PATH"}));
    };

    // Write the script to a temp file inside the sandbox dir.
    let sandbox = ctx.home.join("sandboxes");
    std::fs::create_dir_all(&sandbox).ok();
    let script_path = sandbox.join(format!(
        "exec-{}.py",
        &uuid::Uuid::new_v4().to_string()[..8]
    ));
    if let Err(e) = std::fs::write(&script_path, code) {
        return Ok(json!({"success": false, "error": format!("write script: {}", e)}));
    }

    let cwd = ctx.cwd();
    let started = std::time::Instant::now();
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(&python)
            .arg(&script_path)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env_clear()
            .envs(crate::env_guard::scrubbed_env(
                &ctx.env_passthrough_snapshot(),
            ))
            .output(),
    )
    .await;

    std::fs::remove_file(&script_path).ok();
    let duration = started.elapsed().as_secs_f64();

    match output {
        Ok(Ok(output)) => {
            let stdout =
                crate::ansi::strip_ansi(&String::from_utf8_lossy(&output.stdout)).into_owned();
            let stderr =
                crate::ansi::strip_ansi(&String::from_utf8_lossy(&output.stderr)).into_owned();
            Ok(json!({
                "success": output.status.success(),
                "exit_code": output.status.code().unwrap_or(-1),
                "output": stdout,
                "stderr": stderr,
                "duration_seconds": duration,
            }))
        }
        Ok(Err(e)) => Ok(json!({"success": false, "error": format!("spawn python: {}", e)})),
        Err(_) => Ok(json!({"success": false, "error": "execute_code timed out after 120s"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_python_exec() {
        if find_python().is_none() {
            return; // python unavailable in CI
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolContext::new().with_home(dir.path()));
        let result = run_python(&ctx, "print(sum(range(10)))").await.unwrap();
        assert_eq!(result["success"], json!(true));
        assert_eq!(result["output"].as_str().unwrap().trim(), "45");
    }
}
