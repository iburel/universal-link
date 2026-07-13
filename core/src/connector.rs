// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Transport seam: the library opens its outbound streams — WebSocket to the
//! server, HTTP to the IdP — through a `Connector` injected by the config,
//! exactly as it stows its secrets through a `SecretStore`.
//!
//! Why: TLS cannot live here. `core` is cross-checked lib-only from Linux
//! (`cargo check --target x86_64-pc-windows-msvc`), and no TLS stack
//! cross-compiles without a C compiler for the target. The daemon binary, by
//! contrast, is compiled natively: it is the one that wires in rustls.
//!
//! The URL scheme decides the encryption — `wss`/`https` require TLS,
//! `ws`/`http` forbid it. The library splits the URL once and for all
//! (`parse_url`) and passes the connector only a target; there is thus a single
//! source of truth for the authority and the default port.

use std::future::Future;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};

/// A bidirectional stream to a remote peer. A single `dyn` cannot combine two
/// non-auto traits (`AsyncRead` + `AsyncWrite`): hence this combined trait and
/// its blanket impl.
pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

/// Where to reach a peer, and under what protection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// The scheme required `wss`/`https`.
    pub tls: bool,
}

/// What a `Connector` yields: the stream, later. `async fn` cannot be used in
/// a trait object, hence the boxed future.
pub type Connecting<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<Box<dyn IoStream>>> + Send + 'a>>;

/// Opens the Core's outbound streams. `Debug` is mandatory: `Config` derives
/// it, and an `Arc<dyn Connector>` without it would not compile (same
/// constraint as `SecretStore`).
pub trait Connector: Send + Sync + std::fmt::Debug {
    fn connect<'a>(&'a self, target: &'a Target) -> Connecting<'a>;
}

/// Cleartext connections: all the library can do on its own. A TLS target is
/// REFUSED rather than served in the clear — a URL that promises encryption
/// must never be honored without it.
#[derive(Debug)]
pub struct PlainConnector;

impl Connector for PlainConnector {
    fn connect<'a>(&'a self, target: &'a Target) -> Connecting<'a> {
        Box::pin(async move {
            if target.tls {
                return Err(std::io::Error::other(
                    "TLS requested without a TLS connector wired in (the binary is the one that provides it)",
                ));
            }
            let stream =
                tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await?;
            Ok(Box::new(stream) as Box<dyn IoStream>)
        })
    }
}

/// A split URL: everything needed to open the stream AND write the `Host`
/// header.
pub(crate) struct Location {
    pub target: Target,
    /// `host` or `host:port`, as written in the URL — this is what the `Host`
    /// header must carry.
    pub authority: String,
    /// Path + query, never empty (`/` at minimum).
    pub path: String,
}

/// Splits `ws://`, `wss://`, `http://`, `https://`. `None`: unknown scheme,
/// empty authority, unreadable port, or userinfo (never used here, and it would
/// corrupt the `Host` header).
pub(crate) fn parse_url(url: &str) -> Option<Location> {
    let (scheme, rest) = url.split_once("://")?;
    let (tls, default_port) = match scheme {
        "ws" | "http" => (false, 80),
        "wss" | "https" => (true, 443),
        _ => return None,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let tail = &rest[end..];
    let path = if tail.is_empty() {
        "/".to_string()
    } else if tail.starts_with('/') {
        tail.to_string()
    } else {
        format!("/{tail}")
    };

    let (host, port) = match authority.strip_prefix('[') {
        // IPv6 literal: the port lives after the closing bracket.
        Some(inside) => {
            let (host, after) = inside.split_once(']')?;
            let port = match after {
                "" => default_port,
                p => p.strip_prefix(':')?.parse().ok()?,
            };
            (host.to_string(), port)
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port.parse().ok()?),
            None => (authority.to_string(), default_port),
        },
    };
    if host.is_empty() {
        return None;
    }
    Some(Location {
        target: Target { host, port, tls },
        authority: authority.to_string(),
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(url: &str) -> Location {
        parse_url(url).expect("parseable URL")
    }

    #[test]
    fn schemes_decide_tls_and_default_port() {
        let l = loc("ws://127.0.0.1:1234/ws");
        assert_eq!(
            l.target,
            Target {
                host: "127.0.0.1".into(),
                port: 1234,
                tls: false
            }
        );
        assert_eq!(l.authority, "127.0.0.1:1234");
        assert_eq!(l.path, "/ws");

        let l = loc("wss://relay.example/ws");
        assert_eq!(l.target.port, 443);
        assert!(l.target.tls);

        let l = loc("http://idp.example/.well-known/openid-configuration");
        assert_eq!(l.target.port, 80);
        assert!(!l.target.tls);

        assert_eq!(loc("https://idp.example").target.port, 443);
        // No path: `/`, never the empty string (an HTTP request without a
        // target is invalid).
        assert_eq!(loc("https://idp.example").path, "/");
        assert_eq!(loc("https://idp.example?a=b").path, "/?a=b");
    }

    #[test]
    fn ipv6_literals_keep_their_host() {
        let l = loc("ws://[::1]:9000/ws");
        assert_eq!(l.target.host, "::1");
        assert_eq!(l.target.port, 9000);
        // The Host header keeps the brackets.
        assert_eq!(l.authority, "[::1]:9000");
        assert_eq!(loc("wss://[::1]/ws").target.port, 443);
    }

    #[test]
    fn refuses_what_it_cannot_honour() {
        for url in [
            "ftp://h/x",       // unknown scheme
            "h/x",             // no scheme
            "http:///x",       // empty authority
            "http://user@h/x", // userinfo: would corrupt the Host header
            "http://h:port/x", // unreadable port
            "http://[::1/x",   // unclosed bracket
            "http://[::1]x/y", // junk suffix after the bracket
        ] {
            assert!(parse_url(url).is_none(), "{url} should have been refused");
        }
    }

    #[tokio::test]
    async fn plain_connector_refuses_tls_targets() {
        let target = Target {
            host: "idp.example".into(),
            port: 443,
            tls: true,
        };
        let Err(err) = PlainConnector.connect(&target).await else {
            panic!("a TLS target must be refused, never served in the clear");
        };
        assert!(err.to_string().contains("TLS"), "{err}");
    }
}
