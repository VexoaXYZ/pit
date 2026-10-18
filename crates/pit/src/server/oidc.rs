//! An OpenID Connect issuer for tasks, so CI can reach a cloud without
//! long-lived keys in its secrets.
//!
//! GitHub Actions gives each job a short-lived signed token that AWS, GCP
//! and Azure trade for temporary credentials; the cloud trusts the issuer's
//! public keys and checks the token's claims (`sub`, `aud`) against a policy.
//! A self-hosted CI without an issuer is back to storing access keys. Pit's
//! server is the issuer: it publishes its key at
//! `/.well-known/openid-configuration` and `/.well-known/jwks.json`, and signs
//! ES256 tokens only for the holder of a live job lease. Fork pull requests
//! never get one.

use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

/// How long a token is valid.
pub const TOKEN_SECS: i64 = 600;

pub struct Issuer {
    pkcs8: Vec<u8>,
    kid: String,
    x: String,
    y: String,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl Issuer {
    /// The server's signing key, created on first use and kept in the data
    /// dir (readable by the server's user only).
    pub fn load_or_create(data_dir: &Path) -> Result<Issuer> {
        let path = data_dir.join("oidc-es256.p8");
        let pkcs8 = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                let doc = EcdsaKeyPair::generate_pkcs8(
                    &ECDSA_P256_SHA256_FIXED_SIGNING,
                    &SystemRandom::new(),
                )
                .map_err(|_| anyhow::anyhow!("generating the OIDC signing key"))?;
                std::fs::write(&path, doc.as_ref())
                    .with_context(|| format!("writing {}", path.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                }
                doc.as_ref().to_vec()
            }
        };
        let pair = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|_| anyhow::anyhow!("reading the OIDC signing key {}", path.display()))?;
        // Uncompressed point: 0x04 || X (32 bytes) || Y (32 bytes).
        let public = pair.public_key().as_ref();
        let kid = crate::cas::digest(public)[..16].to_string();
        Ok(Issuer {
            kid,
            x: b64(&public[1..33]),
            y: b64(&public[33..65]),
            pkcs8,
        })
    }

    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({ "keys": [{
            "kty": "EC", "crv": "P-256", "x": self.x, "y": self.y,
            "kid": self.kid, "alg": "ES256", "use": "sig",
        }] })
    }

    pub fn discovery(&self, issuer: &str) -> serde_json::Value {
        serde_json::json!({
            "issuer": issuer,
            "jwks_uri": format!("{issuer}/.well-known/jwks.json"),
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["ES256"],
            "claims_supported": ["sub", "aud", "iss", "iat", "exp", "nbf", "jti",
                "repo", "ref", "sha", "run_id", "trigger", "trusted"],
        })
    }

    pub fn sign(&self, claims: &serde_json::Value) -> Result<String> {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        let key = jsonwebtoken::EncodingKey::from_ec_der(&self.pkcs8);
        Ok(jsonwebtoken::encode(&header, claims, &key)?)
    }
}

/// The subject a cloud policy matches on, in GitHub's shape, for a trusted
/// run: `repo:<owner/name>:ref:refs/heads/<branch>` for runs of the default
/// branch (pushes, post-merge, schedules) and `repo:<owner/name>:merge_queue`
/// for merge queue candidates into it. `None` for anything else, and for
/// names that could smuggle in another claim.
pub fn subject(repo: &str, trigger: &str, git_ref: Option<&str>) -> Option<String> {
    let safe = |s: &str| !s.is_empty() && !s.contains([':', '*', ' ']);
    if !safe(repo) {
        return None;
    }
    match trigger {
        "queue" => Some(format!("repo:{repo}:merge_queue")),
        "push" | "post-merge" => {
            let branch = git_ref.filter(|b| safe(b))?;
            Some(format!("repo:{repo}:ref:refs/heads/{branch}"))
        }
        t if t.starts_with("schedule:") => {
            let branch = git_ref.filter(|b| safe(b))?;
            Some(format!("repo:{repo}:ref:refs/heads/{branch}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_tokens_that_verify_against_the_published_key() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = Issuer::load_or_create(dir.path()).unwrap();
        // The same key comes back on restart.
        let again = Issuer::load_or_create(dir.path()).unwrap();
        assert_eq!(issuer.kid, again.kid);
        let now = crate::model::now_ms() / 1000;
        let token = issuer
            .sign(&serde_json::json!({
                "iss": "https://pit.example", "aud": "sts.amazonaws.com",
                "sub": "repo:acme/app:ref:refs/heads/main", "iat": now, "exp": now + TOKEN_SECS,
            }))
            .unwrap();
        // Verify the way a cloud does: with the key from the JWKS.
        let jwk: jsonwebtoken::jwk::JwkSet = serde_json::from_value(issuer.jwks()).unwrap();
        let key = jsonwebtoken::DecodingKey::from_jwk(&jwk.keys[0]).unwrap();
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        v.set_audience(&["sts.amazonaws.com"]);
        v.set_issuer(&["https://pit.example"]);
        let data = jsonwebtoken::decode::<serde_json::Value>(&token, &key, &v).unwrap();
        assert_eq!(data.claims["sub"], "repo:acme/app:ref:refs/heads/main");
        // Another audience is rejected.
        v.set_audience(&["something-else"]);
        assert!(jsonwebtoken::decode::<serde_json::Value>(&token, &key, &v).is_err());
    }

    #[test]
    fn subjects_only_for_trusted_shapes_and_never_injectable() {
        assert_eq!(
            subject("acme/app", "push", Some("main")).as_deref(),
            Some("repo:acme/app:ref:refs/heads/main")
        );
        assert_eq!(
            subject("acme/app", "post-merge", Some("main")).as_deref(),
            Some("repo:acme/app:ref:refs/heads/main")
        );
        assert_eq!(
            subject("acme/app", "schedule:nightly", Some("main")).as_deref(),
            Some("repo:acme/app:ref:refs/heads/main")
        );
        assert_eq!(
            subject("acme/app", "queue", Some("queue/fix")).as_deref(),
            Some("repo:acme/app:merge_queue")
        );
        // Untrusted shapes, and the review's forgery (`trigger: "ref"`), get nothing.
        for t in ["pr", "local", "api", "rerun", "ref"] {
            assert_eq!(subject("acme/app", t, Some("refs/heads/main")), None, "{t}");
        }
        // Separators cannot smuggle in another claim.
        assert_eq!(
            subject("acme/app", "push", Some("x:ref:refs/heads/main")),
            None
        );
        assert_eq!(subject("acme:x", "push", Some("main")), None);
    }
}
