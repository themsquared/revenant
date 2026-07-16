//! The mTLS A2A listener (SEC-4 P2) — the wire half of identity-pinned certs.
//!
//! Serves /a2a (and the agent card) over TLS with this agent's persistent
//! certificate, and REQUESTS a client certificate on every handshake. Client
//! certs are accepted regardless of who signed them (there is no CA — see
//! docs/DESIGN-MTLS.md), but the handshake still proves POSSESSION of the
//! presented cert's key, and the request handler binds the presented cert's
//! fingerprint to the A2A envelope's sender identity by checking it against
//! that identity's published, identity-signed pin. Wire ↔ identity, closed.
//!
//! Loopback :7717 keeps serving local surfaces in plain HTTP; this listener is
//! separate, off by default, and bound to loopback unless deliberately exposed.

use crate::{AppState, PeerCertFp};
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

/// Best-effort install of the ring provider as the process default; harmless
/// if another provider (e.g. reqwest's) got there first.
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Accept ANY client certificate content (no CA to chain to), while still
/// verifying the handshake signature — the peer must hold the private key of
/// whatever cert it presents. Identity binding happens at the request layer
/// against the published pin; this verifier's job is proof-of-possession.
#[derive(Debug)]
struct AcceptAnyClientCert {
    schemes: Vec<rustls::SignatureScheme>,
}

impl AcceptAnyClientCert {
    fn new() -> Self {
        let provider = rustls::crypto::CryptoProvider::get_default()
            .expect("crypto provider installed")
            .clone();
        AcceptAnyClientCert { schemes: provider.signature_verification_algorithms.supported_schemes() }
    }
}

impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false // pre-mTLS peers may connect; the pin check governs when it matters
    }

    fn verify_client_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let provider = rustls::crypto::CryptoProvider::get_default().expect("provider");
        rustls::crypto::verify_tls12_signature(message, cert, dss, &provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let provider = rustls::crypto::CryptoProvider::get_default().expect("provider");
        rustls::crypto::verify_tls13_signature(message, cert, dss, &provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes.clone()
    }
}

/// The minimal router this listener exposes: A2A + discovery. The bearer-authed
/// control surfaces stay on loopback only.
fn a2a_router(state: AppState) -> Router {
    Router::new()
        .route("/a2a", post(crate::a2a_message))
        .route("/.well-known/agent-card.json", get(crate::agent_card))
        .with_state(state)
}

/// Serve /a2a over TLS at `addr` with this agent's cert. Runs until the daemon
/// exits; individual connection failures are logged and dropped.
pub async fn serve_a2a_tls(
    state: AppState,
    addr: String,
    cert_pem: String,
    key_pem: String,
) -> anyhow::Result<()> {
    use anyhow::Context;
    ensure_crypto_provider();

    let certs: Vec<rustls::pki_types::CertificateDer> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes()).collect::<Result<_, _>>().context("parsing TLS cert")?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .context("parsing TLS key")?
        .context("no private key in PEM")?;
    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert::new()))
        .with_single_cert(certs, key)
        .context("building TLS server config")?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding mTLS A2A listener on {addr}"))?;
    tracing::info!("mTLS A2A listener on {addr} (identity-pinned cert; client certs bound to published pins)");
    let router = a2a_router(state);

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("a2a-tls accept failed: {e:#}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let tls = match acceptor.accept(tcp).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!("a2a-tls handshake with {peer} failed: {e}");
                    return;
                }
            };
            // The presented client cert's fingerprint, if any — the request
            // layer checks it against the envelope sender's published pin.
            let fp = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|cc| cc.first())
                .map(|c| revenant_net::tls::fingerprint_der(c));
            let svc = router.layer(axum::Extension(PeerCertFp(fp)));
            let io = hyper_util::rt::TokioIo::new(tls);
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, hyper_util::service::TowerToHyperService::new(svc))
                .await
            {
                tracing::debug!("a2a-tls connection from {peer} ended: {e}");
            }
        });
    }
}
