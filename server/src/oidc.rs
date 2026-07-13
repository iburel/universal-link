// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! OIDC ID token validation: signature via the issuer's JWKS,
//! `iss`, `aud`, `exp`, and `iat` freshness (sensitive operations).

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::OidcConfig;

/// Token rejected — the exact cause is not exposed to the client (OIDC_INVALID).
pub struct Rejected;

#[derive(Deserialize)]
pub struct Claims {
    pub sub: String,
    pub iat: u64,
    #[allow(dead_code)]
    pub exp: u64,
}

#[derive(Deserialize)]
struct Discovery {
    jwks_uri: String,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

/// All fields are optional: a JWKS may contain keys of other families (EC, OKP)
/// or for other uses. An unusable entry is ignored; it must not invalidate the
/// whole key set.
#[derive(Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

pub struct OidcValidator {
    issuer: String,
    client_id: String,
    max_fresh_age: Duration,
    http: reqwest::Client,
    /// Issuer's JWKS, fetched on the first validation then cached.
    keys: OnceCell<HashMap<String, DecodingKey>>,
}

/// Without a bound, an unreachable or slow issuer would block the calling
/// connection (validation is awaited in its loop) and, via the `OnceCell`, all
/// concurrent `auth.enroll` / `devices.revoke`.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

impl OidcValidator {
    pub fn new(config: &OidcConfig) -> OidcValidator {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .build()
            .expect("HTTP client");
        OidcValidator {
            issuer: config.issuer_url.trim_end_matches('/').to_string(),
            client_id: config.client_id.clone(),
            max_fresh_age: config.max_fresh_token_age,
            http,
            keys: OnceCell::new(),
        }
    }

    /// Validates an ID token for a sensitive operation (`auth.enroll`,
    /// `devices.revoke`): full validity + `iat` within the freshness window.
    pub async fn validate_fresh(&self, token: &str) -> Result<Claims, Rejected> {
        let keys = self
            .keys
            .get_or_try_init(|| self.fetch_keys())
            .await
            .map_err(|_| Rejected)?;

        let header = jsonwebtoken::decode_header(token).map_err(|_| Rejected)?;
        let key = header
            .kid
            .as_deref()
            .and_then(|kid| keys.get(kid))
            .ok_or(Rejected)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.set_audience(&[&self.client_id]);
        validation.set_issuer(&[&self.issuer]);
        // `aud` and `iss` are only checked if present: without this, a token
        // that omits them would pass validation.
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        let data = jsonwebtoken::decode::<Claims>(token, key, &validation).map_err(|_| Rejected)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        if now.saturating_sub(data.claims.iat) > self.max_fresh_age.as_secs() {
            return Err(Rejected);
        }
        Ok(data.claims)
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>, anyhow::Error> {
        let discovery: Discovery = self
            .http
            .get(format!("{}/.well-known/openid-configuration", self.issuer))
            .send()
            .await?
            .json()
            .await?;
        let jwks: Jwks = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await?
            .json()
            .await?;

        let mut keys = HashMap::new();
        for jwk in jwks.keys {
            if jwk.kty.as_deref().is_some_and(|kty| kty != "RSA") {
                continue;
            }
            let (Some(kid), Some(n), Some(e)) = (jwk.kid, jwk.n, jwk.e) else {
                continue;
            };
            if let Ok(key) = DecodingKey::from_rsa_components(&n, &e) {
                keys.insert(kid, key);
            }
        }
        Ok(keys)
    }
}
