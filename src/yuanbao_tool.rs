//! Yuanbao group/sticker/DM tools (hermes `tools/yuanbao_tools.py`) —
//! five tools in the `hermes-yuanbao` toolset, gated on a live yuanbao
//! adapter session (hermes `_check_yuanbao` → `get_active_adapter`).

use serde_json::{json, Value};

use crate::yuanbao_proto::MemberInfo;

const MENTION_HINT: &str =
    "To @mention a user, you MUST use the format: space + @ + nickname + space (e.g. \" @Alice \").";

/// hermes `_USER_TYPE_LABEL`.
fn role_label(role: u64) -> &'static str {
    match role {
        0 => "unknown",
        1 => "user",
        2 => "yuanbao_ai",
        3 => "bot",
        _ => "unknown",
    }
}

fn member_view(member: &MemberInfo) -> Value {
    json!({
        "user_id": member.user_id,
        "nickname": member.nickname,
        "role": role_label(member.role),
    })
}

fn not_connected() -> Value {
    json!({"success": false, "error": "Yuanbao adapter is not connected"})
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// hermes `query_group_members` filtering (pure — shared with tests).
pub fn query_members_result(
    members: &[MemberInfo],
    action: &str,
    name: &str,
    mention: bool,
) -> Value {
    let all: Vec<Value> = members.iter().map(member_view).collect();
    if all.is_empty() {
        return json!({"success": false, "error": "No members found in this group."});
    }
    let mut out = json!({});
    if mention {
        out["mention_hint"] = json!(MENTION_HINT);
    }
    match action {
        "list_bots" => {
            let bots: Vec<&Value> = all
                .iter()
                .filter(|m| matches!(m["role"].as_str(), Some("yuanbao_ai") | Some("bot")))
                .collect();
            if bots.is_empty() {
                return json!({"success": false, "error": "No bots found in this group."});
            }
            out["success"] = json!(true);
            out["msg"] = json!(format!("Found {} bot(s).", bots.len()));
            out["members"] = json!(bots);
            out
        }
        "find" => {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                out["success"] = json!(true);
                out["msg"] = json!(format!("Found {} member(s).", all.len()));
                out["members"] = json!(all);
                return out;
            }
            let needle = trimmed.to_lowercase();
            let matched: Vec<&Value> = all
                .iter()
                .filter(|m| {
                    m["nickname"]
                        .as_str()
                        .map(|n| n.to_lowercase().contains(&needle))
                        .unwrap_or(false)
                })
                .collect();
            if matched.is_empty() {
                out["success"] = json!(false);
                out["msg"] = json!(format!(
                    "No match for \"{trimmed}\". All members listed below."
                ));
                out["members"] = json!(all);
                return out;
            }
            out["success"] = json!(true);
            out["msg"] = json!(format!(
                "Found {} member(s) matching \"{trimmed}\".",
                matched.len()
            ));
            out["members"] = json!(matched);
            out
        }
        _ => {
            out["success"] = json!(true);
            out["msg"] = json!(format!("Found {} member(s).", all.len()));
            out["members"] = json!(all);
            out
        }
    }
}

/// hermes `get_group_info`.
async fn handle_query_group_info(args: &Value) -> Value {
    let group_code = arg_str(args, "group_code");
    if group_code.is_empty() {
        return json!({"success": false, "error": "group_code is required"});
    }
    let Some(handle) = crate::yuanbao::active_handle() else {
        return not_connected();
    };
    match handle.query_group_info(&group_code).await {
        Ok(info) => json!({
            "success": true,
            "group_code": group_code,
            "group_name": info.group_name,
            "member_count": info.member_count,
            "owner": {
                "user_id": info.owner_id,
                "nickname": info.owner_nickname,
            },
            "note": "The group is called \"派 (Pai)\" in the app.",
        }),
        Err(e) => json!({"success": false, "error": e}),
    }
}

/// hermes `query_group_members`.
async fn handle_query_group_members(args: &Value) -> Value {
    let group_code = arg_str(args, "group_code");
    if group_code.is_empty() {
        return json!({"success": false, "error": "group_code is required"});
    }
    let action = {
        let raw = arg_str(args, "action");
        if raw.is_empty() {
            "list_all".to_string()
        } else {
            raw
        }
    };
    let Some(handle) = crate::yuanbao::active_handle() else {
        return not_connected();
    };
    match handle.get_group_member_list(&group_code).await {
        Ok(list) => {
            let mention = args
                .get("mention")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            query_members_result(&list.members, &action, &arg_str(args, "name"), mention)
        }
        Err(e) => json!({"success": false, "error": e}),
    }
}

/// hermes `search_sticker` — local catalog lookup, no adapter traffic.
async fn handle_search_sticker(args: &Value) -> Value {
    let query = arg_str(args, "query");
    let limit = match args.get("limit").and_then(Value::as_i64) {
        Some(n) => n.clamp(1, 50) as usize,
        None => 10,
    };
    let matches = crate::yuanbao_sticker::search_stickers(&query, limit);
    json!({
        "success": true,
        "query": query,
        "count": matches.len(),
        "results": matches.iter().map(|s| json!({
            "sticker_id": s.sticker_id,
            "name": s.name,
            "description": s.description,
            "package_id": s.package_id,
        })).collect::<Vec<_>>(),
    })
}

/// hermes `send_sticker` (no session-env fallback in ulnclaw — the
/// chat target must be provided).
async fn handle_send_sticker(args: &Value) -> Value {
    let target = arg_str(args, "chat_id");
    if target.is_empty() {
        return json!({"success": false, "error": "chat_id is required (no active yuanbao session detected)"});
    }
    let Some(handle) = crate::yuanbao::active_handle() else {
        return not_connected();
    };
    let sticker = arg_str(args, "sticker");
    let reply_to = arg_str(args, "reply_to");
    match handle
        .send_sticker(
            &target,
            &sticker,
            if reply_to.is_empty() {
                None
            } else {
                Some(reply_to.as_str())
            },
        )
        .await
    {
        Ok((sticker_id, name)) => json!({
            "success": true,
            "chat_id": target,
            "sticker": {"sticker_id": sticker_id, "name": name},
            "note": "Sticker delivered to the chat. If you have additional text to say, reply now; otherwise end your turn without generating text.",
        }),
        Err(e) => json!({"success": false, "error": e}),
    }
}

/// hermes `send_dm` (text + media; ulnclaw has no session-env fallback,
/// so group_code/user_id must be supplied).
async fn handle_send_dm(args: &Value) -> Value {
    let mut user_id = arg_str(args, "user_id");
    let name = arg_str(args, "name");
    let group_code = arg_str(args, "group_code");
    let raw_message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Media: explicit media_files + embedded MEDIA:<path> tags.
    let mut media_files: Vec<std::path::PathBuf> = Vec::new();
    if let Some(items) = args.get("media_files").and_then(Value::as_array) {
        for item in items {
            if let Some(path) = item.get("path").and_then(Value::as_str) {
                if !path.trim().is_empty() {
                    media_files.push(std::path::PathBuf::from(path.trim()));
                }
            }
        }
    }
    let (message, embedded) = crate::messaging::extract_media_tags(&raw_message);
    media_files.extend(embedded);

    if user_id.is_empty() && name.is_empty() {
        return json!({"success": false, "error": "name or user_id is required"});
    }
    if user_id.is_empty() && group_code.is_empty() {
        return json!({"success": false, "error": "group_code is required when user_id is not provided"});
    }
    let Some(handle) = crate::yuanbao::active_handle() else {
        return not_connected();
    };

    // Resolve the recipient by name when needed.
    let mut nickname = user_id.clone();
    if user_id.is_empty() {
        match handle.get_group_member_list(&group_code).await {
            Ok(list) => {
                let needle = name.to_lowercase();
                let matched = list
                    .members
                    .iter()
                    .find(|m| m.nickname.to_lowercase().contains(&needle));
                match matched {
                    Some(member) => {
                        user_id = member.user_id.clone();
                        nickname = member.nickname.clone();
                    }
                    None => {
                        let names: Vec<String> =
                            list.members.iter().map(|m| m.nickname.clone()).collect();
                        return json!({
                            "success": false,
                            "error": format!(
                                "No member matching \"{name}\". Members: {}",
                                names.join(", ")
                            ),
                        });
                    }
                }
            }
            Err(e) => return json!({"success": false, "error": e}),
        }
    }

    let mut errors: Vec<String> = Vec::new();
    let mut sent_any = false;
    if !message.trim().is_empty() {
        match handle.send_dm(&user_id, &message, &group_code).await {
            Ok(()) => sent_any = true,
            Err(e) => errors.push(e),
        }
    }
    for path in &media_files {
        match handle.send_media(&format!("direct:{user_id}"), path).await {
            Ok(()) => sent_any = true,
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    if !sent_any && errors.is_empty() {
        return json!({"success": false, "error": "No deliverable text or media remained"});
    }
    if !sent_any {
        return json!({"success": false, "error": errors.join("; ")});
    }
    let mut note = format!("DM sent to \"{nickname}\" successfully.");
    if !errors.is_empty() {
        note.push_str(&format!(" (partial failure: {})", errors.join("; ")));
    }
    json!({
        "success": true,
        "user_id": user_id,
        "nickname": nickname,
        "note": note,
    })
}

/// Dispatch one yuanbao tool by name.
pub async fn run_yuanbao_tool(name: &str, args: &Value) -> Value {
    match name {
        "yb_query_group_info" => handle_query_group_info(args).await,
        "yb_query_group_members" => handle_query_group_members(args).await,
        "yb_send_dm" => handle_send_dm(args).await,
        "yb_search_sticker" => handle_search_sticker(args).await,
        "yb_send_sticker" => handle_send_sticker(args).await,
        _ => json!({"success": false, "error": format!("Unknown yuanbao tool: {name}")}),
    }
}

fn yuanbao_availability() -> crate::tools::ToolAvailability {
    match crate::yuanbao::active_handle() {
        Some(handle) if handle.is_connected() => crate::tools::ToolAvailability::available(),
        _ => crate::tools::ToolAvailability::unavailable("Yuanbao adapter is not connected"),
    }
}

pub fn register(registry: &mut crate::tools::ToolRegistry) {
    use crate::tools::tool;

    registry.register(
        tool("yb_query_group_info")
            .description(
                "Query basic info about a group (called '派/Pai' in the app), including group \
                 name, owner, and member count.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "group_code": {"type": "string", "description": "The unique group identifier (group_code)."}
                },
                "required": ["group_code"]
            }))
            .handler(|args, _ctx| async move { Ok(run_yuanbao_tool("yb_query_group_info", &args).await) })
            .toolset("hermes-yuanbao")
            .emoji("\u{1f465}")
            .check_fn(yuanbao_availability)
            .build()
            .expect("yb_query_group_info builds"),
    );

    registry.register(
        tool("yb_query_group_members")
            .description(
                "Query members of a group (called '派/Pai' in the app). Use this tool when you \
                 need to @mention someone, find a user by name, list bots (including Yuanbao \
                 AI), or list all members. IMPORTANT: You MUST call this tool before @mentioning \
                 any user, because you need the exact nickname to construct the @mention format.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "group_code": {"type": "string", "description": "The unique group identifier (group_code)."},
                    "action": {
                        "type": "string",
                        "enum": ["find", "list_bots", "list_all"],
                        "description": "find — search a user by name (use when you need to @mention or look up someone); list_bots — list bots and Yuanbao AI assistants; list_all — list all members."
                    },
                    "name": {
                        "type": "string",
                        "description": "User name to search (partial match, case-insensitive). Required for 'find'. Use the name the user mentioned in the conversation."
                    },
                    "mention": {
                        "type": "boolean",
                        "description": "Set to true when you need to @mention/at someone in your reply. The response will include the exact @mention format to use."
                    }
                },
                "required": ["group_code", "action"]
            }))
            .handler(|args, _ctx| async move { Ok(run_yuanbao_tool("yb_query_group_members", &args).await) })
            .toolset("hermes-yuanbao")
            .emoji("\u{1f4cb}")
            .check_fn(yuanbao_availability)
            .build()
            .expect("yb_query_group_members builds"),
    );

    registry.register(
        tool("yb_send_dm")
            .description(
                "Send a private/direct message (DM) to a user in a group, with optional media \
                 files. This tool automatically looks up the user by name in the group member \
                 list and sends the message. Use this when someone asks to privately message / \
                 私信 / DM a user. Supports text, images, and file attachments. You can also \
                 provide user_id directly if already known.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "group_code": {
                        "type": "string",
                        "description": "The group where the target user belongs. Extract from chat_id: 'group:328306697' → '328306697'. Required when user_id is not provided."
                    },
                    "name": {
                        "type": "string",
                        "description": "Target user's display name (partial match, case-insensitive). Required when user_id is not provided."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message text to send as a DM. Can be empty if only sending media."
                    },
                    "user_id": {
                        "type": "string",
                        "description": "Target user's account ID. If provided, skips the member lookup. Usually obtained from a previous yb_query_group_members call."
                    },
                    "media_files": {
                        "type": "array",
                        "description": "Optional list of media files to send along with the DM. Images (.jpg/.png/.gif/.webp/.bmp) are sent as image messages; other files are sent as document attachments.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Absolute local file path of the media to send."},
                                "is_voice": {"type": "boolean", "description": "Whether this file is a voice message (default false)."}
                            },
                            "required": ["path"]
                        }
                    }
                },
                "required": []
            }))
            .handler(|args, _ctx| async move { Ok(run_yuanbao_tool("yb_send_dm", &args).await) })
            .toolset("hermes-yuanbao")
            .emoji("\u{2709}\u{fe0f}")
            .check_fn(yuanbao_availability)
            .build()
            .expect("yb_send_dm builds"),
    );

    registry.register(
        tool("yb_search_sticker")
            .description(
                "Search the built-in Yuanbao sticker (TIM face / 表情包) catalogue by keyword. \
                 Returns the top matching candidates with sticker_id, name, and description. Use \
                 this BEFORE yb_send_sticker to discover the right sticker_id. Sticker = 贴纸 = \
                 TIM face — NOT a message reaction. Prefer sending a sticker over bare Unicode \
                 emoji when reacting/expressing emotion.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search keyword (Chinese or English, e.g. '666', '比心', 'cool', '吃瓜'). Empty string returns the first N stickers."
                    },
                    "limit": {"type": "integer", "description": "Max number of candidates to return (default 10, max 50)."}
                },
                "required": []
            }))
            .handler(|args, _ctx| async move { Ok(run_yuanbao_tool("yb_search_sticker", &args).await) })
            .toolset("hermes-yuanbao")
            .emoji("\u{1f50d}")
            .check_fn(yuanbao_availability)
            .build()
            .expect("yb_search_sticker builds"),
    );

    registry.register(
        tool("yb_send_sticker")
            .description(
                "Send a built-in sticker (TIMFaceElem / 贴纸表情) to the current Yuanbao chat. \
                 Call yb_search_sticker first if you don't know the sticker_id/name. Sticker = \
                 贴纸 = TIM face — NOT a message reaction. CRITICAL: Whenever the user asks you \
                 to send a sticker / 贴纸 / 表情包, you MUST use this tool. DO NOT draw a PNG via \
                 execute_code / Pillow / matplotlib and then call send_image_file — that produces \
                 a fake 'sticker' image instead of a real TIM face and is the WRONG path. If no \
                 suitable sticker_id is known, call yb_search_sticker first. When the recent \
                 thread shows users sending stickers, prefer matching that tone by replying with \
                 a sticker instead of (or in addition to) text.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "sticker": {
                        "type": "string",
                        "description": "Sticker name (e.g. '六六六', '比心', 'ok') or numeric sticker_id (e.g. '278'). Empty string sends a random built-in sticker."
                    },
                    "chat_id": {
                        "type": "string",
                        "description": "Target chat. Format: 'direct:{account_id}', 'group:{group_code}', or bare account_id."
                    },
                    "reply_to": {
                        "type": "string",
                        "description": "Optional ref_msg_id to quote-reply (group chat only)."
                    }
                },
                "required": []
            }))
            .handler(|args, _ctx| async move { Ok(run_yuanbao_tool("yb_send_sticker", &args).await) })
            .toolset("hermes-yuanbao")
            .emoji("\u{1f3a8}")
            .check_fn(yuanbao_availability)
            .build()
            .expect("yb_send_sticker builds"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yuanbao_proto::MemberInfo;

    fn members() -> Vec<MemberInfo> {
        vec![
            MemberInfo {
                user_id: "u1".into(),
                nickname: "Alice".into(),
                role: 1,
                ..Default::default()
            },
            MemberInfo {
                user_id: "u2".into(),
                nickname: "Bob".into(),
                role: 1,
                ..Default::default()
            },
            MemberInfo {
                user_id: "b1".into(),
                nickname: "YuanbaoAI".into(),
                role: 2,
                ..Default::default()
            },
            MemberInfo {
                user_id: "b2".into(),
                nickname: "WeatherBot".into(),
                role: 3,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn role_labels_match_hermes() {
        assert_eq!(role_label(0), "unknown");
        assert_eq!(role_label(1), "user");
        assert_eq!(role_label(2), "yuanbao_ai");
        assert_eq!(role_label(3), "bot");
        assert_eq!(role_label(9), "unknown");
    }

    #[test]
    fn list_all_includes_mention_hint_only_when_requested() {
        let out = query_members_result(&members(), "list_all", "", false);
        assert_eq!(out["success"], true);
        assert_eq!(out["members"].as_array().unwrap().len(), 4);
        assert!(out.get("mention_hint").is_none());
        let hinted = query_members_result(&members(), "list_all", "", true);
        assert_eq!(hinted["mention_hint"], json!(MENTION_HINT));
    }

    #[test]
    fn list_bots_filters_ai_and_bots() {
        let out = query_members_result(&members(), "list_bots", "", false);
        assert_eq!(out["success"], true);
        let bots = out["members"].as_array().unwrap();
        assert_eq!(bots.len(), 2);
        assert_eq!(out["msg"], "Found 2 bot(s).");
        let none = query_members_result(&members()[..2], "list_bots", "", false);
        assert_eq!(none["success"], false);
        assert!(none["error"].as_str().unwrap().contains("No bots"));
    }

    #[test]
    fn find_matches_case_insensitively_and_falls_back() {
        let out = query_members_result(&members(), "find", "ali", false);
        assert_eq!(out["success"], true);
        assert_eq!(out["members"].as_array().unwrap().len(), 1);
        assert_eq!(out["members"][0]["nickname"], "Alice");
        let miss = query_members_result(&members(), "find", "Zed", false);
        assert_eq!(miss["success"], false);
        assert!(miss["msg"]
            .as_str()
            .unwrap()
            .contains("No match for \"Zed\""));
        assert_eq!(miss["members"].as_array().unwrap().len(), 4);
        // Empty name returns the full roster (hermes parity).
        let all = query_members_result(&members(), "find", "", false);
        assert_eq!(all["success"], true);
        assert_eq!(all["members"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn empty_roster_reports_error() {
        let out = query_members_result(&[], "list_all", "", false);
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("No members"));
    }

    #[test]
    fn parse_chat_target_shapes() {
        assert_eq!(
            crate::yuanbao::parse_chat_target("direct:u9"),
            ("u9".to_string(), "".to_string())
        );
        assert_eq!(
            crate::yuanbao::parse_chat_target("group:777"),
            ("".to_string(), "777".to_string())
        );
        assert_eq!(
            crate::yuanbao::parse_chat_target("u5"),
            ("u5".to_string(), "".to_string())
        );
    }

    #[tokio::test]
    async fn search_sticker_uses_local_catalog() {
        let out = run_yuanbao_tool("yb_search_sticker", &json!({"query": "", "limit": 5})).await;
        assert_eq!(out["success"], true);
        assert!(out["results"].as_array().unwrap().len() <= 5);
        let first = &out["results"][0];
        assert!(first.get("sticker_id").is_some());
        assert!(first.get("name").is_some());
    }

    #[tokio::test]
    async fn group_tools_require_group_code() {
        let out = run_yuanbao_tool("yb_query_group_info", &json!({})).await;
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("group_code is required"));
        let out = run_yuanbao_tool("yb_query_group_members", &json!({"action": "list_all"})).await;
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("group_code is required"));
    }

    #[tokio::test]
    async fn send_dm_validates_recipient_inputs() {
        let out = run_yuanbao_tool("yb_send_dm", &json!({"message": "hi"})).await;
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("name or user_id is required"));
        let out = run_yuanbao_tool("yb_send_dm", &json!({"name": "Alice", "message": "hi"})).await;
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("group_code is required"));
    }

    #[tokio::test]
    async fn send_sticker_requires_chat_id_without_session() {
        let out = run_yuanbao_tool("yb_send_sticker", &json!({"sticker": "ok"})).await;
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("chat_id is required"));
    }

    #[test]
    fn register_exposes_five_tools_in_yuanbao_toolset() {
        let mut registry = crate::tools::ToolRegistry::new();
        register(&mut registry);
        for name in [
            "yb_query_group_info",
            "yb_query_group_members",
            "yb_send_dm",
            "yb_search_sticker",
            "yb_send_sticker",
        ] {
            assert_eq!(
                registry.get(name).unwrap().toolset,
                "hermes-yuanbao",
                "{name}"
            );
        }
    }
}
