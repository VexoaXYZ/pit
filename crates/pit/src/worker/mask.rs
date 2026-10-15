//! Masks secret values in task logs before they are streamed or stored.
//!
//! Exact values are replaced, and so is any base64-looking token that decodes
//! (up to three times, standard or URL-safe alphabet) to text containing a
//! secret: the tj-actions/changed-files compromise printed secrets
//! double-base64-encoded precisely to get past exact-match masking. This is
//! defense in depth, not a guarantee: a task that can read a secret can always
//! find another way to print it.

use base64::Engine;

/// Values shorter than this are not masked; they would mask ordinary words.
const MIN_SECRET_LEN: usize = 6;

#[derive(Default)]
pub struct Masker {
    secrets: Vec<String>,
}

impl Masker {
    pub fn new(values: impl IntoIterator<Item = String>) -> Masker {
        let mut secrets: Vec<String> = values
            .into_iter()
            .map(|v| v.trim().to_string())
            .filter(|v| v.len() >= MIN_SECRET_LEN)
            .collect();
        // Longest first, so a secret that contains another is masked whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Masker { secrets }
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    pub fn mask(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for s in &self.secrets {
            out = out.replace(s.as_str(), "***");
        }
        self.mask_encoded(&out)
    }

    /// Replaces base64-looking tokens that decode to a secret. A token is a
    /// run of base64 characters plus any `=` padding right after it, so
    /// `b64=<value>` splits into `b64=` and `<value>`.
    fn mask_encoded(&self, text: &str) -> String {
        let is_b64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_');
        let mut out = String::with_capacity(text.len());
        let mut token = String::new();
        let flush = |token: &mut String, out: &mut String| {
            if token.len() >= 8 && self.decodes_to_secret(token) {
                out.push_str("***");
            } else {
                out.push_str(token);
            }
            token.clear();
        };
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if is_b64(c) {
                token.push(c);
                continue;
            }
            if c == '=' && !token.is_empty() {
                token.push(c);
                while chars.peek() == Some(&'=') {
                    token.push(chars.next().unwrap());
                }
                flush(&mut token, &mut out);
                continue;
            }
            flush(&mut token, &mut out);
            out.push(c);
        }
        flush(&mut token, &mut out);
        out
    }

    fn decodes_to_secret(&self, token: &str) -> bool {
        let mut layer = token.trim().to_string();
        for _ in 0..3 {
            let decoded = [
                base64::engine::general_purpose::STANDARD.decode(&layer),
                base64::engine::general_purpose::STANDARD_NO_PAD
                    .decode(layer.trim_end_matches('=')),
                base64::engine::general_purpose::URL_SAFE.decode(&layer),
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(layer.trim_end_matches('=')),
            ]
            .into_iter()
            .find_map(|r| r.ok());
            let Some(bytes) = decoded else { return false };
            let text = String::from_utf8_lossy(&bytes).to_string();
            if self.secrets.iter().any(|s| text.contains(s.as_str())) {
                return true;
            }
            layer = text.trim().to_string();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn masks_plain_and_encoded_secrets() {
        let m = Masker::new(["tok_live_9f8e7d6c5b4a".to_string(), "abc".to_string()]);
        assert_eq!(m.mask("token=tok_live_9f8e7d6c5b4a!"), "token=***!");
        assert_eq!(
            m.mask(&format!("b64={}", b64("tok_live_9f8e7d6c5b4a"))),
            "b64=***"
        );
        // Double base64 with the newline `base64` adds in between, as in the
        // tj-actions attack.
        let twice = b64(&format!("{}\n", b64("tok_live_9f8e7d6c5b4a")));
        assert_eq!(m.mask(&format!("x {twice} y")), "x *** y");
        // Short values and unrelated base64 are left alone.
        assert_eq!(m.mask("abc def"), "abc def");
        assert_eq!(
            m.mask(&b64("hello world, nothing secret")),
            b64("hello world, nothing secret")
        );
    }
}
