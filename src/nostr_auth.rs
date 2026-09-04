//! Dependency-light Nostr signing for Buzz WebSocket authentication —
//! port of hermes `plugins/platforms/buzz/nostr_auth.py`.
//!
//! NIP-42 relay auth answers the relay's `AUTH` challenge with a signed
//! kind-22242 event. Curve math rides the pure-Rust `k256` crate
//! (BIP-340 schnorr); the nsec bech32 decoding is a direct port of the
//! hermes implementation.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const BECH32_CHARSET: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
/// hermes `CURVE_ORDER`.
const CURVE_ORDER_HEX: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";

fn bech32_polymod(values: &[u8]) -> u32 {
    const GENERATORS: [u32; 5] = [0x3B6A57B2, 0x26508E6D, 0x1EA119FA, 0x3D4233DD, 0x2A1462B3];
    let mut checksum: u32 = 1;
    for &value in values {
        let top = checksum >> 25;
        checksum = ((checksum & 0x1FFFFFF) << 5) ^ u32::from(value);
        for (index, generator) in GENERATORS.iter().enumerate() {
            if (top >> index) & 1 == 1 {
                checksum ^= generator;
            }
        }
    }
    checksum
}

fn bech32_hrp_expand(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|b| b >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|b| b & 31));
    out
}

/// Decode an `nsec1...` bech32 secret key into 32 raw bytes (hermes
/// `_decode_nsec`).
fn decode_nsec(value: &str) -> Result<[u8; 32], String> {
    if value.chars().any(|c| c.is_ascii_uppercase())
        && value.chars().any(|c| c.is_ascii_lowercase())
    {
        return Err("nsec cannot mix upper- and lowercase".into());
    }
    let normalized = value.to_lowercase();
    let separator = normalized.rfind('1').ok_or("invalid nsec encoding")?;
    if separator < 1 || separator + 7 > normalized.len() {
        return Err("invalid nsec encoding".into());
    }
    let hrp = &normalized[..separator];
    if hrp != "nsec" {
        return Err("private key must use the nsec prefix".into());
    }
    let mut data: Vec<u8> = Vec::new();
    for ch in normalized[separator + 1..].chars() {
        let idx = BECH32_CHARSET.find(ch).ok_or("invalid character in nsec")?;
        data.push(idx as u8);
    }
    let mut check_input = bech32_hrp_expand(hrp);
    check_input.extend_from_slice(&data);
    if bech32_polymod(&check_input) != 1 {
        return Err("invalid nsec checksum".into());
    }
    let mut decoded: Vec<u8> = Vec::new();
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    for &value5 in &data[..data.len() - 6] {
        accumulator = (accumulator << 5) | u32::from(value5);
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            decoded.push(((accumulator >> bits) & 0xFF) as u8);
        }
    }
    if bits > 0 && (accumulator & ((1 << bits) - 1)) != 0 {
        return Err("non-zero nsec padding".into());
    }
    if decoded.len() != 32 {
        return Err("nsec must encode exactly 32 bytes".into());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded);
    Ok(out)
}

/// hermes `decode_private_key`: 64-hex chars or `nsec1...` bech32 →
/// 32-byte scalar, range-checked against the secp256k1 curve order.
pub fn decode_private_key(value: &str) -> Result<[u8; 32], String> {
    let raw = value.trim();
    let bytes: Vec<u8> = if raw.to_lowercase().starts_with("nsec1") {
        decode_nsec(raw)?.to_vec()
    } else {
        let hexed = raw.strip_prefix("0x").unwrap_or(raw);
        let mut bytes = Vec::new();
        let chars: Vec<char> = hexed.chars().collect();
        if chars.len() != 64 {
            return Err("private key must be 64 hex characters or nsec".into());
        }
        for pair in chars.chunks(2) {
            let byte = u8::from_str_radix(&pair.iter().collect::<String>(), 16)
                .map_err(|_| "private key must be 64 hex characters or nsec")?;
            bytes.push(byte);
        }
        bytes
    };
    // Range check: 1 <= key < curve order.
    let order = hex_bytes(CURVE_ORDER_HEX).expect("static hex");
    let is_zero = bytes.iter().all(|b| *b == 0);
    if is_zero || bytes >= order {
        return Err("private key is outside the secp256k1 range".into());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_bytes(hex: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = hex.chars().collect();
    if chars.len() % 2 != 0 {
        return Err("odd-length hex".into());
    }
    for pair in chars.chunks(2) {
        out.push(
            u8::from_str_radix(&pair.iter().collect::<String>(), 16).map_err(|e| e.to_string())?,
        );
    }
    Ok(out)
}

fn signing_key(private_key: &str) -> Result<k256::schnorr::SigningKey, String> {
    let scalar = decode_private_key(private_key)?;
    k256::schnorr::SigningKey::from_slice(&scalar).map_err(|e| format!("invalid private key: {e}"))
}

/// x-only public key hex for a private key (hermes `public_key_hex`).
pub fn public_key_hex(private_key: &str) -> Result<String, String> {
    let key = signing_key(private_key)?;
    let bytes = key.verifying_key().to_bytes();
    Ok(hex_encode(bytes))
}

/// BIP-340 schnorr signature over a 32-byte message (hermes
/// `schnorr_sign`). Auxiliary randomness is mixed from the clock and a
/// process-local counter (BIP-340 treats aux as defense-in-depth).
pub fn schnorr_sign(message: &[u8; 32], private_key: &str) -> Result<[u8; 64], String> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = signing_key(private_key)?;
    let nonce_seed = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let aux: [u8; 32] = Sha256::digest(
        [
            &nonce_seed.to_le_bytes()[..],
            &nanos.to_le_bytes()[..],
            message.as_slice(),
        ]
        .concat(),
    )
    .into();
    let signature = key
        .sign_raw(message, &aux)
        .map_err(|e| format!("schnorr sign failed: {e}"))?;
    Ok(signature.to_bytes())
}

/// Build a signed NIP-42 kind-22242 auth event (hermes
/// `build_auth_event`): `[relay, challenge]` tags plus the optional
/// NIP-OA owner-attestation tag from `BUZZ_AUTH_TAG`, sha256 event id
/// over the canonical serialization, schnorr signature.
pub fn build_auth_event(
    private_key: &str,
    challenge: &str,
    relay_url: &str,
    auth_tag_json: &str,
    created_at: Option<i64>,
) -> Result<Value, String> {
    let mut tags: Vec<Value> = vec![json!(["relay", relay_url]), json!(["challenge", challenge])];
    let trimmed = auth_tag_json.trim();
    if !trimmed.is_empty() {
        let auth_tag: Value = serde_json::from_str(trimmed)
            .map_err(|_| "BUZZ_AUTH_TAG is not valid JSON".to_string())?;
        let items = auth_tag
            .as_array()
            .ok_or("BUZZ_AUTH_TAG must be a four-string auth tag")?;
        if items.len() != 4
            || items
                .first()
                .and_then(|v| v.as_str())
                .map(|s| s != "auth")
                .unwrap_or(true)
            || !items.iter().all(|v| v.is_string())
        {
            return Err("BUZZ_AUTH_TAG must be a four-string auth tag".into());
        }
        tags.push(auth_tag);
    }
    let pubkey = public_key_hex(private_key)?;
    let timestamp = created_at.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
    let serialized = serde_json::to_string(&json!([
        0,
        Value::String(pubkey.clone()),
        json!(timestamp),
        json!(22242),
        json!(tags),
        json!("")
    ]))
    .map_err(|e| e.to_string())?;
    let event_id: [u8; 32] = Sha256::digest(serialized.as_bytes()).into();
    let sig = schnorr_sign(&event_id, private_key)?;
    Ok(json!({
        "id": hex_encode(event_id),
        "pubkey": pubkey,
        "created_at": timestamp,
        "kind": 22242,
        "tags": tags,
        "content": "",
        "sig": hex_encode(sig),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors generated with hermes' own nostr_auth.py.
    const KEY_ONE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const KEY_THREE: &str = "0000000000000000000000000000000000000000000000000000000000000003";
    const KEY_MISC: &str = "7f8a9b0c1d2e3f40516273849506a7b8c9d0e1f2031425364758697a8b9c0d1e";
    const NSEC_ONE: &str = "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqsmhltgl";

    #[test]
    fn pubkey_vectors_match_hermes() {
        assert_eq!(
            public_key_hex(KEY_ONE).unwrap(),
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        );
        assert_eq!(
            public_key_hex(KEY_THREE).unwrap(),
            "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9"
        );
        assert_eq!(
            public_key_hex(KEY_MISC).unwrap(),
            "b12127f8dcf7c235283ebe57fe7464a1500d02bc0164d29b88757fb99b8bd05f"
        );
    }

    #[test]
    fn nsec_decode_matches_hermes() {
        let key = decode_private_key(NSEC_ONE).unwrap();
        assert_eq!(key[31], 1);
        assert!(key[..31].iter().all(|b| *b == 0));
        assert_eq!(
            public_key_hex(NSEC_ONE).unwrap(),
            public_key_hex(KEY_ONE).unwrap()
        );
        assert!(decode_private_key("nsec1invalid").is_err());
        assert!(decode_private_key("abc").is_err());
        assert!(decode_private_key(&"0".repeat(64)).is_err()); // zero key
    }

    #[test]
    fn auth_event_id_matches_hermes_vector() {
        let event = build_auth_event(
            KEY_THREE,
            "chal-123",
            "wss://relay.example/community",
            "",
            Some(1700000000),
        )
        .unwrap();
        assert_eq!(
            event["id"],
            "9ff9e58bde4fbceae06f902bf190057bdef83567fd3eb3d5f7838fd4e1b455ab"
        );
        assert_eq!(event["kind"], 22242);
        assert_eq!(event["created_at"], 1700000000);
        assert_eq!(
            event["tags"],
            json!([
                ["relay", "wss://relay.example/community"],
                ["challenge", "chal-123"]
            ])
        );
        // Signature verifies against the x-only pubkey (BIP-340).
        let id = hex_bytes(event["id"].as_str().unwrap()).unwrap();
        let sig_bytes = hex_bytes(event["sig"].as_str().unwrap()).unwrap();
        let pubkey = hex_bytes(event["pubkey"].as_str().unwrap()).unwrap();
        let vk = k256::schnorr::VerifyingKey::from_slice(&pubkey).unwrap();
        let mut msg = [0u8; 32];
        msg.copy_from_slice(&id);
        let signature = k256::schnorr::Signature::try_from(sig_bytes.as_slice()).unwrap();
        vk.verify_raw(&msg, &signature).unwrap();
    }

    #[test]
    fn auth_tag_validation() {
        let ok = build_auth_event(KEY_ONE, "c", "wss://r", r#"["auth","a","b","c"]"#, Some(1));
        assert!(ok.is_ok());
        assert_eq!(ok.unwrap()["tags"].as_array().unwrap().len(), 3);
        assert!(
            build_auth_event(KEY_ONE, "c", "wss://r", r#"["nope","a","b","c"]"#, Some(1)).is_err()
        );
        assert!(build_auth_event(KEY_ONE, "c", "wss://r", r#"["auth","a","b"]"#, Some(1)).is_err());
        assert!(build_auth_event(KEY_ONE, "c", "wss://r", "not json", Some(1)).is_err());
    }
}
