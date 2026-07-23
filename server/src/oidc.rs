// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! OIDC ID token validation: signature via the issuer's JWKS,
//! `iss`, `aud`, `exp`, and `iat` freshness (sensitive operations).
//!
//! The issuer's signing keys are fetched lazily on the first validation and
//! cached, then **re-fetched when a token arrives with a key id absent from the
//! cache** — this is how an IdP key rotation is picked up without restarting the
//! server (before, a `OnceCell` cached the keys for the whole process lifetime,
//! so every login started to fail the moment the IdP rotated). The re-fetch is
//! rate-limited (`jwks_refresh_min_interval`) so a flood of tokens bearing
//! unknown key ids cannot turn into one JWKS request per token.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

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

/// The issuer's decoding keys by `kid`, plus when they were last fetched (to
/// rate-limit re-fetches). `last_fetch` is `None` until the first fetch, so the
/// lazy first fetch is never throttled.
#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, Arc<DecodingKey>>,
    last_fetch: Option<Instant>,
}

pub struct OidcValidator {
    issuer: String,
    client_id: String,
    max_fresh_age: Duration,
    /// Shortest delay between two JWKS fetches (see `OidcConfig`).
    refresh_min_interval: Duration,
    http: reqwest::Client,
    cache: RwLock<KeyCache>,
    /// Serializes JWKS refreshes: concurrent key-id misses coalesce into a
    /// single fetch (no thundering herd on the issuer). Held across the network
    /// call — but the data lock is NOT, so cache hits never wait on a refresh.
    refresh: tokio::sync::Mutex<()>,
}

/// Without a bound, an unreachable or slow issuer would block the calling
/// connection (validation is awaited in its loop) and, while `refresh` is held,
/// the other key-id misses waiting to fetch.
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
            refresh_min_interval: config.jwks_refresh_min_interval,
            http,
            cache: RwLock::new(KeyCache::default()),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    /// Validates an ID token for a sensitive operation (`auth.enroll`,
    /// `devices.revoke`): full validity + `iat` within the freshness window.
    pub async fn validate_fresh(&self, token: &str) -> Result<Claims, Rejected> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| Rejected)?;
        let kid = header.kid.ok_or(Rejected)?;
        let key = self.key_for_kid(&kid).await.ok_or(Rejected)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.set_audience(&[&self.client_id]);
        validation.set_issuer(&[&self.issuer]);
        // `aud` and `iss` are only checked if present: without this, a token
        // that omits them would pass validation.
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        let data =
            jsonwebtoken::decode::<Claims>(token, &key, &validation).map_err(|_| Rejected)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        if now.saturating_sub(data.claims.iat) > self.max_fresh_age.as_secs() {
            return Err(Rejected);
        }
        Ok(data.claims)
    }

    /// The decoding key for `kid`. Fast path: a cached key, no network. On a
    /// miss (unknown or rotated key id) it re-fetches the JWKS once — rate-
    /// limited — then retries. `None` if the key stays unknown.
    async fn key_for_kid(&self, kid: &str) -> Option<Arc<DecodingKey>> {
        // Fast path: a cached key. Only a brief read lock; it is never held
        // across the network, so a hit is not stalled by an in-flight refresh.
        if let Some(key) = self.cache.read().await.keys.get(kid).cloned() {
            return Some(key);
        }

        // Miss. `refresh` serializes refreshes so concurrent misses coalesce
        // into one fetch. Crucially the *data* lock is not held across the
        // fetch below, so cache hits keep flowing while it is in flight.
        let _refresh = self.refresh.lock().await;

        // A task that refreshed before us may have just cached this key.
        if let Some(key) = self.cache.read().await.keys.get(kid).cloned() {
            return Some(key);
        }
        // Fetched too recently: the key really is absent (garbage kid, or an
        // attacker's). Reject without hammering the issuer.
        let last_fetch = self.cache.read().await.last_fetch;
        let throttled = last_fetch.is_some_and(|last| {
            Instant::now().saturating_duration_since(last) < self.refresh_min_interval
        });
        if throttled {
            return None;
        }

        // Fetch with no data lock held. `refresh` is still held, so a second
        // miss waits here rather than launching its own fetch.
        let fetched = self.fetch_keys().await;

        let mut cache = self.cache.write().await;
        // Count the attempt even when the fetch fails: an unreachable issuer
        // must not be retried on every token (a transient failure therefore
        // costs up to one `refresh_min_interval` of rejections — a deliberate
        // trade to keep a down issuer from being hammered).
        cache.last_fetch = Some(Instant::now());
        if let Ok(keys) = fetched {
            cache.keys = keys;
        }
        cache.keys.get(kid).cloned()
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, Arc<DecodingKey>>, anyhow::Error> {
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
                keys.insert(kid, Arc::new(key));
            }
        }
        Ok(keys)
    }
}
