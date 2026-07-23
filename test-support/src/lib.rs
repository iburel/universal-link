// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Shared test building blocks between the server and Core suites:
//! fake OIDC (JWKS + signed ID tokens), Ed25519 device keys, JSON-RPC
//! WebSocket client and enrollment/authentication flows.
//!
//! Protocol decisions remain frozen by each suite's harness
//! (`server/tests/api/support.rs`, `core/tests/api/support.rs`) — this crate
//! carries only the common machinery.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// In-memory data-plane transport (double of the Core's `PeerTransport`).
pub mod memory_transport;

pub const TEST_CLIENT_ID: &str = "universallink-tests";
/// The default test account — the one the fake OIDC's browser flow
/// authenticates until `FakeOidc::set_user` decides otherwise.
pub const TEST_SUB: &str = "test-user";
pub const TEST_EMAIL: &str = "test-user@example.com";
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Observation window to assert that no notification arrives.
pub const SILENCE_WINDOW: Duration = Duration::from_millis(300);

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

// ---------------------------------------------------------------------------
// Fake OIDC: serves discovery + the JWKS, signs test ID tokens.
// Fixed RSA keys generated for the suite (never used outside tests).
// ---------------------------------------------------------------------------

const TEST_OIDC_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDV3cOb9mrushAg
xmnoLdPWPcjLLBXS4vMusqKWDXiMNCYxCJodS3B6qkMrSEgsanE3RUFo10o6SIA+
x6dvHgbNLQPCEA1Sv4B96GFcVdRUdcvJ2wxOR7BtAoQebL/3o67Xra0j9trNqA8g
3sV9MfvzppMzL8zK7rWsrjuzVqCn1CFyOV7uPD8GQu7XlpeXpUGFEWWr6RstEZZ1
O7sHRW5Y8Sqj/IvTQTwGEZL8ldPZN958eleI290c2OOaVxLtX7JIDFg76oSmfl85
ywD4mHiZikLa+clqkwkkgh75FbYnQ9ZzqbtQol2nWbquyjpxLYHmhAEWbgsFO7RR
BgGlPioVAgMBAAECggEAJ3HtC52B8H+AEQN3ch7NtPSVTb/WUJySPLku2p0mcDmI
F8Ad6KxK1R0FIX0F7sc9FXQdEdCvfJt1p96cJ5byqzITwN3ED1CJyu8q0eR0spU9
XoAbv25igGCX7KKjL3INo/MA/aKgjZDjJW1hIGuxLCm7eZPl4kHv2ScTlMqz+/AC
XMXutyFTkdrm/JHQSFTFWCrZ/8RhodepjRuslrJAQlGt9mzyaK8jH1TWyx5L3eoH
9+jNEYKMIQ396E/K4sjP1mo6s+tMNv8gbeLBhgHmpl6gVKd1xMZzmwgTik5RA3/b
KKjBCglKM1EGQsGHEDs14jke7wu7hjxWuUKFbRYLKwKBgQDunSi+Gyu6ab6zHAHo
xw+CTAv+E0EZowhRuIGrhX0uD8w4/jPW6jN1wbtBKCXTWs2IUClh2Drayf0yiCv/
3qM9Yw0xK05cP8JgIL4M1cyqENLT8qpHNYCsY9DXth3RnaHVEeYABRSOgam1nEmg
GJFc5Zb/V/aulaEu2TQMfrmpLwKBgQDlcv1jLpPi8vAJjMeW6UyyfCS8PvHpdDyj
42fBU+mAwLjBkbrA14b8RhiJw6QHOWf023h9mVcc6V/DngTXo3X4fB4KHuqUmRyb
CbElILN1UqFfBHDK+89VZ2toWysTS0DNLPI8faq2nqrUT//x5OzKl5/LuUNoYau4
MH64eHsH+wKBgQC34kNIpdXAFcfRbc8G3YjVJ9fCGIJ9yEcb+y1qyjea7K+8aCH5
CIlZwU+WOeNUitCDsQsDDUjl3O2UKJ6H08JHB3NeJfqAWt5niDQN3iGYGdjvXz3c
HJ4pu85wvjcil/DkK6Ps9f1OiRwJCgUvLF+xkHkaFGbVShhp6xcSXgKkHwKBgCpC
5sbo4lZP1sR7iJuUNws88Gs30MjmHvE0xnyGXyDW3nDQzawNRpCavJRLU5/9A9fG
wMepgbESjw/xwAST1/u/qKzGiwB5vxoTS+yBvGwknxJoN/o7LTIWzWK4QBParYHd
AHUB1Hq0eNSIM6UzPCYIjWcxpWfJbZ6MWBrUHh0XAoGAH2TtH00W/V+de5gx5fjT
MQFm7bI8Soenxf+zP4ZKCbRNprmS5ebAMDOK+JrpG33V0dpCheLZxVMVAlydCw2u
XVcBOxYnNHCkcCPKuCUVe6XJWoR8w+FMy2OrC8LapcaJnJNn5HkOHPEwwVLLrcy+
ZfO9Dq5ab2o+T23p46T5QEs=
-----END PRIVATE KEY-----";

const TEST_OIDC_KEY_N_B64: &str = "1d3Dm_Zq7rIQIMZp6C3T1j3IyywV0uLzLrKilg14jDQmMQiaHUtweqpDK0hILGpxN0VBaNdKOkiAPsenbx4GzS0DwhANUr-AfehhXFXUVHXLydsMTkewbQKEHmy_96Ou162tI_bazagPIN7FfTH786aTMy_Myu61rK47s1agp9Qhcjle7jw_BkLu15aXl6VBhRFlq-kbLRGWdTu7B0VuWPEqo_yL00E8BhGS_JXT2TfefHpXiNvdHNjjmlcS7V-ySAxYO-qEpn5fOcsA-Jh4mYpC2vnJapMJJIIe-RW2J0PWc6m7UKJdp1m6rso6cS2B5oQBFm4LBTu0UQYBpT4qFQ";

/// Second key, absent from the JWKS: signs tokens with an invalid signature.
const TEST_OIDC_WRONG_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDoSWrbxqZ6QuNC
ZgCGtGzGeSEBQn5FF2PFqo0OPSIEtoDrZ6IQv0NCEitEPdSVzhPAJXVYkJQGfqz4
We3Zft8NUtah8jZ8tKBEc1HKs6L3GROSGnU0u/YEMc2+a+Emc7y4W05RThGvo+rL
zHQ5FtB3TT+6TPe02NqymuEju7FIxxtXqUAOzGdIcH/xg5iMIjchcMSqh+wdlvDN
sS7Ly5VDWiW2mbQ/TINNuT/22tisLxCbI3jUX9vwHN46Cm0SJhZaOLcNVSotzgp4
9vXBdbP5neA1TFK8/FDE+7T0/Ett7Nem8Iq45g3awFWZt5e1WRCwdDzrYi/zmFdr
F4q6ye5NAgMBAAECggEAWBWfJABMmQhMTZ2IjYxVw12jmmqwn1qjRw3Jt6CPCeJ7
OMlvt5IP2zowlgwsVTJ0YuTRfug0edIHnZXckCGAS/kh0v+akeec7tgcKBW+sp0b
wsetsnWkcSrBrngSRRaWdgKJzGMiacxq+SVq3Us9ekAc7nTJahbht+DrhzVhoQrG
1/iU5cwuzsSymCZzJs66sM0Ik6KefeZvpvI79g3R7/lvhhJXT6sXFl3IPifVdQcf
toWyth4SYvdKXju5TPJsicBnk9RRCCGsDFto7MO1dc6lanNCAJsYbhQmoo4OuabO
HD4YuRozljAtL1TV+lZUvS4WFLjjQVbEvKoiVwUTsQKBgQD7U/dgqMbNVYfUdH4T
3aNTGbzt7V+EdXGrKgi28mlftrnGC5EeN13bnr4r7O/SYSbo2sBSvVgXyRzEoqq2
nL8+qzm8815ZptMM3zVQORcMZmPosvzwuKpxoL7HSfKkzSTNb1hGufOrnv1tfLNz
1Yp/VKh1CAkv4IQpXxPwkuasVwKBgQDsmtY0n/wTt58bWSnTfeWYwV7HnDP2F+wb
L1x1SqKHUhUG9gvzVlGOqGESg8xp/2qqjpkyFQtjELEZKd+dUTo+m/M4W697UBwy
+uEyE9aooXtdj1LW32PI4RxetC2GvTzQdYWsNXh+0zAvm3NA7njqE18iOCftOK1a
zNwuJf+T+wKBgBrfBGD6Sp8jmO03M0+ub8rvwopxybeg0vFpAhuFWYzZPY2WKQLh
CpDzrQOHRrDooD9fPBbclbGdWA0SE0yI/82UgwzXvGu7cW874jhckkFKJT54/KBE
Lj0N4bfvCRljsZ6hW//b29iqnA/7uDgXScKJa6VvoYPT7m158+jR3AXrAoGABFfX
RMIHC4mcVxEs7l/qPgKWrc1VOtg3kkwtQ03qa9d64VTU5VOZTagTmBZpQyzYFWdm
sn+mZNwilBarrySVkB6muUsdjoLq8ZifV577mr7UF+SQnbceCsrvDWH7T/TbT+xI
Vt/oZVOVF9qfo/p8p7dRULx9JyKaNAd8pzA1X88CgYAFhs+6efBraHUa4KShIjkR
1TfJLGvcKkJ9Q6PnK1j1EF3WWnZT2A4mSp1iXiXinEssHnshVvWKZ5osWPj40n6b
uv/H+SOnwe9vhBHL9/kuLoaJ+PymYJj3B5B8qJOiIfMRkGLigvVP/cLUrJdsi01T
D4t4ISEGy0B/qoKF9PCFrQ==
-----END PRIVATE KEY-----";

const TEST_OIDC_KID: &str = "test-key";
/// Second signing key, used by `rotate_signing_key` to simulate an IdP key
/// rotation. Its material is `TEST_OIDC_WRONG_KEY_PEM`; the modulus below is
/// that key's `n` (base64url), so the JWKS can advertise it after rotation.
const TEST_OIDC_KID_2: &str = "test-key-2";
const TEST_OIDC_WRONG_KEY_N_B64: &str = "6Elq28amekLjQmYAhrRsxnkhAUJ-RRdjxaqNDj0iBLaA62eiEL9DQhIrRD3Ulc4TwCV1WJCUBn6s-Fnt2X7fDVLWofI2fLSgRHNRyrOi9xkTkhp1NLv2BDHNvmvhJnO8uFtOUU4Rr6Pqy8x0ORbQd00_ukz3tNjasprhI7uxSMcbV6lADsxnSHB_8YOYjCI3IXDEqofsHZbwzbEuy8uVQ1oltpm0P0yDTbk_9trYrC8QmyN41F_b8BzeOgptEiYWWji3DVUqLc4KePb1wXWz-Z3gNUxSvPxQxPu09PxLbezXpvCKuOYN2sBVmbeXtVkQsHQ862Iv85hXaxeKusnuTQ";

/// An IdP signing key as the fake serves it: `kid`, the PEM it signs with, and
/// the modulus (`n`) it advertises in the JWKS.
#[derive(Clone, Copy)]
struct IssuerKey {
    kid: &'static str,
    pem: &'static str,
    n_b64: &'static str,
}

const PRIMARY_ISSUER_KEY: IssuerKey = IssuerKey {
    kid: TEST_OIDC_KID,
    pem: TEST_OIDC_KEY_PEM,
    n_b64: TEST_OIDC_KEY_N_B64,
};
const ROTATED_ISSUER_KEY: IssuerKey = IssuerKey {
    kid: TEST_OIDC_KID_2,
    pem: TEST_OIDC_WRONG_KEY_PEM,
    n_b64: TEST_OIDC_WRONG_KEY_N_B64,
};

/// The live state of the browser flow: the user the next `authorize`
/// authenticates, the authorization codes to exchange, and the refresh
/// tokens issued.
struct OidcFlows {
    /// (sub, email) — email absent: the IdP does not emit the claim (scope denied).
    user: (String, Option<String>),
    codes: HashMap<String, AuthCode>,
    /// refresh token → (sub, email).
    refresh_tokens: HashMap<String, (String, Option<String>)>,
    /// The `refresh_token` grants issue ID tokens with an aged `iat`
    /// (valid token but no longer "fresh") — to exercise the server-side
    /// `OIDC_INVALID` rejection of sensitive operations.
    stale_refresh: bool,
    /// Current signing key: what `id_token(...)` signs with and what `/jwks`
    /// advertises. Swapped by `rotate_signing_key`.
    signing: IssuerKey,
}

/// Authorization code awaiting exchange, tied to its request (PKCE).
struct AuthCode {
    challenge: String,
    redirect_uri: String,
    sub: String,
    email: Option<String>,
}

pub struct FakeOidc {
    base_url: String,
    flows: Arc<Mutex<OidcFlows>>,
    /// Number of times `/jwks` has been served — lets a test assert the server
    /// does not re-fetch the key set on every token (rate-limited refresh).
    jwks_hits: Arc<AtomicUsize>,
}

impl FakeOidc {
    pub async fn start() -> FakeOidc {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake OIDC");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));

        let discovery = json!({
            "issuer": base_url,
            "jwks_uri": format!("{base_url}/jwks"),
            "authorization_endpoint": format!("{base_url}/authorize"),
            "token_endpoint": format!("{base_url}/token"),
        });
        let flows = Arc::new(Mutex::new(OidcFlows {
            user: (TEST_SUB.to_string(), Some(TEST_EMAIL.to_string())),
            codes: HashMap::new(),
            refresh_tokens: HashMap::new(),
            stale_refresh: false,
            signing: PRIMARY_ISSUER_KEY,
        }));
        let jwks_hits = Arc::new(AtomicUsize::new(0));

        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(move || {
                    let v = discovery.clone();
                    async move { axum::Json(v) }
                }),
            )
            .route(
                "/jwks",
                axum::routing::get({
                    let flows = flows.clone();
                    let hits = jwks_hits.clone();
                    move || {
                        let flows = flows.clone();
                        let hits = hits.clone();
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            let key = flows.lock().expect("lock OIDC").signing;
                            axum::Json(json!({
                                "keys": [{
                                    "kty": "RSA",
                                    "use": "sig",
                                    "alg": "RS256",
                                    "kid": key.kid,
                                    "n": key.n_b64,
                                    "e": "AQAB",
                                }]
                            }))
                        }
                    }
                }),
            )
            .route(
                "/authorize",
                axum::routing::get({
                    let flows = flows.clone();
                    move |axum::extract::RawQuery(query): axum::extract::RawQuery| {
                        let flows = flows.clone();
                        async move { authorize(&flows, query.as_deref().unwrap_or("")) }
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post({
                    let flows = flows.clone();
                    let issuer = base_url.clone();
                    move |body: String| {
                        let flows = flows.clone();
                        async move { token(&flows, &issuer, &body) }
                    }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fake OIDC");
        });

        FakeOidc {
            base_url,
            flows,
            jwks_hits,
        }
    }

    pub fn issuer(&self) -> String {
        self.base_url.clone()
    }

    /// Simulates an IdP signing-key rotation: subsequent `id_token(...)` are
    /// signed by a new key under a new `kid`, and `/jwks` advertises that key
    /// (only). A server that cached the previous JWKS must re-fetch to accept
    /// the new tokens.
    pub fn rotate_signing_key(&self) {
        self.flows.lock().expect("lock OIDC").signing = ROTATED_ISSUER_KEY;
    }

    /// How many times the server has fetched `/jwks`.
    pub fn jwks_fetch_count(&self) -> usize {
        self.jwks_hits.load(Ordering::SeqCst)
    }

    /// The user the next browser flow will authenticate.
    pub fn set_user(&self, sub: &str, email: &str) {
        self.flows.lock().expect("lock OIDC").user = (sub.to_string(), Some(email.to_string()));
    }

    /// Like `set_user`, but the IdP will not emit an `email` claim (scope
    /// denied by the user, IdP that does not carry it…).
    pub fn set_user_without_email(&self, sub: &str) {
        self.flows.lock().expect("lock OIDC").user = (sub.to_string(), None);
    }

    /// The next `refresh_token` grants will issue ID tokens with an aged
    /// `iat`: valid, but no longer fresh enough for sensitive operations.
    pub fn stale_refresh_grants(&self) {
        self.flows.lock().expect("lock OIDC").stale_refresh = true;
    }

    /// Invalidates all issued refresh tokens (expiration, IdP-side
    /// revocation): the next `refresh_token` grant will fail with `invalid_grant`.
    pub fn revoke_refresh_tokens(&self) {
        self.flows.lock().expect("lock OIDC").refresh_tokens.clear();
    }

    /// Valid and fresh ID token for `sub`.
    pub fn id_token(&self, sub: &str) -> String {
        self.id_token_with(sub, |_| {})
    }

    /// ID token whose default claims `tweak` may alter
    /// (`iss`, `sub`, `aud`, `iat` = now, `exp` = +1 h). Signed by the current
    /// signing key (see `rotate_signing_key`).
    pub fn id_token_with(
        &self,
        sub: &str,
        tweak: impl FnOnce(&mut serde_json::Map<String, Value>),
    ) -> String {
        let signing = self.flows.lock().expect("lock OIDC").signing;
        let mut claims = self.default_claims(sub);
        tweak(&mut claims);
        sign_token(signing.kid, signing.pem, &claims)
    }

    /// ID token with valid claims, stamped with the current `kid` but signed
    /// with the wrong private key: the signature check fails.
    pub fn id_token_wrong_key(&self, sub: &str) -> String {
        let signing = self.flows.lock().expect("lock OIDC").signing;
        // A key genuinely different from the one the JWKS advertises for `kid`.
        let wrong_pem = if signing.pem == TEST_OIDC_KEY_PEM {
            TEST_OIDC_WRONG_KEY_PEM
        } else {
            TEST_OIDC_KEY_PEM
        };
        let claims = self.default_claims(sub);
        sign_token(signing.kid, wrong_pem, &claims)
    }

    /// ID token with valid claims, stamped with a `kid` the IdP never
    /// advertises. The server cannot resolve the key — exercises the
    /// unknown-key-id path (and, with a real cooldown, its rate limit) without
    /// an actual rotation.
    pub fn id_token_unknown_kid(&self, sub: &str) -> String {
        let claims = self.default_claims(sub);
        sign_token("no-such-kid", TEST_OIDC_KEY_PEM, &claims)
    }

    fn default_claims(&self, sub: &str) -> serde_json::Map<String, Value> {
        let now = unix_now();
        let mut claims = serde_json::Map::new();
        claims.insert("iss".into(), json!(self.issuer()));
        claims.insert("sub".into(), json!(sub));
        claims.insert("aud".into(), json!(TEST_CLIENT_ID));
        claims.insert("iat".into(), json!(now));
        claims.insert("exp".into(), json!(now + 3600));
        claims
    }
}

fn sign_token(kid: &str, pem: &str, claims: &serde_json::Map<String, Value>) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.into());
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).expect("test RSA key");
    jsonwebtoken::encode(&header, claims, &key).expect("JWT signature")
}

/// ID token issued by the browser flow or a refresh, signed by the IdP's
/// current key. `email`: the claim from which the Core derives session.json's
/// `account` (absent if `None`). `iat_age`: seconds by which the `iat` is aged
/// (0 = fresh) — the `exp` stays in the future, the token is valid but no
/// longer "fresh".
fn signed_id_token(
    signing: &IssuerKey,
    issuer: &str,
    sub: &str,
    email: Option<&str>,
    iat_age: u64,
) -> String {
    let now = unix_now();
    let mut claims = serde_json::Map::new();
    claims.insert("iss".into(), json!(issuer));
    claims.insert("sub".into(), json!(sub));
    claims.insert("aud".into(), json!(TEST_CLIENT_ID));
    claims.insert("iat".into(), json!(now - iat_age));
    claims.insert("exp".into(), json!(now + 3600));
    if let Some(email) = email {
        claims.insert("email".into(), json!(email));
    }
    sign_token(signing.kid, signing.pem, &claims)
}

/// `GET /authorize`: validates the request (client, PKCE S256), authenticates
/// the current user without a screen (consent is instantaneous), and
/// redirects to `redirect_uri` with a single-use code.
fn authorize(flows: &Mutex<OidcFlows>, query: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let bad = |what: &str| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            format!("invalid authorize: {what}"),
        )
            .into_response()
    };
    if params.get("client_id").map(String::as_str) != Some(TEST_CLIENT_ID) {
        return bad("client_id");
    }
    if params.get("response_type").map(String::as_str) != Some("code") {
        return bad("response_type");
    }
    if params.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return bad("code_challenge_method");
    }
    let Some(challenge) = params.get("code_challenge").filter(|c| !c.is_empty()) else {
        return bad("code_challenge");
    };
    let Some(redirect_uri) = params.get("redirect_uri") else {
        return bad("redirect_uri");
    };
    let Some(state) = params.get("state").filter(|s| !s.is_empty()) else {
        return bad("state");
    };

    let code = format!("code_{}", random_hex(16));
    let mut f = flows.lock().expect("lock OIDC");
    let (sub, email) = f.user.clone();
    f.codes.insert(
        code.clone(),
        AuthCode {
            challenge: challenge.clone(),
            redirect_uri: redirect_uri.clone(),
            sub,
            email,
        },
    );
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    let location = format!(
        "{redirect_uri}{sep}{}",
        form_urlencoded::Serializer::new(String::new())
            .append_pair("code", &code)
            .append_pair("state", state)
            .finish()
    );
    (
        axum::http::StatusCode::FOUND,
        [(axum::http::header::LOCATION, location)],
    )
        .into_response()
}

/// `POST /token`: exchanges a code (grant `authorization_code`, verifies the
/// PKCE `code_verifier`, issues a refresh token) or refresh (grant
/// `refresh_token`). OAuth errors: `400 { "error": … }`.
fn token(flows: &Mutex<OidcFlows>, issuer: &str, body: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    let params: HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let denied = |error: &str| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": error })),
        )
            .into_response()
    };
    if params.get("client_id").map(String::as_str) != Some(TEST_CLIENT_ID) {
        return denied("invalid_client");
    }
    match params.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let (Some(code), Some(redirect_uri), Some(verifier)) = (
                params.get("code"),
                params.get("redirect_uri"),
                params.get("code_verifier"),
            ) else {
                return denied("invalid_request");
            };
            let mut f = flows.lock().expect("lock OIDC");
            // Single-use: the code is consumed even if the exchange fails.
            let Some(auth) = f.codes.remove(code) else {
                return denied("invalid_grant");
            };
            if auth.redirect_uri != *redirect_uri {
                return denied("invalid_grant");
            }
            let hashed = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(verifier.as_bytes()));
            if hashed != auth.challenge {
                return denied("invalid_grant");
            }
            let refresh = format!("rt_{}", random_hex(16));
            f.refresh_tokens
                .insert(refresh.clone(), (auth.sub.clone(), auth.email.clone()));
            let signing = f.signing;
            chunked_json(json!({
                "id_token": signed_id_token(&signing, issuer, &auth.sub, auth.email.as_deref(), 0),
                "refresh_token": refresh,
                "token_type": "Bearer",
                "expires_in": 3600,
            }))
        }
        Some("refresh_token") => {
            let (known, stale, signing) = {
                let f = flows.lock().expect("lock OIDC");
                (
                    params
                        .get("refresh_token")
                        .and_then(|t| f.refresh_tokens.get(t).cloned()),
                    f.stale_refresh,
                    f.signing,
                )
            };
            let Some((sub, email)) = known else {
                return denied("invalid_grant");
            };
            // Aged by one hour: still valid, no longer fresh enough for
            // sensitive operations.
            let iat_age = if stale { 3600 } else { 0 };
            chunked_json(json!({
                "id_token": signed_id_token(&signing, issuer, &sub, email.as_deref(), iat_age),
                "token_type": "Bearer",
                "expires_in": 3600,
            }))
        }
        _ => denied("unsupported_grant_type"),
    }
}

/// Response with an UNKNOWN body length: hyper therefore emits it as
/// `Transfer-Encoding: chunked`, exactly like Google's token endpoint
/// (discovery, on the other hand, keeps its `Content-Length` — as with Google).
/// This is what makes the whole login suite exercise the Core's de-chunker,
/// and not only its unit tests.
fn chunked_json(value: Value) -> axum::response::Response {
    let text = value.to_string();
    // Two parts: a single-chunk body would not prove that the
    // reader chains chunks together.
    let bytes = text.into_bytes();
    let middle = bytes.len() / 2;
    let parts = [&bytes[..middle], &bytes[middle..]]
        .map(|part| Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(part)));
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/json")
        .body(axum::body::Body::from_stream(futures_util::stream::iter(
            parts,
        )))
        .expect("chunked response")
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    hex::encode(buf)
}

// ---------------------------------------------------------------------------
// Mini HTTP browser: run the OIDC flow (authorize → redirect to the Core's
// loopback) as the user's browser would. http only, like all the test traffic.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    /// Value of a header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The decoded query-string parameters of a URL.
pub fn url_params(url: &str) -> HashMap<String, String> {
    match url.split_once('?') {
        Some((_, query)) => form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect(),
        None => HashMap::new(),
    }
}

/// GET, without following redirects. `Err` if the connection fails — a
/// vanished loopback listener (flow replaced or consumed) is tested this way.
pub async fn http_get(url: &str) -> Result<HttpResponse, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("non-http URL: {url}"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let mut stream = tokio::net::TcpStream::connect(authority)
        .await
        .map_err(|e| format!("connection to {authority}: {e}"))?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("HTTP write: {e}"))?;
    let mut raw = Vec::new();
    timeout(RESPONSE_TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .map_err(|_| "HTTP timeout".to_string())?
        .map_err(|e| format!("HTTP read: {e}"))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("HTTP response without headers: {text:?}"))?;
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty HTTP response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("unreadable status line: {status_line}"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok(HttpResponse {
        status,
        headers,
        body: body.to_string(),
    })
}

/// Follows redirects like a browser (5 max) → final response.
pub async fn browse(url: &str) -> Result<HttpResponse, String> {
    let mut url = url.to_string();
    for _ in 0..5 {
        let response = http_get(&url).await?;
        match response.status {
            301 | 302 | 303 | 307 | 308 => {
                url = response
                    .header("location")
                    .ok_or("redirect without Location")?
                    .to_string();
            }
            _ => return Ok(response),
        }
    }
    Err(format!("too many redirects (last: {url})"))
}

// ---------------------------------------------------------------------------
// Device key (= iroh identity: Ed25519).
// ---------------------------------------------------------------------------

pub struct DeviceKey {
    key: SigningKey,
}

impl DeviceKey {
    pub fn generate() -> DeviceKey {
        DeviceKey {
            key: SigningKey::generate(&mut rand::rng()),
        }
    }

    /// Rebuilds the key from a hex seed (64 characters) — the format of the
    /// Core's `device.key` file.
    pub fn from_seed_hex(seed_hex: &str) -> DeviceKey {
        let bytes: [u8; 32] = hex::decode(seed_hex)
            .expect("seed hex")
            .try_into()
            .expect("32-byte seed");
        DeviceKey {
            key: SigningKey::from_bytes(&bytes),
        }
    }

    /// Private seed in hex (64 characters) — to write a `device.key`.
    pub fn seed_hex(&self) -> String {
        hex::encode(self.key.to_bytes())
    }

    /// Public key in hex (64 characters).
    pub fn node_id(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    /// Signature of the nonce (UTF-8 bytes), in hex.
    pub fn proof(&self, nonce: &str) -> String {
        hex::encode(self.key.sign(nonce.as_bytes()).to_bytes())
    }
}

// ---------------------------------------------------------------------------
// Test connection: JSON-RPC over WebSocket, buffered notifications.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RpcError {
    /// JSON-RPC code (`error.code`).
    pub code: i64,
    pub message: String,
    /// Application code (`error.data.code`).
    pub data_code: Option<String>,
}

impl RpcError {
    /// Application code, panics if it is absent.
    pub fn app_code(&self) -> &str {
        self.data_code
            .as_deref()
            .unwrap_or_else(|| panic!("no application code in the error: {self:?}"))
    }
}

pub struct TestConn {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    notifications: VecDeque<(String, Value)>,
}

impl TestConn {
    pub async fn connect(ws_url: &str) -> TestConn {
        let (ws, _) = connect_async(ws_url).await.expect("WS connection");
        TestConn {
            ws,
            next_id: 0,
            notifications: VecDeque::new(),
        }
    }

    /// Sends a JSON-RPC request and awaits its response. Notifications
    /// received in the meantime are buffered.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.ws
            .send(Message::text(msg.to_string()))
            .await
            .expect("WS send");

        timeout(RESPONSE_TIMEOUT, async {
            loop {
                let v = self.recv_json().await;
                if v.get("method").is_some() {
                    self.buffer_notification(v);
                } else if v.get("id") == Some(&json!(id)) {
                    return parse_response(v);
                } else {
                    panic!("response for an unexpected id: {v}");
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for the response to {method}"))
    }

    /// Next notification (buffered or upcoming) → `(method, params)`.
    pub async fn notification(&mut self) -> (String, Value) {
        if let Some(n) = self.notifications.pop_front() {
            return n;
        }
        timeout(RESPONSE_TIMEOUT, async {
            let v = self.recv_json().await;
            assert!(
                v.get("method").is_some(),
                "unexpected response while waiting for a notification: {v}"
            );
            split_notification(v)
        })
        .await
        .expect("timeout waiting for a notification")
    }

    /// The next notification MUST be `method`; returns its params.
    pub async fn expect_notification(&mut self, method: &str) -> Value {
        let (m, params) = self.notification().await;
        assert_eq!(m, method, "unexpected notification (params: {params})");
        params
    }

    /// Waits for a `method` notification, ignoring the others.
    pub async fn wait_notification(&mut self, method: &str) -> Value {
        loop {
            let (m, params) = self.notification().await;
            if m == method {
                return params;
            }
        }
    }

    /// Checks that no notification arrives during `SILENCE_WINDOW`.
    pub async fn assert_silent(&mut self) {
        if let Some((m, p)) = self.notifications.front() {
            panic!("unexpected notification in buffer: {m} {p}");
        }
        match timeout(SILENCE_WINDOW, self.recv_json()).await {
            Err(_) => {}
            Ok(v) => panic!("unexpected notification: {v}"),
        }
    }

    /// Empties the buffer and absorbs what arrives during `SILENCE_WINDOW`
    /// (to use after a multi-device setup to start fresh).
    pub async fn drain(&mut self) {
        self.notifications.clear();
        while timeout(SILENCE_WINDOW, self.ws.next()).await.is_ok() {}
    }

    /// Awaits the connection close → `Some((code, reason))` if a close frame
    /// was received, `None` if the stream ended without one.
    pub async fn expect_close(&mut self) -> Option<(u16, String)> {
        timeout(RESPONSE_TIMEOUT, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Close(frame))) => {
                        return frame.map(|f| (u16::from(f.code), f.reason.as_str().to_owned()));
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return None,
                }
            }
        })
        .await
        .expect("timeout waiting for the close")
    }

    /// Like `expect_close`, but requires that NO text message arrive before
    /// the close frame (spec: a revoked device is not notified by a message).
    pub async fn expect_close_silent(&mut self) -> Option<(u16, String)> {
        if let Some((m, p)) = self.notifications.front() {
            panic!("message received before close: {m} {p}");
        }
        timeout(RESPONSE_TIMEOUT, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Close(frame))) => {
                        return frame.map(|f| (u16::from(f.code), f.reason.as_str().to_owned()));
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(other)) => panic!("message received before close: {other:?}"),
                    Some(Err(_)) | None => return None,
                }
            }
        })
        .await
        .expect("timeout waiting for the close")
    }

    /// Sends a raw text frame (protocol conformance tests).
    pub async fn send_raw(&mut self, text: &str) {
        self.ws.send(Message::text(text)).await.expect("WS send");
    }

    /// Next raw JSON message (response OR notification), pings ignored.
    pub async fn recv_raw_json(&mut self) -> Value {
        timeout(RESPONSE_TIMEOUT, self.recv_json())
            .await
            .expect("timeout waiting for a message")
    }

    /// Next WebSocket frame, unfiltered (to observe the pings).
    pub async fn recv_frame(&mut self) -> Message {
        timeout(RESPONSE_TIMEOUT, self.ws.next())
            .await
            .expect("timeout waiting for a frame")
            .expect("WS stream ended")
            .expect("WS error")
    }

    fn buffer_notification(&mut self, v: Value) {
        assert!(
            v.get("id").is_none_or(Value::is_null),
            "a notification must not have an id: {v}"
        );
        self.notifications.push_back(split_notification(v));
    }

    async fn recv_json(&mut self) -> Value {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    return serde_json::from_str(&t).expect("invalid JSON");
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(f))) => panic!("connection closed: {f:?}"),
                Some(Ok(other)) => panic!("unexpected frame: {other:?}"),
                Some(Err(e)) => panic!("WS error: {e}"),
                None => panic!("WS stream ended"),
            }
        }
    }
}

fn split_notification(v: Value) -> (String, Value) {
    let method = v["method"].as_str().expect("method").to_string();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    (method, params)
}

fn parse_response(v: Value) -> Result<Value, RpcError> {
    assert_eq!(v["jsonrpc"], "2.0", "response not JSON-RPC 2.0: {v}");
    if let Some(err) = v.get("error") {
        Err(RpcError {
            code: err["code"].as_i64().expect("error.code"),
            message: err["message"].as_str().unwrap_or_default().to_string(),
            data_code: err
                .pointer("/data/code")
                .and_then(Value::as_str)
                .map(String::from),
        })
    } else {
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// Flows: ready-to-use enrollment and authentication, parameterized by the
// server's WebSocket URL.
// ---------------------------------------------------------------------------

/// A test device: its connection (open), its key and its id.
pub struct Device {
    pub conn: TestConn,
    pub key: DeviceKey,
    pub device_id: String,
}

/// `auth.challenge` → nonce.
pub async fn challenge(conn: &mut TestConn) -> String {
    conn.request("auth.challenge", json!({}))
        .await
        .expect("auth.challenge")["nonce"]
        .as_str()
        .expect("nonce")
        .to_string()
}

/// Enrolls `key` as a device of `sub`, on `conn`. The connection is NOT
/// authenticated on return (you still need `authenticate`) → device_id.
pub async fn enroll_key(
    conn: &mut TestConn,
    oidc: &FakeOidc,
    key: &DeviceKey,
    sub: &str,
    name: &str,
    platform: &str,
) -> String {
    let nonce = challenge(conn).await;
    let result = conn
        .request(
            "auth.enroll",
            json!({
                "id_token": oidc.id_token(sub),
                "node_id": key.node_id(),
                "name": name,
                "platform": platform,
                "proof": key.proof(&nonce),
            }),
        )
        .await
        .expect("auth.enroll");
    result["device_id"].as_str().expect("device_id").to_string()
}

/// Connects + enrolls a fresh device under `sub`. The connection is NOT
/// authenticated on return.
pub async fn enroll_device_at(
    ws_url: &str,
    oidc: &FakeOidc,
    sub: &str,
    name: &str,
    platform: &str,
) -> Device {
    let mut conn = TestConn::connect(ws_url).await;
    let key = DeviceKey::generate();
    let device_id = enroll_key(&mut conn, oidc, &key, sub, name, platform).await;
    Device {
        conn,
        key,
        device_id,
    }
}

/// `auth.challenge` + `auth.authenticate` on `conn` → device record.
pub async fn authenticate(conn: &mut TestConn, key: &DeviceKey, device_id: &str) -> Value {
    let nonce = challenge(conn).await;
    let result = conn
        .request(
            "auth.authenticate",
            json!({ "device_id": device_id, "proof": key.proof(&nonce) }),
        )
        .await
        .expect("auth.authenticate");
    result["device"].clone()
}

/// Checks that a value is an RFC 3339 UTC timestamp
/// ("2026-07-09T15:04:05Z", fractional seconds allowed).
pub fn assert_rfc3339(v: &Value) -> &str {
    let s = v
        .as_str()
        .unwrap_or_else(|| panic!("non-textual timestamp: {v}"));
    let b = s.as_bytes();
    let ok = b.len() >= 20
        && s[..4].bytes().all(|c| c.is_ascii_digit())
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && s.ends_with('Z');
    assert!(ok, "timestamp not RFC 3339 UTC: {s}");
    s
}

/// Finds a device by id in a `devices.list` result.
pub fn find_device<'a>(list: &'a Value, device_id: &str) -> &'a Value {
    list.as_array()
        .expect("device list")
        .iter()
        .find(|d| d["device_id"] == device_id)
        .unwrap_or_else(|| panic!("device {device_id} absent from the list: {list}"))
}
