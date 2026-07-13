// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The daemon's connector: what the Core lib cannot carry.
//!
//! rustls with the `ring` provider — and not `aws-lc-rs`, which requires cmake
//! and NASM on windows-msvc. The trust roots come from the OS store via
//! `rustls-platform-verifier`: on Windows and macOS it is the system verifier
//! (so an enterprise root or a root added by the user is honored); on Linux,
//! for lack of a system API, they are the CA bundle certificates
//! (`SSL_CERT_FILE`/`SSL_CERT_DIR` included).

use std::sync::Arc;

use tokio_rustls::rustls::pki_types::ServerName;
use universallink_core::{Connecting, Connector, IoStream, Target};

/// Opens in plaintext or in TLS depending on what the URL scheme required.
pub struct TlsConnector {
    inner: tokio_rustls::TlsConnector,
}

// The Core's `Config` derives `Debug`, so `Connector` requires it. The rustls
// config has nothing to show.
impl std::fmt::Debug for TlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsConnector(rustls/ring, OS roots)")
    }
}

impl TlsConnector {
    pub fn new() -> anyhow::Result<TlsConnector> {
        // `with_platform_verifier` relies on the process's default crypto
        // provider. Without this installation, it would fail at runtime, on
        // the first connection — that is, too late. A failure here means
        // "already installed", which suits us.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let config = <tokio_rustls::rustls::ClientConfig as rustls_platform_verifier::ConfigVerifierExt>::with_platform_verifier()?;
        Ok(TlsConnector {
            inner: tokio_rustls::TlsConnector::from(Arc::new(config)),
        })
    }
}

impl Connector for TlsConnector {
    fn connect<'a>(&'a self, target: &'a Target) -> Connecting<'a> {
        Box::pin(async move {
            let tcp = tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_plain_target_is_not_wrapped() {
        // The daemon's connector also serves plaintext targets: development (a
        // local server on `ws://`) must remain possible without switching
        // connectors.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let connector = TlsConnector::new().expect("connector");
        let target = Target {
            host: "127.0.0.1".into(),
            port,
            tls: false,
        };
        connector
            .connect(&target)
            .await
            .expect("a plaintext target opens without TLS");
    }

    #[tokio::test]
    async fn a_tls_target_refuses_a_server_that_speaks_plaintext() {
        // No one speaks TLS on the other side: the handshake must fail, not
        // fall back to plaintext.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // A plaintext HTTP response: exactly what a misconfigured
                // server would return.
                let mut stream = stream;
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });

        let connector = TlsConnector::new().expect("connector");
        let target = Target {
            host: "localhost".into(),
            port,
            tls: true,
        };
        assert!(
            connector.connect(&target).await.is_err(),
            "a peer that does not speak TLS must make the connection fail"
        );
    }

    #[test]
    fn the_crypto_provider_survives_a_second_connector() {
        // `install_default` fails on the second call; it must not make the
        // construction fail.
        TlsConnector::new().expect("first");
        TlsConnector::new().expect("second");
    }
}
