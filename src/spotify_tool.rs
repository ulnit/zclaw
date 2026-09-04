//! Native Spotify tools (hermes `plugins/spotify`) — 7 tools in the
//! `spotify` toolset backed by the Spotify Web API with PKCE OAuth
//! credentials from `auth.json` (`spotify_auth`).

use serde_json::{json, Value};

use crate::spotify_auth::{SpotifyAuthError, SpotifyRuntime};

const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Tool-facing failure (hermes `SpotifyError` family).
#[derive(Debug)]
pub enum SpotifyToolError {
    Auth(SpotifyAuthError),
    Api { status: u16, message: String },
    Other(String),
}

impl std::fmt::Display for SpotifyToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotifyToolError::Auth(e) => write!(f, "{e}"),
            SpotifyToolError::Api { message, .. } => write!(f, "{message}"),
            SpotifyToolError::Other(m) => write!(f, "Spotify tool failed: {m}"),
        }
    }
}

impl SpotifyToolError {
    fn status_code(&self) -> Option<u16> {
        match self {
            SpotifyToolError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

fn tool_error(e: &SpotifyToolError) -> Value {
    let mut out = json!({"success": false, "error": e.to_string()});
    if let Some(status) = e.status_code() {
        out["status_code"] = json!(status);
    }
    out
}

// ---------------------------------------------------------------------------
// ID / URI normalization (hermes client.normalize_spotify_*)
// ---------------------------------------------------------------------------

/// Accept a raw id, `spotify:<type>:<id>` URI, or open.spotify.com URL.
pub fn normalize_spotify_id(
    value: &str,
    expected_type: Option<&str>,
) -> Result<String, SpotifyToolError> {
    let cleaned = value.trim();
    if cleaned.is_empty() {
        return Err(SpotifyToolError::Other(
            "Spotify id/uri/url is required.".into(),
        ));
    }
    if let Some(rest) = cleaned.strip_prefix("spotify:") {
        let rebuilt = format!("spotify:{rest}");
        let parts: Vec<&str> = rebuilt.split(':').collect();
        if parts.len() >= 3 {
            let item_type = parts[1];
            if let Some(expected) = expected_type {
                if item_type != expected {
                    return Err(SpotifyToolError::Other(format!(
                        "Expected a Spotify {expected}, got {item_type}."
                    )));
                }
            }
            return Ok(parts[2].to_string());
        }
    }
    if cleaned.contains("open.spotify.com") {
        if let Ok(parsed) = url::Url::parse(cleaned) {
            let path_parts: Vec<&str> = parsed
                .path_segments()
                .map(|s| s.filter(|p| !p.is_empty()).collect())
                .unwrap_or_default();
            if path_parts.len() >= 2 {
                let (item_type, item_id) = (path_parts[0], path_parts[1]);
                if let Some(expected) = expected_type {
                    if item_type != expected {
                        return Err(SpotifyToolError::Other(format!(
                            "Expected a Spotify {expected}, got {item_type}."
                        )));
                    }
                }
                return Ok(item_id.to_string());
            }
        }
    }
    Ok(cleaned.to_string())
}

/// Normalize to a full `spotify:<type>:<id>` URI.
pub fn normalize_spotify_uri(
    value: &str,
    expected_type: Option<&str>,
) -> Result<String, SpotifyToolError> {
    let cleaned = value.trim();
    if cleaned.is_empty() {
        return Err(SpotifyToolError::Other(
            "Spotify URI/url/id is required.".into(),
        ));
    }
    if cleaned.starts_with("spotify:") {
        if let Some(expected) = expected_type {
            let parts: Vec<&str> = cleaned.split(':').collect();
            if parts.len() >= 3 && parts[1] != expected {
                return Err(SpotifyToolError::Other(format!(
                    "Expected a Spotify {expected}, got {}.",
                    parts[1]
                )));
            }
        }
        return Ok(cleaned.to_string());
    }
    let item_id = normalize_spotify_id(cleaned, expected_type)?;
    match expected_type {
        Some(expected) => Ok(format!("spotify:{expected}:{item_id}")),
        None => Ok(cleaned.to_string()),
    }
}

/// Normalize a list, deduped, at least one item.
pub fn normalize_spotify_uris(
    values: &[String],
    expected_type: Option<&str>,
) -> Result<Vec<String>, SpotifyToolError> {
    let mut uris: Vec<String> = Vec::new();
    for value in values {
        let uri = normalize_spotify_uri(value, expected_type)?;
        if !uris.contains(&uri) {
            uris.push(uri);
        }
    }
    if uris.is_empty() {
        return Err(SpotifyToolError::Other(
            "At least one Spotify item is required.".into(),
        ));
    }
    Ok(uris)
}

// ---------------------------------------------------------------------------
// Argument coercion helpers (hermes tools.py)
// ---------------------------------------------------------------------------

fn coerce_limit(args: &Value, default: i64) -> i64 {
    let value = arg_i64(args, "limit").unwrap_or(default);
    value.clamp(1, 50)
}

fn coerce_bool(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        _ => default,
    }
}

fn as_list(raw: Option<&Value>) -> Vec<String> {
    match raw {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| {
                v.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .collect(),
        Some(other) => {
            let text = other.as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![text]
            }
        }
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Web API client (hermes client.SpotifyClient)
// ---------------------------------------------------------------------------

pub struct SpotifyClient {
    runtime: SpotifyRuntime,
}

/// hermes `_friendly_spotify_error_message`.
fn friendly_error_message(
    status: u16,
    detail: &str,
    path: &str,
    retry_after: Option<&str>,
) -> String {
    let normalized = detail.to_lowercase();
    let is_playback_path = path.starts_with("/me/player");
    match status {
        401 => "Spotify authentication failed or expired. Run `ulnclaw spotify-auth login` again."
            .to_string(),
        403 => {
            if is_playback_path {
                "Spotify rejected this playback request. Playback control usually requires a Spotify Premium account and an active Spotify Connect device.".to_string()
            } else if normalized.contains("scope") || normalized.contains("permission") {
                "Spotify rejected the request because the current auth scope is insufficient. Re-run `ulnclaw spotify-auth login` to refresh permissions.".to_string()
            } else {
                "Spotify rejected the request. The account may not have permission for this action."
                    .to_string()
            }
        }
        404 => {
            if is_playback_path {
                "Spotify could not find an active playback device or player session for this request.".to_string()
            } else {
                "Spotify resource not found.".to_string()
            }
        }
        429 => {
            let mut message = "Spotify rate limit exceeded.".to_string();
            if let Some(retry) = retry_after {
                message.push_str(&format!(" Retry after {retry} seconds."));
            }
            message
        }
        _ => {
            if !detail.is_empty() {
                detail.to_string()
            } else {
                format!("Spotify API request failed with status {status}.")
            }
        }
    }
}

fn extract_error_detail(value: &Value, fallback: &str) -> String {
    if let Some(error_obj) = value.get("error") {
        if let Some(obj) = error_obj.as_object() {
            if let Some(message) = obj.get("message").and_then(Value::as_str) {
                if !message.trim().is_empty() {
                    return message.trim().to_string();
                }
            }
        } else if let Some(text) = error_obj.as_str() {
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
    }
    fallback.trim().to_string()
}

/// hermes `_describe_empty_playback`.
fn describe_empty_playback(payload: &Value, action: &str) -> Option<Value> {
    if !payload
        .get("empty")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let status_code = payload.get("status_code").cloned().unwrap_or(json!(204));
    match action {
        "get_currently_playing" => Some(json!({
            "success": true,
            "action": action,
            "is_playing": false,
            "status_code": status_code,
            "message": payload.get("message").and_then(Value::as_str).filter(|m| !m.is_empty())
                .unwrap_or("Spotify is not currently playing anything."),
        })),
        "get_state" => Some(json!({
            "success": true,
            "action": action,
            "has_active_device": false,
            "status_code": status_code,
            "message": payload.get("message").and_then(Value::as_str).filter(|m| !m.is_empty())
                .unwrap_or("No active Spotify playback session was found."),
        })),
        _ => None,
    }
}

impl SpotifyClient {
    pub async fn new() -> Result<Self, SpotifyToolError> {
        let runtime = crate::spotify_auth::resolve_runtime_credentials(false, true)
            .await
            .map_err(SpotifyToolError::Auth)?;
        Ok(Self { runtime })
    }

    /// hermes `SpotifyClient.request` — one retry on 401 with forced
    /// token refresh.
    async fn request(
        &mut self,
        method: reqwest::Method,
        path: &str,
        params: Option<Vec<(String, String)>>,
        json_body: Option<Value>,
        empty_response: Option<Value>,
        allow_retry_on_401: bool,
    ) -> Result<Value, SpotifyToolError> {
        let url = format!("{}{}", self.runtime.base_url.trim_end_matches('/'), path);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| SpotifyToolError::Other(format!("http client: {e}")))?;
        let mut request = client
            .request(method.clone(), &url)
            .header(
                "Authorization",
                format!("Bearer {}", self.runtime.access_token),
            )
            .header("Content-Type", "application/json");
        if let Some(params) = &params {
            request = request.query(
                &params
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect::<Vec<_>>(),
            );
        }
        if let Some(body) = &json_body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| SpotifyToolError::Other(format!("request failed: {e}")))?;
        let status = response.status().as_u16();
        if status == 401 && allow_retry_on_401 {
            self.runtime = crate::spotify_auth::resolve_runtime_credentials(true, true)
                .await
                .map_err(SpotifyToolError::Auth)?;
            return Box::pin(self.request(method, path, params, json_body, empty_response, false))
                .await;
        }
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_type = response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = response.text().await.unwrap_or_default();
        if status >= 400 {
            let parsed: Value = serde_json::from_str(&text).unwrap_or(json!({}));
            let detail = extract_error_detail(&parsed, &text);
            return Err(SpotifyToolError::Api {
                status,
                message: friendly_error_message(status, &detail, path, retry_after.as_deref()),
            });
        }
        if status == 204 || text.is_empty() {
            return Ok(empty_response.unwrap_or_else(
                || json!({"success": true, "status_code": status, "empty": true}),
            ));
        }
        if content_type.contains("application/json") {
            Ok(serde_json::from_str(&text)
                .unwrap_or_else(|_| json!({"success": true, "text": text})))
        } else {
            Ok(json!({"success": true, "text": text}))
        }
    }

    // ── endpoint wrappers (hermes client methods) ─────────────────────

    async fn get_devices(&mut self) -> Result<Value, SpotifyToolError> {
        self.request(
            reqwest::Method::GET,
            "/me/player/devices",
            None,
            None,
            None,
            true,
        )
        .await
    }

    async fn transfer_playback(
        &mut self,
        device_id: &str,
        play: bool,
    ) -> Result<Value, SpotifyToolError> {
        self.request(
            reqwest::Method::PUT,
            "/me/player",
            None,
            Some(json!({"device_ids": [device_id], "play": play})),
            None,
            true,
        )
        .await
    }

    async fn get_playback_state(
        &mut self,
        market: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let params = market.map(|m| vec![("market".to_string(), m.to_string())]);
        self.request(
            reqwest::Method::GET,
            "/me/player",
            params,
            None,
            Some(json!({
                "status_code": 204,
                "empty": true,
                "message": "No active Spotify playback session was found. Open Spotify on a device and start playback, or transfer playback to an available device.",
            })),
            true,
        )
        .await
    }

    async fn get_currently_playing(
        &mut self,
        market: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let params = market.map(|m| vec![("market".to_string(), m.to_string())]);
        self.request(
            reqwest::Method::GET,
            "/me/player/currently-playing",
            params,
            None,
            Some(json!({
                "status_code": 204,
                "empty": true,
                "message": "Spotify is not currently playing anything. Start playback in Spotify and try again.",
            })),
            true,
        )
        .await
    }

    async fn start_playback(
        &mut self,
        device_id: Option<&str>,
        context_uri: Option<&str>,
        uris: Option<Vec<String>>,
        offset: Option<Value>,
        position_ms: Option<i64>,
    ) -> Result<Value, SpotifyToolError> {
        let params = device_id.map(|d| vec![("device_id".to_string(), d.to_string())]);
        let mut body = serde_json::Map::new();
        if let Some(context) = context_uri {
            body.insert("context_uri".into(), json!(context));
        }
        if let Some(uris) = uris {
            body.insert("uris".into(), json!(uris));
        }
        if let Some(offset) = offset {
            body.insert("offset".into(), offset);
        }
        if let Some(position) = position_ms {
            body.insert("position_ms".into(), json!(position));
        }
        self.request(
            reqwest::Method::PUT,
            "/me/player/play",
            params,
            Some(Value::Object(body)),
            None,
            true,
        )
        .await
    }

    async fn pause_playback(&mut self, device_id: Option<&str>) -> Result<Value, SpotifyToolError> {
        let params = device_id.map(|d| vec![("device_id".to_string(), d.to_string())]);
        self.request(
            reqwest::Method::PUT,
            "/me/player/pause",
            params,
            None,
            None,
            true,
        )
        .await
    }

    async fn skip_next(&mut self, device_id: Option<&str>) -> Result<Value, SpotifyToolError> {
        let params = device_id.map(|d| vec![("device_id".to_string(), d.to_string())]);
        self.request(
            reqwest::Method::POST,
            "/me/player/next",
            params,
            None,
            None,
            true,
        )
        .await
    }

    async fn skip_previous(&mut self, device_id: Option<&str>) -> Result<Value, SpotifyToolError> {
        let params = device_id.map(|d| vec![("device_id".to_string(), d.to_string())]);
        self.request(
            reqwest::Method::POST,
            "/me/player/previous",
            params,
            None,
            None,
            true,
        )
        .await
    }

    async fn seek(
        &mut self,
        position_ms: i64,
        device_id: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![("position_ms".to_string(), position_ms.to_string())];
        if let Some(d) = device_id {
            params.push(("device_id".to_string(), d.to_string()));
        }
        self.request(
            reqwest::Method::PUT,
            "/me/player/seek",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn set_repeat(
        &mut self,
        state: &str,
        device_id: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![("state".to_string(), state.to_string())];
        if let Some(d) = device_id {
            params.push(("device_id".to_string(), d.to_string()));
        }
        self.request(
            reqwest::Method::PUT,
            "/me/player/repeat",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn set_shuffle(
        &mut self,
        state: bool,
        device_id: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![(
            "state".to_string(),
            if state {
                "true".to_string()
            } else {
                "false".to_string()
            },
        )];
        if let Some(d) = device_id {
            params.push(("device_id".to_string(), d.to_string()));
        }
        self.request(
            reqwest::Method::PUT,
            "/me/player/shuffle",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn set_volume(
        &mut self,
        volume_percent: i64,
        device_id: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![("volume_percent".to_string(), volume_percent.to_string())];
        if let Some(d) = device_id {
            params.push(("device_id".to_string(), d.to_string()));
        }
        self.request(
            reqwest::Method::PUT,
            "/me/player/volume",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn get_recently_played(
        &mut self,
        limit: i64,
        after: Option<i64>,
        before: Option<i64>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![("limit".to_string(), limit.to_string())];
        if let Some(after) = after {
            params.push(("after".to_string(), after.to_string()));
        }
        if let Some(before) = before {
            params.push(("before".to_string(), before.to_string()));
        }
        self.request(
            reqwest::Method::GET,
            "/me/player/recently-played",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn get_queue(&mut self) -> Result<Value, SpotifyToolError> {
        self.request(
            reqwest::Method::GET,
            "/me/player/queue",
            None,
            None,
            None,
            true,
        )
        .await
    }

    async fn add_to_queue(
        &mut self,
        uri: &str,
        device_id: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![("uri".to_string(), uri.to_string())];
        if let Some(d) = device_id {
            params.push(("device_id".to_string(), d.to_string()));
        }
        self.request(
            reqwest::Method::POST,
            "/me/player/queue",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn search(
        &mut self,
        query: &str,
        search_types: &[String],
        limit: i64,
        offset: i64,
        market: Option<&str>,
        include_external: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![
            ("q".to_string(), query.to_string()),
            ("type".to_string(), search_types.join(",")),
            ("limit".to_string(), limit.to_string()),
            ("offset".to_string(), offset.to_string()),
        ];
        if let Some(market) = market {
            params.push(("market".to_string(), market.to_string()));
        }
        if let Some(include) = include_external {
            params.push(("include_external".to_string(), include.to_string()));
        }
        self.request(
            reqwest::Method::GET,
            "/search",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn get_my_playlists(
        &mut self,
        limit: i64,
        offset: i64,
    ) -> Result<Value, SpotifyToolError> {
        self.request(
            reqwest::Method::GET,
            "/me/playlists",
            Some(vec![
                ("limit".to_string(), limit.to_string()),
                ("offset".to_string(), offset.to_string()),
            ]),
            None,
            None,
            true,
        )
        .await
    }

    async fn get_playlist(
        &mut self,
        playlist_id: &str,
        market: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let params = market.map(|m| vec![("market".to_string(), m.to_string())]);
        self.request(
            reqwest::Method::GET,
            &format!("/playlists/{playlist_id}"),
            params,
            None,
            None,
            true,
        )
        .await
    }

    async fn create_playlist(
        &mut self,
        name: &str,
        public: bool,
        collaborative: bool,
        description: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut body = json!({"name": name, "public": public, "collaborative": collaborative});
        if let Some(description) = description {
            body["description"] = json!(description);
        }
        self.request(
            reqwest::Method::POST,
            "/me/playlists",
            None,
            Some(body),
            None,
            true,
        )
        .await
    }

    async fn add_playlist_items(
        &mut self,
        playlist_id: &str,
        uris: &[String],
        position: Option<i64>,
    ) -> Result<Value, SpotifyToolError> {
        let mut body = json!({"uris": uris});
        if let Some(position) = position {
            body["position"] = json!(position);
        }
        self.request(
            reqwest::Method::POST,
            &format!("/playlists/{playlist_id}/items"),
            None,
            Some(body),
            None,
            true,
        )
        .await
    }

    async fn remove_playlist_items(
        &mut self,
        playlist_id: &str,
        uris: &[String],
        snapshot_id: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let items: Vec<Value> = uris.iter().map(|uri| json!({"uri": uri})).collect();
        let mut body = json!({"items": items});
        if let Some(snapshot) = snapshot_id {
            body["snapshot_id"] = json!(snapshot);
        }
        self.request(
            reqwest::Method::DELETE,
            &format!("/playlists/{playlist_id}/items"),
            None,
            Some(body),
            None,
            true,
        )
        .await
    }

    async fn update_playlist_details(
        &mut self,
        playlist_id: &str,
        name: Option<&str>,
        public: Option<bool>,
        collaborative: Option<bool>,
        description: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut body = serde_json::Map::new();
        if let Some(name) = name {
            body.insert("name".into(), json!(name));
        }
        if let Some(public) = public {
            body.insert("public".into(), json!(public));
        }
        if let Some(collaborative) = collaborative {
            body.insert("collaborative".into(), json!(collaborative));
        }
        if let Some(description) = description {
            body.insert("description".into(), json!(description));
        }
        self.request(
            reqwest::Method::PUT,
            &format!("/playlists/{playlist_id}"),
            None,
            Some(Value::Object(body)),
            None,
            true,
        )
        .await
    }

    async fn get_album(
        &mut self,
        album_id: &str,
        market: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let params = market.map(|m| vec![("market".to_string(), m.to_string())]);
        self.request(
            reqwest::Method::GET,
            &format!("/albums/{album_id}"),
            params,
            None,
            None,
            true,
        )
        .await
    }

    async fn get_album_tracks(
        &mut self,
        album_id: &str,
        limit: i64,
        offset: i64,
        market: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![
            ("limit".to_string(), limit.to_string()),
            ("offset".to_string(), offset.to_string()),
        ];
        if let Some(market) = market {
            params.push(("market".to_string(), market.to_string()));
        }
        self.request(
            reqwest::Method::GET,
            &format!("/albums/{album_id}/tracks"),
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn get_saved_tracks(
        &mut self,
        limit: i64,
        offset: i64,
        market: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![
            ("limit".to_string(), limit.to_string()),
            ("offset".to_string(), offset.to_string()),
        ];
        if let Some(market) = market {
            params.push(("market".to_string(), market.to_string()));
        }
        self.request(
            reqwest::Method::GET,
            "/me/tracks",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn get_saved_albums(
        &mut self,
        limit: i64,
        offset: i64,
        market: Option<&str>,
    ) -> Result<Value, SpotifyToolError> {
        let mut params = vec![
            ("limit".to_string(), limit.to_string()),
            ("offset".to_string(), offset.to_string()),
        ];
        if let Some(market) = market {
            params.push(("market".to_string(), market.to_string()));
        }
        self.request(
            reqwest::Method::GET,
            "/me/albums",
            Some(params),
            None,
            None,
            true,
        )
        .await
    }

    async fn save_library_items(&mut self, uris: &[String]) -> Result<Value, SpotifyToolError> {
        self.request(
            reqwest::Method::PUT,
            "/me/library",
            Some(vec![("uris".to_string(), uris.join(","))]),
            None,
            None,
            true,
        )
        .await
    }

    async fn remove_saved_items(
        &mut self,
        ids: &[String],
        kind: &str,
    ) -> Result<Value, SpotifyToolError> {
        let uris: Vec<String> = ids
            .iter()
            .map(|id| format!("spotify:{kind}:{id}"))
            .collect();
        self.request(
            reqwest::Method::DELETE,
            "/me/library",
            Some(vec![("uris".to_string(), uris.join(","))]),
            None,
            None,
            true,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Tool handlers (hermes tools.py)
// ---------------------------------------------------------------------------

async fn handle_playback(args: &Value) -> Result<Value, SpotifyToolError> {
    let action = arg_str(args, "action")
        .unwrap_or_else(|| "get_state".to_string())
        .to_lowercase();
    let mut client = SpotifyClient::new().await?;
    let device_id = arg_str(args, "device_id");
    match action.as_str() {
        "get_state" => {
            let payload = client
                .get_playback_state(arg_str(args, "market").as_deref())
                .await?;
            Ok(describe_empty_playback(&payload, &action).unwrap_or(payload))
        }
        "get_currently_playing" => {
            let payload = client
                .get_currently_playing(arg_str(args, "market").as_deref())
                .await?;
            Ok(describe_empty_playback(&payload, &action).unwrap_or(payload))
        }
        "play" => {
            let offset = match args.get("offset") {
                Some(Value::Object(obj)) => {
                    let filtered: serde_json::Map<String, Value> = obj
                        .iter()
                        .filter(|(_, v)| !v.is_null())
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if filtered.is_empty() {
                        None
                    } else {
                        Some(Value::Object(filtered))
                    }
                }
                _ => None,
            };
            let uris = if args.get("uris").map(|v| !v.is_null()).unwrap_or(false) {
                Some(normalize_spotify_uris(
                    &as_list(args.get("uris")),
                    Some("track"),
                )?)
            } else {
                None
            };
            let context_uri = match arg_str(args, "context_uri") {
                Some(raw) => {
                    let context_type = if raw.starts_with("spotify:album:")
                        || raw.contains("/album/")
                    {
                        Some("album")
                    } else if raw.starts_with("spotify:playlist:") || raw.contains("/playlist/") {
                        Some("playlist")
                    } else if raw.starts_with("spotify:artist:") || raw.contains("/artist/") {
                        Some("artist")
                    } else {
                        None
                    };
                    Some(normalize_spotify_uri(&raw, context_type)?)
                }
                None => None,
            };
            let result = client
                .start_playback(
                    device_id.as_deref(),
                    context_uri.as_deref(),
                    uris,
                    offset,
                    arg_i64(args, "position_ms"),
                )
                .await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "pause" => {
            let result = client.pause_playback(device_id.as_deref()).await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "next" => {
            let result = client.skip_next(device_id.as_deref()).await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "previous" => {
            let result = client.skip_previous(device_id.as_deref()).await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "seek" => {
            let Some(position_ms) = arg_i64(args, "position_ms") else {
                return Err(SpotifyToolError::Other(
                    "position_ms is required for action='seek'".into(),
                ));
            };
            let result = client.seek(position_ms, device_id.as_deref()).await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "set_repeat" => {
            let state = arg_str(args, "state").unwrap_or_default().to_lowercase();
            if !matches!(state.as_str(), "track" | "context" | "off") {
                return Err(SpotifyToolError::Other(
                    "state must be one of: track, context, off".into(),
                ));
            }
            let result = client.set_repeat(&state, device_id.as_deref()).await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "set_shuffle" => {
            let state = coerce_bool(args.get("state"), false);
            let result = client.set_shuffle(state, device_id.as_deref()).await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "set_volume" => {
            let Some(volume) = arg_i64(args, "volume_percent") else {
                return Err(SpotifyToolError::Other(
                    "volume_percent is required for action='set_volume'".into(),
                ));
            };
            let result = client
                .set_volume(volume.clamp(0, 100), device_id.as_deref())
                .await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        "recently_played" => {
            let after = arg_i64(args, "after");
            let before = arg_i64(args, "before");
            if after.is_some() && before.is_some() {
                return Err(SpotifyToolError::Other(
                    "Provide only one of 'after' or 'before'".into(),
                ));
            }
            Ok(client
                .get_recently_played(coerce_limit(args, 20), after, before)
                .await?)
        }
        _ => Err(SpotifyToolError::Other(format!(
            "Unknown spotify_playback action: {action}"
        ))),
    }
}

async fn handle_devices(args: &Value) -> Result<Value, SpotifyToolError> {
    let action = arg_str(args, "action")
        .unwrap_or_else(|| "list".to_string())
        .to_lowercase();
    let mut client = SpotifyClient::new().await?;
    match action.as_str() {
        "list" => Ok(client.get_devices().await?),
        "transfer" => {
            let Some(device_id) = arg_str(args, "device_id") else {
                return Err(SpotifyToolError::Other(
                    "device_id is required for action='transfer'".into(),
                ));
            };
            let result = client
                .transfer_playback(&device_id, coerce_bool(args.get("play"), false))
                .await?;
            Ok(json!({"success": true, "action": action, "result": result}))
        }
        _ => Err(SpotifyToolError::Other(format!(
            "Unknown spotify_devices action: {action}"
        ))),
    }
}

async fn handle_queue(args: &Value) -> Result<Value, SpotifyToolError> {
    let action = arg_str(args, "action")
        .unwrap_or_else(|| "get".to_string())
        .to_lowercase();
    let mut client = SpotifyClient::new().await?;
    match action.as_str() {
        "get" => Ok(client.get_queue().await?),
        "add" => {
            let uri = normalize_spotify_uri(&arg_str(args, "uri").unwrap_or_default(), None)?;
            let result = client
                .add_to_queue(&uri, arg_str(args, "device_id").as_deref())
                .await?;
            Ok(json!({"success": true, "action": action, "uri": uri, "result": result}))
        }
        _ => Err(SpotifyToolError::Other(format!(
            "Unknown spotify_queue action: {action}"
        ))),
    }
}

const SEARCH_TYPES: [&str; 7] = [
    "album",
    "artist",
    "playlist",
    "track",
    "show",
    "episode",
    "audiobook",
];

async fn handle_search(args: &Value) -> Result<Value, SpotifyToolError> {
    let mut client = SpotifyClient::new().await?;
    let Some(query) = arg_str(args, "query") else {
        return Err(SpotifyToolError::Other("query is required".into()));
    };
    let raw_types = as_list(args.get("types").or_else(|| args.get("type")));
    let raw_types = if raw_types.is_empty() {
        vec!["track".to_string()]
    } else {
        raw_types
    };
    let search_types: Vec<String> = raw_types
        .iter()
        .map(|t| t.to_lowercase())
        .filter(|t| SEARCH_TYPES.contains(&t.as_str()))
        .collect();
    if search_types.is_empty() {
        return Err(SpotifyToolError::Other(
            "types must contain one or more of: album, artist, playlist, track, show, episode, audiobook".into(),
        ));
    }
    let offset = arg_i64(args, "offset").unwrap_or(0).max(0);
    Ok(client
        .search(
            &query,
            &search_types,
            coerce_limit(args, 10),
            offset,
            arg_str(args, "market").as_deref(),
            arg_str(args, "include_external").as_deref(),
        )
        .await?)
}

async fn handle_playlists(args: &Value) -> Result<Value, SpotifyToolError> {
    let action = arg_str(args, "action")
        .unwrap_or_else(|| "list".to_string())
        .to_lowercase();
    let mut client = SpotifyClient::new().await?;
    match action.as_str() {
        "list" => {
            let offset = arg_i64(args, "offset").unwrap_or(0).max(0);
            Ok(client
                .get_my_playlists(coerce_limit(args, 20), offset)
                .await?)
        }
        "get" => {
            let playlist_id = normalize_spotify_id(
                &arg_str(args, "playlist_id").unwrap_or_default(),
                Some("playlist"),
            )?;
            Ok(client
                .get_playlist(&playlist_id, arg_str(args, "market").as_deref())
                .await?)
        }
        "create" => {
            let Some(name) = arg_str(args, "name") else {
                return Err(SpotifyToolError::Other(
                    "name is required for action='create'".into(),
                ));
            };
            Ok(client
                .create_playlist(
                    &name,
                    coerce_bool(args.get("public"), false),
                    coerce_bool(args.get("collaborative"), false),
                    arg_str(args, "description").as_deref(),
                )
                .await?)
        }
        "add_items" => {
            let playlist_id = normalize_spotify_id(
                &arg_str(args, "playlist_id").unwrap_or_default(),
                Some("playlist"),
            )?;
            let uris = normalize_spotify_uris(&as_list(args.get("uris")), None)?;
            Ok(client
                .add_playlist_items(&playlist_id, &uris, arg_i64(args, "position"))
                .await?)
        }
        "remove_items" => {
            let playlist_id = normalize_spotify_id(
                &arg_str(args, "playlist_id").unwrap_or_default(),
                Some("playlist"),
            )?;
            let uris = normalize_spotify_uris(&as_list(args.get("uris")), None)?;
            Ok(client
                .remove_playlist_items(&playlist_id, &uris, arg_str(args, "snapshot_id").as_deref())
                .await?)
        }
        "update_details" => {
            let playlist_id = normalize_spotify_id(
                &arg_str(args, "playlist_id").unwrap_or_default(),
                Some("playlist"),
            )?;
            Ok(client
                .update_playlist_details(
                    &playlist_id,
                    arg_str(args, "name").as_deref(),
                    args.get("public").and_then(Value::as_bool),
                    args.get("collaborative").and_then(Value::as_bool),
                    arg_str(args, "description").as_deref(),
                )
                .await?)
        }
        _ => Err(SpotifyToolError::Other(format!(
            "Unknown spotify_playlists action: {action}"
        ))),
    }
}

async fn handle_albums(args: &Value) -> Result<Value, SpotifyToolError> {
    let action = arg_str(args, "action")
        .unwrap_or_else(|| "get".to_string())
        .to_lowercase();
    let mut client = SpotifyClient::new().await?;
    let album_id = normalize_spotify_id(
        &arg_str(args, "album_id")
            .or_else(|| arg_str(args, "id"))
            .unwrap_or_default(),
        Some("album"),
    )?;
    match action.as_str() {
        "get" => Ok(client
            .get_album(&album_id, arg_str(args, "market").as_deref())
            .await?),
        "tracks" => {
            let offset = arg_i64(args, "offset").unwrap_or(0).max(0);
            Ok(client
                .get_album_tracks(
                    &album_id,
                    coerce_limit(args, 20),
                    offset,
                    arg_str(args, "market").as_deref(),
                )
                .await?)
        }
        _ => Err(SpotifyToolError::Other(format!(
            "Unknown spotify_albums action: {action}"
        ))),
    }
}

async fn handle_library(args: &Value) -> Result<Value, SpotifyToolError> {
    let kind = arg_str(args, "kind").unwrap_or_default().to_lowercase();
    if kind != "tracks" && kind != "albums" {
        return Err(SpotifyToolError::Other(
            "kind must be one of: tracks, albums".into(),
        ));
    }
    let action = arg_str(args, "action")
        .unwrap_or_else(|| "list".to_string())
        .to_lowercase();
    let item_type = if kind == "tracks" { "track" } else { "album" };
    let mut client = SpotifyClient::new().await?;
    match action.as_str() {
        "list" => {
            let limit = coerce_limit(args, 20);
            let offset = arg_i64(args, "offset").unwrap_or(0).max(0);
            let market = arg_str(args, "market");
            if kind == "tracks" {
                Ok(client
                    .get_saved_tracks(limit, offset, market.as_deref())
                    .await?)
            } else {
                Ok(client
                    .get_saved_albums(limit, offset, market.as_deref())
                    .await?)
            }
        }
        "save" => {
            let items = as_list(args.get("uris").or_else(|| args.get("items")));
            let uris = normalize_spotify_uris(&items, Some(item_type))?;
            Ok(client.save_library_items(&uris).await?)
        }
        "remove" => {
            let items = as_list(args.get("ids").or_else(|| args.get("items")));
            let mut ids = Vec::new();
            for item in &items {
                ids.push(normalize_spotify_id(item, Some(item_type))?);
            }
            if ids.is_empty() {
                return Err(SpotifyToolError::Other(
                    "ids/items is required for action='remove'".into(),
                ));
            }
            Ok(client.remove_saved_items(&ids, item_type).await?)
        }
        _ => Err(SpotifyToolError::Other(format!(
            "Unknown spotify_library action: {action}"
        ))),
    }
}

/// Dispatch one spotify tool by name.
pub async fn run_spotify_tool(name: &str, args: &Value) -> Value {
    let result = match name {
        "spotify_playback" => handle_playback(args).await,
        "spotify_devices" => handle_devices(args).await,
        "spotify_queue" => handle_queue(args).await,
        "spotify_search" => handle_search(args).await,
        "spotify_playlists" => handle_playlists(args).await,
        "spotify_albums" => handle_albums(args).await,
        "spotify_library" => handle_library(args).await,
        _ => Err(SpotifyToolError::Other(format!(
            "Unknown spotify tool: {name}"
        ))),
    };
    match result {
        Ok(value) => value,
        Err(e) => tool_error(&e),
    }
}

fn spotify_availability() -> crate::tools::ToolAvailability {
    let status = crate::spotify_auth::auth_status();
    if status
        .get("logged_in")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        crate::tools::ToolAvailability::available()
    } else {
        crate::tools::ToolAvailability::unavailable(
            "Spotify not authenticated (ulnclaw spotify-auth login)",
        )
    }
}

pub fn register(registry: &mut crate::tools::ToolRegistry) {
    use crate::tools::tool;

    let common_string = json!({"type": "string"});

    let entries: [(&str, &str, Value, &str); 7] = [
        (
            "spotify_playback",
            "Control Spotify playback, inspect the active playback state, or fetch recently played tracks.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["get_state", "get_currently_playing", "play", "pause", "next", "previous", "seek", "set_repeat", "set_shuffle", "set_volume", "recently_played"]},
                    "device_id": common_string,
                    "market": common_string,
                    "context_uri": common_string,
                    "uris": {"type": "array", "items": common_string},
                    "offset": {"type": "object"},
                    "position_ms": {"type": "integer"},
                    "state": {"description": "For set_repeat use track/context/off. For set_shuffle use boolean-like true/false.", "oneOf": [{"type": "string"}, {"type": "boolean"}]},
                    "volume_percent": {"type": "integer"},
                    "limit": {"type": "integer", "description": "For recently_played: number of tracks (max 50)"},
                    "after": {"type": "integer", "description": "For recently_played: Unix ms cursor (after this timestamp)"},
                    "before": {"type": "integer", "description": "For recently_played: Unix ms cursor (before this timestamp)"}
                },
                "required": ["action"]
            }),
            "\u{1f3b5}",
        ),
        (
            "spotify_devices",
            "List Spotify Connect devices or transfer playback to a different device.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "transfer"]},
                    "device_id": common_string,
                    "play": {"type": "boolean"}
                },
                "required": ["action"]
            }),
            "\u{1f508}",
        ),
        (
            "spotify_queue",
            "Inspect the user's Spotify queue or add an item to it.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["get", "add"]},
                    "uri": common_string,
                    "device_id": common_string
                },
                "required": ["action"]
            }),
            "\u{1f4fb}",
        ),
        (
            "spotify_search",
            "Search the Spotify catalog for tracks, albums, artists, playlists, shows, or episodes.",
            json!({
                "type": "object",
                "properties": {
                    "query": common_string,
                    "types": {"type": "array", "items": common_string},
                    "type": common_string,
                    "limit": {"type": "integer"},
                    "offset": {"type": "integer"},
                    "market": common_string,
                    "include_external": common_string
                },
                "required": ["query"]
            }),
            "\u{1f50e}",
        ),
        (
            "spotify_playlists",
            "List, inspect, create, update, and modify Spotify playlists.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "get", "create", "add_items", "remove_items", "update_details"]},
                    "playlist_id": common_string,
                    "market": common_string,
                    "limit": {"type": "integer"},
                    "offset": {"type": "integer"},
                    "name": common_string,
                    "description": common_string,
                    "public": {"type": "boolean"},
                    "collaborative": {"type": "boolean"},
                    "uris": {"type": "array", "items": common_string},
                    "position": {"type": "integer"},
                    "snapshot_id": common_string
                },
                "required": ["action"]
            }),
            "\u{1f4da}",
        ),
        (
            "spotify_albums",
            "Fetch Spotify album metadata or album tracks.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["get", "tracks"]},
                    "album_id": common_string,
                    "id": common_string,
                    "market": common_string,
                    "limit": {"type": "integer"},
                    "offset": {"type": "integer"}
                },
                "required": ["action"]
            }),
            "\u{1f4bf}",
        ),
        (
            "spotify_library",
            "List, save, or remove the user's saved Spotify tracks or albums. Use `kind` to select which.",
            json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["tracks", "albums"], "description": "Which library to operate on"},
                    "action": {"type": "string", "enum": ["list", "save", "remove"]},
                    "limit": {"type": "integer"},
                    "offset": {"type": "integer"},
                    "market": common_string,
                    "uris": {"type": "array", "items": common_string},
                    "ids": {"type": "array", "items": common_string},
                    "items": {"type": "array", "items": common_string}
                },
                "required": ["kind", "action"]
            }),
            "\u{2764}\u{fe0f}",
        ),
    ];

    for (name, description, parameters, emoji) in entries {
        registry.register(
            tool(name)
                .description(description)
                .parameters(parameters)
                .handler(move |args, _ctx| {
                    let name = name.to_string();
                    async move { Ok(run_spotify_tool(&name, &args).await) }
                })
                .toolset("spotify")
                .emoji(emoji)
                .check_fn(spotify_availability)
                .build()
                .unwrap_or_else(|_| panic!("{name} builds")),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_id_handles_uri_url_and_plain() {
        assert_eq!(
            normalize_spotify_id("spotify:track:abc123", Some("track")).unwrap(),
            "abc123"
        );
        assert_eq!(
            normalize_spotify_id("https://open.spotify.com/track/abc123?si=x", Some("track"))
                .unwrap(),
            "abc123"
        );
        assert_eq!(normalize_spotify_id("abc123", None).unwrap(), "abc123");
        let e = normalize_spotify_id("spotify:album:abc", Some("track")).unwrap_err();
        assert!(
            e.to_string()
                .contains("Expected a Spotify track, got album"),
            "{e}"
        );
        assert!(normalize_spotify_id("   ", None).is_err());
    }

    #[test]
    fn normalize_uri_builds_full_uri_and_checks_type() {
        assert_eq!(
            normalize_spotify_uri("abc123", Some("track")).unwrap(),
            "spotify:track:abc123"
        );
        assert_eq!(
            normalize_spotify_uri("spotify:track:abc", Some("track")).unwrap(),
            "spotify:track:abc"
        );
        // expected_type=None keeps the input form (hermes parity); typed calls upgrade to URIs.
        assert_eq!(
            normalize_spotify_uri("https://open.spotify.com/playlist/pl1", None).unwrap(),
            "https://open.spotify.com/playlist/pl1"
        );
        assert_eq!(
            normalize_spotify_uri("https://open.spotify.com/playlist/pl1", Some("playlist"))
                .unwrap(),
            "spotify:playlist:pl1"
        );
        assert!(normalize_spotify_uri("spotify:album:x", Some("track")).is_err());
    }

    #[test]
    fn normalize_uris_dedupes_and_requires_items() {
        let uris = normalize_spotify_uris(
            &["a".into(), "spotify:track:a".into(), "b".into()],
            Some("track"),
        )
        .unwrap();
        assert_eq!(uris, vec!["spotify:track:a", "spotify:track:b"]);
        assert!(normalize_spotify_uris(&[], Some("track")).is_err());
    }

    #[test]
    fn coerce_helpers_match_hermes() {
        assert_eq!(coerce_limit(&json!({}), 20), 20);
        assert_eq!(coerce_limit(&json!({"limit": 0}), 20), 1);
        assert_eq!(coerce_limit(&json!({"limit": 500}), 20), 50);
        assert_eq!(coerce_limit(&json!({"limit": "7"}), 20), 7);
        assert!(coerce_bool(Some(&json!("yes")), false));
        assert!(!coerce_bool(Some(&json!("off")), true));
        assert!(coerce_bool(None, true));
        assert_eq!(as_list(Some(&json!(["a", " ", "b"]))), vec!["a", "b"]);
        assert_eq!(as_list(Some(&json!("solo"))), vec!["solo"]);
        assert!(as_list(None).is_empty());
        assert_eq!(arg_i64(&json!({"v": "42"}), "v"), Some(42));
    }

    #[test]
    fn friendly_errors_map_status_codes() {
        assert!(friendly_error_message(401, "", "/me/player", None).contains("spotify-auth login"));
        assert!(friendly_error_message(403, "", "/me/player/play", None).contains("Premium"));
        assert!(
            friendly_error_message(403, "insufficient scope", "/search", None).contains("scope")
        );
        assert!(
            friendly_error_message(404, "", "/me/player", None).contains("active playback device")
        );
        assert!(friendly_error_message(404, "", "/albums/x", None).contains("not found"));
        assert!(friendly_error_message(429, "", "/search", Some("5")).contains("Retry after 5"));
        assert_eq!(
            friendly_error_message(500, "", "/search", None),
            "Spotify API request failed with status 500."
        );
    }

    #[test]
    fn extract_error_detail_prefers_structured_message() {
        let body = json!({"error": {"message": " Player command failed "}});
        assert_eq!(extract_error_detail(&body, "raw"), "Player command failed");
        let body = json!({"error": "plain string"});
        assert_eq!(extract_error_detail(&body, "raw"), "plain string");
        assert_eq!(extract_error_detail(&json!({}), "  raw "), "raw");
    }

    #[test]
    fn describe_empty_playback_shapes() {
        let payload = json!({"empty": true, "status_code": 204});
        let state = describe_empty_playback(&payload, "get_state").unwrap();
        assert_eq!(state["has_active_device"], false);
        assert_eq!(state["success"], true);
        let playing = describe_empty_playback(&payload, "get_currently_playing").unwrap();
        assert_eq!(playing["is_playing"], false);
        assert!(describe_empty_playback(&payload, "play").is_none());
        assert!(describe_empty_playback(&json!({"item": {}}), "get_state").is_none());
    }

    fn spotify_env_guard() -> (tempfile::TempDir, Option<String>) {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        (tmp, prev)
    }

    fn restore_home(prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[tokio::test]
    async fn tools_fail_fast_without_auth() {
        let _guard = crate::models_dev::test_env_lock();
        let (_tmp, prev) = spotify_env_guard();
        let out = run_spotify_tool("spotify_playback", &json!({"action": "get_state"})).await;
        assert_eq!(out["success"], false);
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("spotify-auth login"),
            "{out}"
        );
        let out = run_spotify_tool("spotify_devices", &json!({"action": "list"})).await;
        assert_eq!(out["success"], false);
        restore_home(prev);
    }

    #[tokio::test]
    async fn handler_validation_runs_before_auth_where_possible() {
        let _guard = crate::models_dev::test_env_lock();
        let (_tmp, prev) = spotify_env_guard();
        // library kind validation precedes client construction.
        let out = run_spotify_tool(
            "spotify_library",
            &json!({"kind": "movies", "action": "list"}),
        )
        .await;
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("kind must be one of"),
            "{out}"
        );
        // search builds the client first (hermes parity) → auth error wins.
        let out = run_spotify_tool("spotify_search", &json!({})).await;
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("spotify-auth login"),
            "{out}"
        );
        restore_home(prev);
    }

    #[test]
    fn register_exposes_seven_tools_in_spotify_toolset() {
        let mut registry = crate::tools::ToolRegistry::new();
        register(&mut registry);
        for name in [
            "spotify_playback",
            "spotify_devices",
            "spotify_queue",
            "spotify_search",
            "spotify_playlists",
            "spotify_albums",
            "spotify_library",
        ] {
            assert_eq!(registry.get(name).unwrap().toolset, "spotify", "{name}");
        }
        let schema = registry.get("spotify_playback").unwrap();
        assert_eq!(
            schema.definition.parameters["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            11
        );
    }

    #[test]
    fn availability_gates_on_auth_state() {
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        assert!(!spotify_availability().is_available());
        let future = chrono::Utc::now() + chrono::Duration::seconds(3600);
        let state = json!({
            "access_token": "at",
            "refresh_token": "rt",
            "expires_at": future.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        });
        crate::spotify_auth::store_provider_state(&state).unwrap();
        assert!(spotify_availability().is_available());
        restore_home(prev);
    }
}
