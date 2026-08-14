// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The mobile shell's connector — the mobile analogue of `daemon/src/tls.rs`.
//!
//! Same rustls/ring stack as the daemon, but the trust roots are the bundled
//! Mozilla set (`webpki-roots`) instead of the OS store. On desktop the daemon
//! uses `rustls-platform-verifier` (the system trust store, so enterprise and
//! user-added roots are honored); on Android that verifier requires a JNI +
//! Kotlin bridge and the app `Context` threaded through the activity — heavy
//! plumbing for no practical gain here, since the Core only ever reaches
//! PUBLIC endpoints (the signaling server behind a public CA, the OIDC
//! provider). The bundled roots cover those, and this exact path was already
//! proven reaching the server on-device in the brick-1 spike.
//!
//! Trade-off vs the desktop verifier: roots added by the user or an enterprise
//! MDM on the device are NOT honored, and the root set is refreshed by bumping
//! the `webpki-roots` crate rather than by the OS.

use std::sync::Arc;
use std::time::Duration;

use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use onedevice_core::{Connecting, Connector, IoStream, Target};

/// Bounded retries around the TCP connect (DNS + handshake-less socket open).
/// Android's native resolver is prone to transient `getaddrinfo` failures
/// ("No address associated with hostname" / EAI_NODATA) — e.g. after the
/// process has been dozing in the background, the per-app resolver can return
/// nothing for a name that resolves fine moments later (and that `ping`
/// resolves throughout). A short retry turns those blips into a delay instead
/// of a failed login/connection. The desktop daemon has no such need.
const CONNECT_ATTEMPTS: u32 = 4;
const CONNECT_BACKOFF: [Duration; 3] = [
    Duration::from_millis(300),
    Duration::from_millis(600),
    Duration::from_millis(1200),
];

/// Opens plaintext or TLS depending on what the URL scheme required
/// (`Target::tls`), exactly like the daemon's `TlsConnector`.
pub struct WebPkiConnector {
    inner: tokio_rustls::TlsConnector,
}

// `Config` derives `Debug`, so `Connector` requires it; the rustls config has
// nothing to show.
impl std::fmt::Debug for WebPkiConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebPkiConnector(rustls/ring, bundled Mozilla roots)")
    }
}

impl WebPkiConnector {
    pub fn new() -> anyhow::Result<WebPkiConnector> {
        // `ClientConfig::builder` reads the process's default crypto provider;
        // install ring's first, exactly as the daemon does. A failure here
        // means "already installed", which suits us.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        Ok(WebPkiConnector {
            inner: tokio_rustls::TlsConnector::from(Arc::new(config)),
        })
    }
}

impl Connector for WebPkiConnector {
    fn connect<'a>(&'a self, target: &'a Target) -> Connecting<'a> {
        Box::pin(async move {
            let mut attempt = 0u32;
            let tcp = loop {
                match tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await {
                    Ok(stream) => break stream,
                    Err(e) => {
                        attempt += 1;
                        if attempt >= CONNECT_ATTEMPTS {
                            return Err(e);
                        }
                        tracing::warn!(
                            host = %target.host,
                            port = target.port,
                            attempt,
                            error = %e,
                            "TCP connect/DNS failed; retrying"
                        );
                        tokio::time::sleep(CONNECT_BACKOFF[(attempt - 1) as usize]).await;
                    }
                }
            };
            if !target.tls {
                return Ok(Box::new(tcp) as Box<dyn IoStream>);
            }
            // The name presented in the SNI and verified in the certificate is
            // the URL's, not that of the resolved address.
            let name = ServerName::try_from(target.host.clone())
                .map_err(|e| std::io::Error::other(format!("invalid server name: {e}")))?;
            let tls = self.inner.connect(name, tcp).await?;
            Ok(Box::new(tls) as Box<dyn IoStream>)
        })
    }
}
