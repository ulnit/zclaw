//! FFI probe — reproduces the exact Android call sequence against a local mock.
//! Prints the RAW JSON that zclaw_poll_chunks() returns, so we can see whether
//! the Rust layer emits text chunks at all.
//!
//! Usage: cargo run --example ffi_probe <api_url> <api_key> <model> <workspace> [message]

use std::ffi::{CStr, CString};
use std::thread;
use std::time::Duration;

fn s(x: &str) -> CString {
    CString::new(x).unwrap()
}

unsafe fn ptr_str(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        return "<null>".to_string();
    }
    CStr::from_ptr(p).to_string_lossy().to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let api_url = args.get(1).cloned().unwrap_or_else(|| "http://127.0.0.1:8899/v1".into());
    // key comes from ZCLAW_PROBE_KEY env — never argv, so it can't leak into shell history/logs
    let api_key = std::env::var("ZCLAW_PROBE_KEY").unwrap_or_else(|_| {
        args.get(2).cloned().unwrap_or_else(|| "sk-mock".into())
    });
    println!("key loaded: len={} sha8={}", api_key.len(), {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        api_key.hash(&mut h);
        format!("{:08x}", (h.finish() & 0xffffffff) as u32)
    });
    let model = args.get(3).cloned().unwrap_or_else(|| "mock-model".into());
    let workspace = args.get(4).cloned().unwrap_or_else(|| {
        let p = std::env::temp_dir().join("zclaw_probe_ws");
        std::fs::create_dir_all(&p).ok();
        p.to_string_lossy().to_string()
    });
    let message = args.get(5).cloned().unwrap_or_else(|| "你好".into());

    let config = format!(
        r#"{{"api_url":"{}","api_key":"{}","default_model":"{}","temperature":0.7,"workspace_dir":"{}","security":{{"autonomy":"full"}},"memory":{{"backend":"sqlite"}},"agent":{{"max_iterations":10,"system_prompt":"You are ZClaw, a helpful pocket AI assistant."}}}}"#,
        api_url, api_key, model,
        workspace.replace('\\', "/")
    );

    println!("version = {}", unsafe { ptr_str(zclaw::ffi::zclaw_version()) });

    let rc = unsafe { zclaw::ffi::zclaw_init(s(&config).as_ptr()) };
    println!("init rc = {}  (0=ok)", rc);
    if rc != 0 {
        println!("INIT FAILED — workspace/api_url problem");
        return;
    }

    let rc = unsafe { zclaw::ffi::zclaw_chat(s(&message).as_ptr()) };
    println!("chat rc = {}  (0=accepted, -1=err, -2=busy)", rc);
    if rc != 0 {
        println!("CHAT NOT ACCEPTED — would fall back to Kotlin path");
        return;
    }

    let mut poll = 0;
    let mut collected_text = String::new();
    let mut collected_thinking = String::new();
    let mut saw_done = false;
    let mut saw_error: Option<String> = None;
    let mut tool_calls: Vec<String> = Vec::new();
    loop {
        poll += 1;
        let raw = unsafe { ptr_str(zclaw::ffi::zclaw_poll_chunks()) };
        let running = unsafe { zclaw::ffi::zclaw_is_running() };

        if raw != "[]" && !raw.is_empty() {
            let show = if raw.chars().count() > 600 {
                format!("{}…<{} chars>", &raw[..raw.floor_char_boundary(600)], raw.chars().count())
            } else {
                raw.clone()
            };
            println!("poll#{} running={} RAW {}", poll, running, show);
            // tally chunk types the same way Kotlin does
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(arr) = v.as_array() {
                    for el in arr {
                        let kind = el["chunkType"].as_i64().unwrap_or(-999);
                        let name = el["name"].as_str();
                        match kind {
                            0 => {
                                if let Some(t) = name {
                                    collected_text.push_str(t);
                                } else {
                                    println!("  !! chunkType=0 but 'name' MISSING -> Kotlin drops it");
                                }
                            }
                            1 => tool_calls.push(name.unwrap_or("?").to_string()),
                            2 => println!("  (tool_result {})", name.unwrap_or("?")),
                            3 => saw_done = true,
                            4 => saw_error = name.map(|x| x.to_string()).or(Some("<no msg>".into())),
                            5 => {
                                if let Some(t) = name {
                                    collected_thinking.push_str(t);
                                } else {
                                    println!("  !! chunkType=5 but 'name' MISSING -> thinking dropped");
                                }
                            }
                            other => println!("  (chunkType={other})"),
                        }
                    }
                }
            } else {
                println!("  !! RAW is not valid JSON — Kotlin's runCatching would swallow it");
            }
        }

        if saw_done || saw_error.is_some() {
            break;
        }
        if running == 0 {
            // mimic Kotlin: drain once more then stop
            let rest = unsafe { ptr_str(zclaw::ffi::zclaw_poll_chunks()) };
            if rest != "[]" {
                println!("drain-after-idle RAW {}", rest);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&rest) {
                    if let Some(arr) = v.as_array() {
                        for el in arr {
                            match el["chunkType"].as_i64().unwrap_or(-999) {
                                0 => {
                                    if let Some(t) = el["name"].as_str() {
                                        collected_text.push_str(t);
                                    }
                                }
                                5 => {
                                    if let Some(t) = el["name"].as_str() {
                                        collected_thinking.push_str(t);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            break;
        }
        if poll > 1200 {
            println!("TIMEOUT after {} polls", poll);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    println!("---");
    println!("polls = {}", poll);
    println!("tool_calls = {:?}", tool_calls);
    println!("TEXT len = {}  value = {:?}", collected_text.chars().count(), collected_text);
    println!("THINKING len = {}  value = {:?}", collected_thinking.chars().count(),
        if collected_thinking.chars().count() > 400 {
            collected_thinking.chars().take(400).collect::<String>() + "…"
        } else {
            collected_thinking.clone()
        });
    println!("saw_done = {}  saw_error = {:?}", saw_done, saw_error);
    println!("=== VERDICT ===");
    if !collected_text.is_empty() {
        println!("OK: 有可见正文 -> UI 应能显示（bug 在 UI/JNI 层）");
    } else if !collected_thinking.is_empty() {
        println!("BUG-CONFIRMED: 模型只产出 reasoning_content(thinking)，正文 content 为空");
        println!("  -> Kotlin onThinking 只置 thinking=true 不累积文本");
        println!("  -> ZClawBubble 在 content 为空时只显示「💭 思考中…」，finish 后显示「…」");
        println!("  => 用户看到的就是「没有响应结果」");
    } else if let Some(e) = &saw_error {
        println!("Rust 报错: {} -> UI 应显示 ⚠️", e);
    } else {
        println!("Rust 既无正文也无思考也无错误 -> chunk 管道有问题");
    }
}
