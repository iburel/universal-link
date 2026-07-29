// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Deployment descriptor: what a client that knows nothing but the server's
//! address reads to configure itself (doc/server-api.md).
//!
//! Frozen here: the path, the field names — they are the ones the Core writes
//! into its `config.json`, so renaming one on either side breaks a first-run —
//! and the fact that it answers without any authentication.

use crate::support::*;
use serde_json::Value;

const PATH: &str = "/.well-known/universallink.json";

async fn descriptor(env: &TestEnv) -> (u16, String, Value) {
    let (status, headers, body) = env.http_get(PATH).await;
    let json = serde_json::from_str(&body).unwrap_or_else(|e| panic!("JSON body ({e}): {body}"));
    (status, headers, json)
}

#[tokio::test]
async fn it_hands_out_the_idp_configuration_without_authentication() {
    let env = TestEnv::start().await;

    // No WebSocket, no ID token, no device key: this is read BEFORE a login is
    // even possible, and it is what makes the login possible.
    let (status, headers, v) = descriptor(&env).await;

    assert_eq!(status, 200);
    assert!(
        headers.contains("content-type: application/json"),
        "expected JSON: {headers}"
    );
    assert_eq!(v["oidc_issuer"], env.oidc.issuer());
    assert_eq!(v["oidc_client_id"], TEST_CLIENT_ID);
    assert_eq!(v["api_version"], 1);
}

#[tokio::test]
async fn the_client_secret_is_advertised_when_the_deployment_has_one() {
    let env = TestEnv::start_with(|c| c.oidc.client_secret = Some("GOCSPX-secret".into())).await;

    let (_, _, v) = descriptor(&env).await;

    // Deliberate: for an installed-application OAuth client this is not a
    // confidential value (see `descriptor::body`), and it is the one field that
    // otherwise has to be carried to every device by hand.
    assert_eq!(v["oidc_client_secret"], "GOCSPX-secret");
}

#[tokio::test]
async fn an_idp_without_a_secret_advertises_null() {
    let env = TestEnv::start().await;

    let (_, _, v) = descriptor(&env).await;

    assert_eq!(v["oidc_client_secret"], Value::Null);
}

/// A device sets itself up once; a cached copy is how a client keeps being
/// configured with an OIDC client the deployment has already replaced.
#[tokio::test]
async fn it_is_not_cacheable() {
    let env = TestEnv::start().await;

    let (_, headers, _) = descriptor(&env).await;

    assert!(
        headers.contains("cache-control: no-store"),
        "expected no-store: {headers}"
    );
}
