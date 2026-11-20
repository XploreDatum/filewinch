//! TLS trust: self-signed certificates are refused with a HostTrust error carrying the
//! fingerprint, accepted once pinned, and refused again when the certificate changes.

mod common;

use std::io::BufReader;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use filewinch_core::ErrorKind;
use filewinch_http::{Http, trust};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

fn self_signed() -> (Vec<u8>, Arc<ServerConfig>) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let der = ck.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(der.clone())], key)
        .unwrap();
    (der, Arc::new(config))
}

/// HTTPS server whose certificate can be swapped between connections.
fn tls_server(config: Arc<Mutex<Arc<ServerConfig>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let cfg = config.lock().unwrap().clone();
            std::thread::spawn(move || {
                let conn = ServerConnection::new(cfg).unwrap();
                let mut tls = BufReader::new(StreamOwned::new(conn, tcp));
                if common::read_request(&mut tls).is_ok() {
                    common::respond(tls.get_mut(), "200 OK", "", b"hello");
                }
            });
        }
    });
    port
}

fn get(http: &Http, port: u16) -> filewinch_core::Result<String> {
    let url = format!("https://localhost:{port}/");
    let mut resp = http
        .agent()
        .get(&url)
        .call()
        .map_err(|e| http.transport_error(e, "localhost"))?;
    Ok(resp.body_mut().read_to_string().unwrap())
}

fn assert_host_trust(err: filewinch_core::Error, der: &[u8]) {
    assert_eq!(err.kind, ErrorKind::HostTrust, "{err}");
    assert_eq!(err.message, "The certificate for localhost isn't trusted.");
    let detail = err.detail.unwrap();
    let expected = format!("{}; ", trust::fingerprint(der));
    assert!(detail.starts_with(&expected), "{detail} vs {expected}");
    assert!(
        detail.len() > expected.len(),
        "detail should include the verifier's reason"
    );
}

#[test]
fn site_trusted_fingerprints_let_one_client_through() {
    let (der, cfg) = self_signed();
    let port = tls_server(Arc::new(Mutex::new(cfg)));
    let fp = trust::fingerprint(&der);
    // The hex form a browser shows is as good as the canonical one.
    let digest = ring::digest::digest(&ring::digest::SHA256, &der);
    let hex: Vec<String> = digest.as_ref().iter().map(|b| format!("{b:02X}")).collect();
    let site_trusting = Http::new();
    site_trusting.trust_fingerprints(&[format!("sha256:{}", hex.join(":"))]);
    assert_eq!(get(&site_trusting, port).unwrap(), "hello");
    // Another client (another site) isn't affected, and nothing was pinned process-wide.
    assert_host_trust(get(&Http::new(), port).unwrap_err(), &der);
    assert!(!trust::is_trusted("localhost", port, &fp));
    let mut site = filewinch_core::Site::new("s", "s", filewinch_core::Protocol::WebDavs);
    site.trusted_fingerprints = vec![fp];
    assert_eq!(get(&Http::for_site(&site), port).unwrap(), "hello");
}

#[test]
fn self_signed_is_refused_then_pinned_then_refused_when_changed() {
    let (der1, cfg1) = self_signed();
    let current = Arc::new(Mutex::new(cfg1));
    let port = tls_server(current.clone());
    let http = Http::new();

    assert_host_trust(get(&http, port).unwrap_err(), &der1);

    let fp = trust::fingerprint(&der1);
    trust::trust_fingerprint("localhost", port, &fp.replace("SHA256:", "sha256:"));
    assert_eq!(get(&http, port).unwrap(), "hello");
    // A pin is per host:port.
    assert!(!trust::is_trusted("localhost", port.wrapping_add(1), &fp));

    let (der2, cfg2) = self_signed();
    *current.lock().unwrap() = cfg2;
    assert_host_trust(get(&http, port).unwrap_err(), &der2);

    // Pinning the new certificate too (the user accepted the change) lets it through;
    // untrusting the host refuses it again.
    trust::trust_fingerprint("localhost", port, &trust::fingerprint(&der2));
    assert_eq!(get(&http, port).unwrap(), "hello");
    trust::untrust("localhost", port);
    assert!(!trust::is_trusted("localhost", port, &fp));
    assert_host_trust(get(&http, port).unwrap_err(), &der2);
}

#[test]
fn fingerprint_forms_are_equivalent() {
    let fp = trust::fingerprint(b"not really a certificate");
    assert!(fp.starts_with("SHA256:") && fp.len() == 7 + 43, "{fp}");
    let digest = ring::digest::digest(&ring::digest::SHA256, b"not really a certificate");
    let hex: Vec<String> = digest.as_ref().iter().map(|b| format!("{b:02X}")).collect();
    let hex = hex.join(":");
    trust::trust_fingerprint("Example.com", 8443, &hex.to_lowercase().replace(':', ""));
    assert!(trust::is_trusted("example.com", 8443, &fp));
    assert!(trust::is_trusted("example.com", 8443, &hex));
    assert!(trust::is_trusted(
        "example.com",
        8443,
        &format!("SHA256:{hex}")
    ));
    assert!(!trust::is_trusted("example.com", 443, &fp));
    trust::trust_fingerprint("bad.example", 443, "sha256:nothex");
    assert!(!trust::is_trusted("bad.example", 443, "sha256:nothex"));
    trust::untrust("example.com", 8443);
    assert!(!trust::is_trusted("example.com", 8443, &fp));
}
