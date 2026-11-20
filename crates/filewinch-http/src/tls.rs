//! TLS for the ureq agent: rustls (ring) with the macOS platform verifier, falling back to
//! the site's trusted fingerprints ([`crate::Http::trust_fingerprints`]) and then the
//! process-wide pins in [`crate::trust`].
//!
//! ureq's own `RustlsConnector` can't take a custom verifier, so this module provides a
//! small replacement connector that sits after ureq's `TcpConnector` in the chain.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, RwLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use ureq::unversioned::transport::time::{Duration as UDuration, Instant as UInstant};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport,
    TransportAdapter,
};

use crate::trust;

/// Why the last certificate for a host was rejected, so `Http::transport_error` can build
/// the `HostTrust` detail. Keyed by bare host name (lowercase).
pub(crate) struct Rejection {
    pub fingerprint: String,
    pub reason: String,
}

#[derive(Default)]
pub(crate) struct Rejections(Mutex<HashMap<String, Rejection>>);

impl Rejections {
    fn record(&self, host: &str, rejection: Rejection) {
        self.lock().insert(host.to_ascii_lowercase(), rejection);
    }
    pub(crate) fn take(&self, host: &str) -> Option<Rejection> {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        self.lock().remove(&host)
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Rejection>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The site's "always trust" fingerprints, shared by an `Http` and its TLS connector.
pub(crate) type Trusted = Arc<RwLock<Vec<String>>>;

/// Platform verification first; if that fails, accept a pinned end-entity certificate.
struct PinningVerifier {
    platform: rustls_platform_verifier::Verifier,
    port: u16,
    rejections: Arc<Rejections>,
    trusted: Trusted,
}

impl fmt::Debug for PinningVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinningVerifier")
            .field("port", &self.port)
            .finish()
    }
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let err = match self.platform.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(ok) => return Ok(ok),
            Err(e) => e,
        };
        let host = server_name.to_str();
        let fingerprint = trust::fingerprint(end_entity.as_ref());
        let site_trusts = filewinch_core::is_trusted(
            &self.trusted.read().unwrap_or_else(|p| p.into_inner()),
            &fingerprint,
        );
        if site_trusts || trust::is_trusted(&host, self.port, &fingerprint) {
            return Ok(ServerCertVerified::assertion());
        }
        self.rejections.record(
            &host,
            Rejection {
                fingerprint,
                reason: err.to_string(),
            },
        );
        Err(err)
    }

    // Handshake signatures prove the server holds the certificate's key; that check is the
    // same whether the certificate is trusted by the platform or pinned, so delegate.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.platform.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.platform.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.platform.supported_verify_schemes()
    }
}

/// Wraps the chained TCP transport in TLS for `https` URLs.
pub(crate) struct TlsConnector {
    provider: Arc<CryptoProvider>,
    rejections: Arc<Rejections>,
    trusted: Trusted,
    /// One client config per port, because the verifier needs the port for `host:port`
    /// pins and rustls only hands it the server name.
    configs: Mutex<HashMap<u16, Arc<ClientConfig>>>,
}

impl TlsConnector {
    pub(crate) fn new(rejections: Arc<Rejections>, trusted: Trusted) -> Self {
        TlsConnector {
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            rejections,
            trusted,
            configs: Mutex::new(HashMap::new()),
        }
    }

    fn config(&self, port: u16) -> Result<Arc<ClientConfig>, ureq::Error> {
        let mut configs = self.configs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = configs.get(&port) {
            return Ok(c.clone());
        }
        let verifier = PinningVerifier {
            platform: rustls_platform_verifier::Verifier::new(self.provider.clone())
                .map_err(ureq::Error::Rustls)?,
            port,
            rejections: self.rejections.clone(),
            trusted: self.trusted.clone(),
        };
        let config = ClientConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(ureq::Error::Rustls)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        // Resumed sessions skip certificate verification, so an `untrust` (or a pin the
        // user revoked) would not take effect. Pooled connections keep handshakes rare.
        let mut config = config;
        config.resumption = rustls::client::Resumption::disabled();
        let config = Arc::new(config);
        configs.insert(port, config.clone());
        Ok(config)
    }
}

impl fmt::Debug for TlsConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConnector").finish()
    }
}

impl<In: Transport> Connector<In> for TlsConnector {
    type Out = Either<In, TlsTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            return Ok(None);
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }
        let authority = details
            .uri
            .authority()
            .ok_or(ureq::Error::Tls("missing host"))?;
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = authority.port_u16().unwrap_or(443);
        let name = ServerName::try_from(host.to_string())
            .map_err(|_| ureq::Error::Tls("invalid server name"))?;

        let mut conn =
            ClientConnection::new(self.config(port)?, name).map_err(ureq::Error::Rustls)?;
        let mut sock = TransportAdapter::new(transport.boxed(), details.current_time.clone());
        sock.set_timeout(remaining(details));
        conn.complete_io(&mut sock).map_err(handshake_error)?;

        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(Either::B(TlsTransport {
            buffers,
            stream: StreamOwned::new(conn, sock),
        })))
    }
}

/// The connect budget left after TCP setup (the budget counts from `details.now`).
fn remaining(details: &ConnectionDetails) -> NextTimeout {
    let mut timeout = details.timeout;
    if let (UDuration::Exact(budget), UInstant::Exact(start), UInstant::Exact(now)) =
        (timeout.after, details.now, (details.current_time)())
    {
        timeout.after =
            UDuration::Exact(budget.saturating_sub(now.saturating_duration_since(start)));
    }
    timeout
}

/// rustls reports handshake failures as io::Error(InvalidData, rustls::Error); surface them as
/// `ureq::Error::Rustls` so callers can tell TLS failures from network failures.
fn handshake_error(e: std::io::Error) -> ureq::Error {
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(tls) => ureq::Error::Rustls(tls.clone()),
        None => ureq::Error::from(e),
    }
}

pub(crate) struct TlsTransport {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, TransportAdapter>,
}

impl Transport for TlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        self.stream.flush()?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}

impl fmt::Debug for TlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTransport").finish()
    }
}
