//! Page-load token handoff for embedding this UI in an iframe.
//!
//! The Strapi plugin springboard sets `http://gateway/#token=<jwt>`. The
//! hash is never sent to the gateway on `GET /` (unlike `?token=`). Parsing
//! lives here with no wasm/`web-sys` dependency so it is unit-testable on
//! the host target.

/// Extract `token` from a location hash such as `#token=abc` or
/// `#token=abc%20def&other=1`. Empty values are ignored.
pub fn token_from_hash(hash: &str) -> Option<String> {
    let hash = hash.strip_prefix('#').unwrap_or(hash);
    if hash.is_empty() {
        return None;
    }
    for pair in hash.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        if key != "token" {
            continue;
        }
        let raw = parts.next().unwrap_or("");
        if raw.is_empty() {
            return None;
        }
        return Some(percent_decode(raw));
    }
    None
}

/// Read `sub` from an unsigned JWT payload. Display-only: the gateway still
/// validates the token on the WebSocket. Empty or malformed tokens yield `None`.
pub fn email_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let json = String::from_utf8(base64url_decode(payload)?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    value
        .get("sub")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut padded = input.replace('-', "+").replace('_', "/");
    match padded.len() % 4 {
        0 => {}
        2 => padded.push_str("=="),
        3 => padded.push('='),
        _ => return None,
    }
    let mut out = Vec::with_capacity(padded.len() * 3 / 4);
    let bytes = padded.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let a = b64_val(bytes[i])?;
        let b = b64_val(bytes[i + 1])?;
        let c = bytes.get(i + 2).copied().and_then(b64_val);
        let d = bytes.get(i + 3).copied().and_then(b64_val);
        out.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            if bytes[i + 2] != b'=' {
                out.push(((b & 0x0f) << 4) | (c >> 2));
            }
            if let Some(d) = d {
                if bytes[i + 3] != b'=' {
                    out.push(((c & 0x03) << 6) | d);
                }
            }
        }
        i += 4;
    }
    Some(out)
}

fn b64_val(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        b'=' => Some(0),
        _ => None,
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                decoded.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            decoded.push(b' ');
        } else {
            decoded.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_token() {
        assert_eq!(token_from_hash("#token=abc").as_deref(), Some("abc"));
        assert_eq!(token_from_hash("token=abc").as_deref(), Some("abc"));
    }

    #[test]
    fn percent_decodes_token() {
        assert_eq!(
            token_from_hash("#token=tok%20en").as_deref(),
            Some("tok en")
        );
    }

    #[test]
    fn ignores_empty_and_other_keys() {
        assert!(token_from_hash("").is_none());
        assert!(token_from_hash("#").is_none());
        assert!(token_from_hash("#token=").is_none());
        assert!(token_from_hash("#wsBase=ws://x").is_none());
    }

    #[test]
    fn reads_token_among_other_pairs() {
        assert_eq!(
            token_from_hash("#foo=1&token=secret&bar=2").as_deref(),
            Some("secret")
        );
    }

    fn jwt_with_sub(sub: &str) -> String {
        // header.payload.sig — only the payload is parsed.
        let payload = format!(r#"{{"sub":"{sub}","purpose":"webui"}}"#);
        format!("eyJhbGciOiJFZERTQSJ9.{}.sig", base64url_encode(payload.as_bytes()))
    }

    fn base64url_encode(bytes: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut i = 0;
        while i < bytes.len() {
            let b0 = bytes[i];
            let b1 = bytes.get(i + 1).copied();
            let b2 = bytes.get(i + 2).copied();
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
            if b1.is_some() {
                out.push(TABLE[(((b1.unwrap_or(0) & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char);
            }
            if b2.is_some() {
                out.push(TABLE[(b2.unwrap_or(0) & 0x3f) as usize] as char);
            }
            i += 3;
        }
        out
    }

    #[test]
    fn email_from_jwt_reads_sub() {
        assert_eq!(
            email_from_jwt(&jwt_with_sub("admin@example.com")).as_deref(),
            Some("admin@example.com")
        );
    }

    #[test]
    fn email_from_jwt_reads_sub_among_other_claims() {
        let payload = r#"{"iss":"x","sub":"gil.fernandes@gmail.com","aud":"/ws","exp":1,"iat":1,"purpose":"webui"}"#;
        let jwt = format!(
            "eyJhbGciOiJFZERTQSJ9.{}.sig",
            base64url_encode(payload.as_bytes())
        );
        assert_eq!(
            email_from_jwt(&jwt).as_deref(),
            Some("gil.fernandes@gmail.com")
        );
    }

    #[test]
    fn email_from_jwt_rejects_garbage() {
        assert!(email_from_jwt("not-a-jwt").is_none());
        assert!(email_from_jwt("a.b").is_none());
        assert!(email_from_jwt(&jwt_with_sub("")).is_none());
    }
}
