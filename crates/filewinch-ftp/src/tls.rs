//! TLS for FTPS: the site's own trusted fingerprints (`Site::trusted_fingerprints`), then the
//! system trust roots, then certificates trusted through [`trust_certificate`] (stored in
//! `trusted_certs` in Filewinch's config folder — see `filewinch_os::dirs` — a fallback for
//! callers without a saved site).

use base64::Engine as _;
use filewinch_core::{Error, ErrorKind, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

static OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where in-app trusted certificate fingerprints live (one `host:port SHA256:...` per line).
pub fn trusted_certs_path() -> PathBuf {
    if let Some(p) = OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return p;
    }
    filewinch_os::dirs::Dirs::current().map(|d| d.config).unwrap_or_default().join("trusted_certs")
}

/// Points the trusted-certificate file elsewhere (tests; a sandbox container).
pub fn set_trusted_certs_path(path: Option<PathBuf>) {
    *OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = path;
}

/// `SHA256:` + unpadded base64 of the SHA-256 of the DER certificate (OpenSSH style).
pub fn fingerprint(der: &[u8]) -> String {
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(der)))
}

fn label(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

fn trusted_for(host: &str, port: u16) -> Option<String> {
    let text = std::fs::read_to_string(trusted_certs_path()).ok()?;
    let want = label(host, port);
    text.lines().filter_map(|l| l.split_once(' ')).find(|(h, _)| *h == want).map(|(_, fp)| fp.trim().to_string())
}

/// Trusts the certificate with `fingerprint` (the `detail` of the HostTrust error) for
/// `host:port`, replacing any earlier one.
pub fn trust_certificate(host: &str, port: u16, fingerprint: &str) -> Result<()> {
    // Accept the whole `detail` ("SHA256:…; reason") or any form core can normalise.
    let raw = fingerprint.split(';').next().unwrap_or_default();
    let Some(fingerprint) = filewinch_core::normalize_fingerprint(raw) else {
        return Err(Error::new(ErrorKind::Other, "That isn't a certificate fingerprint."));
    };
    let path = trusted_certs_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(Error::from)?;
    }
    let want = label(host, port);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().filter(|l| l.split(' ').next() != Some(want.as_str())).map(String::from).collect();
    lines.push(format!("{want} {fingerprint}"));
    std::fs::write(&path, lines.join("\n") + "\n").map_err(|e| Error::from(e).with_detail(path.display()))
}

fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())).clone()
}

fn webpki() -> Option<Arc<WebPkiServerVerifier>> {
    static V: OnceLock<Option<Arc<WebPkiServerVerifier>>> = OnceLock::new();
    V.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        for c in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(c);
        }
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build().ok()
    })
    .clone()
}

#[derive(Debug)]
pub(crate) struct Verifier {
    host: String,
    port: u16,
    trusted: Vec<String>,
    /// Set when the certificate wasn't trusted, with the error to show.
    pub verdict: Verdict,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity);
        if filewinch_core::is_trusted(&self.trusted, &fp) {
            return Ok(ServerCertVerified::assertion());
        }
        let pinned = trusted_for(&self.host, self.port);
        if pinned.as_deref().is_some_and(|p| filewinch_core::is_trusted(&[p.to_string()], &fp)) {
            return Ok(ServerCertVerified::assertion());
        }
        let system = match webpki() {
            Some(v) => v.verify_server_cert(end_entity, intermediates, server_name, ocsp, now),
            None => Err(rustls::Error::General("no system trust roots".into())),
        };
        match system {
            Ok(v) => Ok(v),
            Err(e) => {
                let who = label(&self.host, self.port);
                let msg = if pinned.is_some() {
                    format!("The certificate of {who} has changed since you trusted it. Only trust the new one if you know why it changed.")
                } else {
                    format!("The certificate of {who} isn't signed by an authority this Mac trusts (it may be self-signed). Check that its fingerprint matches what the server's administrator gave you.")
                };
                *self.verdict.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(Error::host_trust(msg, &fp, &e.to_string()));
                Err(e)
            }
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider().signature_verification_algorithms.supported_schemes()
    }
}

/// Where a certificate trust failure lands during a handshake.
pub(crate) type Verdict = Arc<Mutex<Option<Error>>>;

/// A client config for one site, plus the slot where a trust failure lands.
pub(crate) fn client_config(host: &str, port: u16, trusted: &[String]) -> Result<(Arc<ClientConfig>, Verdict)> {
    let verdict = Arc::new(Mutex::new(None));
    let verifier = Arc::new(Verifier { host: host.to_string(), port, trusted: trusted.to_vec(), verdict: verdict.clone() });
    let cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::new(ErrorKind::Other, "Couldn't set up TLS.").with_detail(e))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok((Arc::new(cfg), verdict))
}
