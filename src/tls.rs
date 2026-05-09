//! Self-signed cert generation for the SMTP STARTTLS path.
//!
//! Each saehrimnir process generates a fresh ephemeral cert at startup
//! and wraps it in a `tokio_rustls::TlsAcceptor`. Clients must accept
//! invalid certificates - typical for test mocks. The cert subject
//! alt names cover `localhost` and `saehrimnir.test` so any local
//! hostname the test seeds will route correctly through TLS hostname
//! verification *if* the client opts to verify (it usually won't).

use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::TlsAcceptor;

/// Generate a self-signed cert and wrap it in a `TlsAcceptor` ready to
/// hand out to the SMTP listener. Installs the `ring` crypto provider
/// as the rustls default if one is not already installed.
pub fn make_acceptor() -> Result<TlsAcceptor, String> {
    // Idempotent: install_default returns Err if a provider is already
    // installed. We swallow that - the test suite spins many TLS
    // instances over the lifetime of one process.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let subjects = vec!["localhost".to_string(), "saehrimnir.test".to_string()];
    let certified = rcgen::generate_simple_self_signed(subjects)
        .map_err(|e| format!("rcgen: {e}"))?;

    let cert_der: CertificateDer<'static> =
        CertificateDer::from(certified.cert.der().to_vec());
    let key_der: PrivateKeyDer<'static> =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| format!("rustls: {e}"))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}
