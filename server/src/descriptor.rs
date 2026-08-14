// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Deployment descriptor: `GET /.well-known/1device.json`.
//!
//! What a brand-new client needs in order to log in belongs to the
//! **deployment**, not to the user: the IdP and the OIDC client are the same for
//! every device of one server. Until this endpoint each device had them typed in
//! by hand on its first-run screen — three fields, one of them secret-shaped,
//! retyped on every machine and every phone. The server already holds the issuer
//! and the `client_id` (it validates ID tokens with them), so it hands them out
//! and the setup screen is down to one question: the server's address.
//!
//! Unauthenticated, necessarily: it is read *before* any login — it is what
//! makes the login possible. What that exposes is argued on [`body`].

use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::OidcConfig;
use crate::state::AppState;

/// Route path. A `/.well-known/` URI because the input is a domain name and the
/// question is "what is deployed here" (RFC 8615). The name is not registered
/// with IANA; the risk that answers to is a collision on a shared domain, and a
/// deployment's domain serves this control plane alone.
pub const PATH: &str = "/.well-known/1device.json";

/// The descriptor as served.
///
/// It does **not** carry the server's own URL. The client reached this endpoint
/// by that address, so it already knows it — and derives `wss://<host>/ws`, the
/// path being fixed by this API. A server behind a reverse proxy has no reliable
/// idea of its public origin anyway (it listens in cleartext on an internal
/// address), and a URL it dictated would be a redirect it controls.
///
/// `oidc_client_secret` is `null` when the IdP wants none. Serving it in the
/// clear is deliberate, and is not a leak: for an *installed application* OAuth
/// client the secret is not confidential. Google's own OAuth 2.0 documentation,
/// under "Installed applications", says to embed it in the source code of the
/// app and that "in this context, the client secret is obviously not treated as
/// a secret" — its Android and iOS client types are not even issued one, and
/// what protects the exchange is PKCE plus the loopback redirect (RFC 8252).
/// Published clients do exactly that: Thunderbird and rclone ship theirs inside
/// the binary. Handing it out here is the same exposure as putting it in every
/// installer, with one improvement — each deployment keeps its own client
/// instead of every user of the project sharing one.
///
/// So the most a reader of this endpoint gains is the ability to pose as this
/// deployment's OAuth client on its IdP's own consent screen. It grants nothing
/// on an account, the directory, or a device: those are behind the ID token and
/// the device key.
fn body(oidc: &OidcConfig) -> Value {
    json!({
        "api_version": crate::API_VERSION,
        "oidc_issuer": oidc.issuer_url,
        "oidc_client_id": oidc.client_id,
        "oidc_client_secret": oidc.client_secret,
    })
}

pub async fn get(State(state): State<Arc<AppState>>) -> Response {
    // `no-store`: a device reads this once, while being set up. Nothing gains
    // from keeping a copy — and a cached one is a way to keep configuring
    // clients with a `client_id` the deployment has since rotated.
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body(&state.config.oidc)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn oidc(client_secret: Option<&str>) -> OidcConfig {
        OidcConfig {
            issuer_url: "https://accounts.google.com".into(),
            client_id: "abc.apps.googleusercontent.com".into(),
            client_secret: client_secret.map(str::to_string),
            max_fresh_token_age: Duration::from_secs(300),
            jwks_refresh_min_interval: Duration::from_secs(60),
        }
    }

    #[test]
    fn it_hands_out_the_idp_the_client_and_the_api_version() {
        let v = body(&oidc(Some("GOCSPX-secret")));

        assert_eq!(v["oidc_issuer"], "https://accounts.google.com");
        assert_eq!(v["oidc_client_id"], "abc.apps.googleusercontent.com");
        assert_eq!(v["oidc_client_secret"], "GOCSPX-secret");
        // The same number `auth.enroll` answers: a client learns before
        // enrolling whether this server speaks its protocol.
        assert_eq!(v["api_version"], crate::API_VERSION);
    }

    #[test]
    fn an_idp_that_wants_no_secret_advertises_null() {
        let v = body(&oidc(None));

        // Present and null, not absent: curl the endpoint and the shape tells
        // you this deployment has no secret, rather than leaving you to wonder
        // whether the server just forgot to mention it.
        assert_eq!(v["oidc_client_secret"], Value::Null);
        assert!(
            v.as_object()
                .expect("an object")
                .contains_key("oidc_client_secret"),
            "the key must be there even when unset: {v}"
        );
    }

    /// Guards the decision documented on `body`: the address is the client's,
    /// not the server's to dictate. A field added here later would silently make
    /// the server able to point its own clients elsewhere.
    #[test]
    fn it_does_not_dictate_where_to_connect() {
        let v = body(&oidc(None));

        // Sorted, because whether `serde_json` keeps insertion order is a
        // feature of a dependency and not something this shape depends on.
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "api_version",
                "oidc_client_id",
                "oidc_client_secret",
                "oidc_issuer"
            ],
            "unexpected descriptor shape"
        );
    }
}
