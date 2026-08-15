// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Deployment discovery: what a device needs in order to log in, read from the
//! server instead of typed in by hand — `GET /.well-known/1device.json`
//! (doc/server-api.md, "Deployment descriptor").
//!
//! The IdP and the OIDC client are the same on every device of one server, so
//! the setup screen has one question left: the address. This module turns what
//! a human typed into the two URLs that follow from it, reads the descriptor
//! over the [`Connector`] seam (TLS in the real binary, cleartext loopback in
//! tests), and hands back settings that `config.json` can hold as they are.
//!
//! It lives in the Core rather than in each shell because there is one HTTP
//! client and one TLS story here, and every client — the three desktops and the
//! phone — talks to a Core.

use serde_json::Value;

use crate::connector::{Connector, parse_url};

/// Where the descriptor lives, fixed by the server API — as is `/ws`. A
/// deployment mounted under a path prefix is therefore not discoverable; the
/// reverse-proxy recipes in `deploy/` all serve a whole domain.
const DESCRIPTOR_PATH: &str = "/.well-known/1device.json";

/// The settings to write into `config.json`. Field for field what the setup
/// screen used to ask for.
pub(crate) struct Deployment {
    pub server_url: String,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: Option<String>,
}

/// Why discovery failed. The caller maps these onto IPC error codes, which is
/// what lets the interface tell "type the fields yourself" apart from "check the
/// address".
#[derive(Debug)]
pub(crate) enum DiscoverError {
    /// Not an address we can do anything with.
    BadUrl,
    /// Answered, but publishes no descriptor: a server older than this endpoint,
    /// or not a 1Device server at all.
    NoDescriptor,
    /// Could not be reached, or answered at the HTTP level in a way that leaves
    /// nothing to read.
    Unreachable(String),
    /// A descriptor that does not describe a deployment we could configure.
    Invalid(String),
}

/// The descriptor URL and the `server_url` that follow from what was typed.
/// `None` if nothing usable does.
pub(crate) fn addresses(input: &str) -> Option<(String, String)> {
    let input = input.trim();
    // No scheme means TLS. This fetch decides where the login goes and which
    // client id it uses, so the default has to be the protected one; cleartext
    // is only ever what the user explicitly asked for, and it is accepted
    // because a cleartext deployment (`ws://`) is accepted everywhere else.
    let (tls, rest) = match input.split_once("://") {
        None => (true, input),
        Some(("https" | "wss", rest)) => (true, rest),
        Some(("http" | "ws", rest)) => (false, rest),
        Some(_) => return None,
    };
    // Only the authority survives. Whatever path came with it — a trailing
    // slash, or the `/ws` of an address pasted straight out of `config.json` —
    // is not where the descriptor is.
    let authority = rest.split(['/', '?', '#']).next()?;

    let descriptor_url = format!(
        "{}://{authority}{DESCRIPTOR_PATH}",
        if tls { "https" } else { "http" }
    );
    // Validated by the one function that already knows the rules (empty
    // authority, unreadable port, userinfo that would corrupt a `Host` header,
    // IPv6 literals) rather than by a second opinion written here.
    parse_url(&descriptor_url)?;
    let server_url = format!("{}://{authority}/ws", if tls { "wss" } else { "ws" });
    Some((descriptor_url, server_url))
}

pub(crate) async fn discover(
    connector: &dyn Connector,
    input: &str,
) -> Result<Deployment, DiscoverError> {
    let (descriptor_url, server_url) = addresses(input).ok_or(DiscoverError::BadUrl)?;
    let (status, body) = crate::http::request(connector, &descriptor_url, None)
        .await
        .map_err(DiscoverError::Unreachable)?;
    match status {
        200 => parse(&body, server_url),
        404 => Err(DiscoverError::NoDescriptor),
        other => Err(DiscoverError::Unreachable(format!(
            "the server answered HTTP {other}"
        ))),
    }
}

/// Reads a descriptor. Errors name the field at fault and **never quote the
/// response**: the caller chooses the address, so a body echoed back would turn
/// the Core into a reader of whatever else answers on its network.
fn parse(body: &str, server_url: String) -> Result<Deployment, DiscoverError> {
    let descriptor: Value = serde_json::from_str(body)
        .map_err(|_| DiscoverError::Invalid("the answer is not JSON".to_string()))?;
    // Absent, null, empty and blank are one and the same thing here: nothing
    // usable was published.
    let field = |name: &str| {
        descriptor
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let missing = |name: &str| DiscoverError::Invalid(format!("{name} is missing"));

    let oidc_issuer = field("oidc_issuer").ok_or_else(|| missing("oidc_issuer"))?;
    // The same rule the daemon applies when it loads `config.json`: refuse it
    // here rather than propose a file that the next reload would reject.
    if !(oidc_issuer.starts_with("http://") || oidc_issuer.starts_with("https://")) {
        return Err(DiscoverError::Invalid(
            "oidc_issuer is not an http(s) URL".to_string(),
        ));
    }
    let oidc_client_id = field("oidc_client_id").ok_or_else(|| missing("oidc_client_id"))?;

    // `api_version` is deliberately ignored: nothing in the Core checks the
    // server's version today (it is read from `auth.enroll` and not acted upon),
    // and discovery is not the place to invent that policy. Tolerant JSON —
    // unknown fields are not an error.
    Ok(Deployment {
        server_url,
        oidc_issuer: oidc_issuer.to_string(),
        oidc_client_id: oidc_client_id.to_string(),
        oidc_client_secret: field("oidc_client_secret").map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(input: &str) -> (String, String) {
        addresses(input).unwrap_or_else(|| panic!("addresses({input:?})"))
    }

    #[test]
    fn a_bare_host_is_read_over_tls() {
        let (descriptor, server) = urls("1device.example.com");

        assert_eq!(
            descriptor,
            "https://1device.example.com/.well-known/1device.json"
        );
        assert_eq!(server, "wss://1device.example.com/ws");
    }

    #[test]
    fn the_four_schemes_map_onto_their_two_protocols() {
        for input in ["https://host", "wss://host"] {
            let (descriptor, server) = urls(input);
            assert!(descriptor.starts_with("https://host/"), "{input}");
            assert_eq!(server, "wss://host/ws", "{input}");
        }
        // Cleartext, when it was asked for explicitly: the daemon accepts a
        // `ws://` deployment, so discovery has to reach one.
        for input in ["http://host", "ws://host"] {
            let (descriptor, server) = urls(input);
            assert!(descriptor.starts_with("http://host/"), "{input}");
            assert_eq!(server, "ws://host/ws", "{input}");
        }
    }

    #[test]
    fn a_pasted_websocket_address_round_trips() {
        // What is already in a configured device's `config.json`, and therefore
        // what someone setting up the next device will paste.
        let (descriptor, server) = urls("wss://host:8443/ws");

        assert_eq!(descriptor, "https://host:8443/.well-known/1device.json");
        assert_eq!(server, "wss://host:8443/ws");
    }

    #[test]
    fn paths_queries_and_padding_are_dropped() {
        for input in [
            "  https://host/  ",
            "https://host/ws",
            "https://host/?x=1",
            "https://host#f",
        ] {
            let (descriptor, server) = urls(input);
            assert_eq!(
                descriptor, "https://host/.well-known/1device.json",
                "{input}"
            );
            assert_eq!(server, "wss://host/ws", "{input}");
        }
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets() {
        let (descriptor, server) = urls("[::1]:8080");

        assert_eq!(descriptor, "https://[::1]:8080/.well-known/1device.json");
        assert_eq!(server, "wss://[::1]:8080/ws");
    }

    #[test]
    fn what_cannot_be_an_address_is_refused() {
        for input in [
            "",
            "   ",
            "https://",
            "ftp://host",
            "file:///etc/passwd",
            // Userinfo would corrupt the `Host` header.
            "https://user@host",
            // A port that is not one.
            "https://host:notaport",
        ] {
            assert!(addresses(input).is_none(), "must be refused: {input:?}");
        }
    }

    fn read(body: &str) -> Result<Deployment, DiscoverError> {
        parse(body, "wss://host/ws".to_string())
    }

    fn reason(body: &str) -> String {
        match read(body) {
            Err(DiscoverError::Invalid(why)) => why,
            Err(_) => panic!("expected an invalid descriptor"),
            Ok(_) => panic!("expected a refusal: {body}"),
        }
    }

    #[test]
    fn a_descriptor_becomes_the_settings_to_write() {
        let d = read(
            r#"{"api_version":1,"oidc_issuer":"https://accounts.google.com",
                "oidc_client_id":"abc.apps.googleusercontent.com",
                "oidc_client_secret":"GOCSPX-secret"}"#,
        )
        .expect("a valid descriptor");

        assert_eq!(d.server_url, "wss://host/ws");
        assert_eq!(d.oidc_issuer, "https://accounts.google.com");
        assert_eq!(d.oidc_client_id, "abc.apps.googleusercontent.com");
        assert_eq!(d.oidc_client_secret.as_deref(), Some("GOCSPX-secret"));
    }

    #[test]
    fn null_absent_and_empty_secrets_all_mean_no_secret() {
        for secret in [
            r#""oidc_client_secret":null,"#,
            "",
            r#""oidc_client_secret":"  ","#,
        ] {
            let d = read(&format!(
                r#"{{{secret}"oidc_issuer":"https://idp.example","oidc_client_id":"id"}}"#
            ))
            .expect("a valid descriptor");
            assert_eq!(d.oidc_client_secret, None, "{secret:?}");
        }
    }

    #[test]
    fn a_descriptor_missing_what_a_login_needs_is_refused() {
        assert!(reason(r#"{"oidc_client_id":"id"}"#).contains("oidc_issuer"));
        assert!(reason(r#"{"oidc_issuer":"https://idp.example"}"#).contains("oidc_client_id"));
        // Present but empty is the same as absent — it would be written into
        // `config.json` and rejected at the next reload.
        assert!(
            reason(r#"{"oidc_issuer":"https://idp.example","oidc_client_id":"  "}"#)
                .contains("oidc_client_id")
        );
    }

    #[test]
    fn an_issuer_that_is_not_an_http_url_is_refused() {
        // The daemon enforces this when it reads `config.json`: catching it here
        // is what keeps the interface from proposing a file the Core will reject.
        let why = reason(r#"{"oidc_issuer":"accounts.google.com","oidc_client_id":"id"}"#);
        assert!(why.contains("oidc_issuer"), "{why}");
    }

    #[test]
    fn what_is_not_a_descriptor_is_refused_without_being_quoted() {
        // Someone typed the address of an ordinary website — or pointed the Core
        // at something on its own network. The reason must not carry what came
        // back.
        let why = reason("<html><body>secret-intranet-page</body></html>");
        assert!(!why.contains("secret-intranet-page"), "{why}");
        assert!(why.contains("not JSON"), "{why}");

        // Valid JSON, but not a descriptor.
        assert!(reason(r#"{"unrelated":true}"#).contains("oidc_issuer"));
        // JSON, but not even an object.
        assert!(reason("[1,2,3]").contains("oidc_issuer"));
    }
}
