//! TLS for the two client legs: the CLI's HTTP calls (ureq) and the relay's upstream WebSocket.
//!
//! Both are needed only when the broker sits behind a TLS front — a reverse proxy that serves
//! `https://` and forwards to the broker's plain port. The broker itself never speaks TLS.
//!
//! **Trust is the public web roots plus, optionally, one PEM file (`ca_file`).** A front with a
//! certificate from a private root, which is the usual case for a broker that only ever faces
//! the operator's own machines, is trusted by naming that root's PEM. The operating system's
//! certificate store is deliberately not consulted: one explicit file is the whole of what this
//! tool trusts beyond the public roots, on every OS, and a test can prove it.

use std::path::Path;
use std::sync::Arc;

/// The roots a client config trusts: the public web roots, plus every certificate in `ca_file`.
///
/// A file that is set but unreadable, or that holds no certificate, is an error. Silently trusting
/// nothing extra would look configured and fail later as a certificate error naming no file.
pub fn roots(ca_file: Option<&Path>) -> Result<rustls::RootCertStore, String> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::CertificateDer;

    let mut store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let Some(path) = ca_file else {
        return Ok(store);
    };
    let pem =
        std::fs::read(path).map_err(|e| format!("cannot read ca_file {}: {e}", path.display()))?;
    let mut added = 0;
    for cert in CertificateDer::pem_slice_iter(&pem) {
        let cert = cert.map_err(|e| format!("ca_file {} is not valid PEM: {e}", path.display()))?;
        store.add(cert).map_err(|e| {
            format!(
                "ca_file {} holds a certificate rustls rejects: {e}",
                path.display()
            )
        })?;
        added += 1;
    }
    if added == 0 {
        return Err(format!(
            "ca_file {} holds no certificate (expected PEM, \"-----BEGIN CERTIFICATE-----\")",
            path.display()
        ));
    }
    Ok(store)
}

/// One client config for both legs. The ring provider is named explicitly rather than taken from
/// a process default, which nothing in this binary installs.
pub fn client_config(ca_file: Option<&Path>) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .with_root_certificates(roots(ca_file)?)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// A hint to append when a connection failed because the server's certificate was not trusted.
///
/// rustls says `invalid peer certificate: UnknownIssuer`, which names neither the cause nor the fix.
pub fn hint(err: &str) -> Option<&'static str> {
    let untrusted = [
        "UnknownIssuer",
        "invalid peer certificate",
        "certificate verify failed",
    ];
    untrusted.iter().any(|m| err.contains(m)).then_some(
        "the broker's TLS certificate is not trusted. If its front uses a private root, set \
         ca_file in the config file (or --ca-file / AMB_CA_FILE) to that root's PEM",
    )
}

/// `err`, with the hint appended when there is one.
pub fn with_hint(err: String) -> String {
    match hint(&err) {
        Some(h) => format!("{err} ({h})"),
        None => err,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("amb-tls-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ca.pem");
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn a_root_pem() -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.self_signed(&key).unwrap().pem()
    }

    #[test]
    fn without_a_ca_file_only_the_public_roots_are_trusted() {
        assert_eq!(
            roots(None).unwrap().len(),
            webpki_roots::TLS_SERVER_ROOTS.len()
        );
    }

    /// The private root is added to the public roots, not swapped in for them.
    #[test]
    fn a_ca_file_adds_its_root_to_the_public_ones() {
        let p = temp("one", a_root_pem().as_bytes());
        assert_eq!(
            roots(Some(&p)).unwrap().len(),
            webpki_roots::TLS_SERVER_ROOTS.len() + 1
        );
    }

    /// A file that parses to nothing would otherwise trust nothing extra while looking configured,
    /// and the first sign would be a certificate error that names no file at all.
    #[test]
    fn a_ca_file_with_no_certificate_in_it_is_an_error_not_an_empty_trust() {
        let p = temp("empty", b"this is not a certificate\n");
        let e = roots(Some(&p)).unwrap_err();
        assert!(e.contains("no certificate"), "unexpected error: {e}");
        assert!(
            e.contains("ca.pem"),
            "the error does not name the file: {e}"
        );
    }

    #[test]
    fn a_missing_ca_file_is_an_error_naming_it() {
        let p = std::env::temp_dir()
            .join("amb-tls-does-not-exist")
            .join("ca.pem");
        let e = roots(Some(&p)).unwrap_err();
        assert!(
            e.contains("amb-tls-does-not-exist"),
            "the error does not name the file: {e}"
        );
    }

    /// rustls reports an untrusted certificate as `UnknownIssuer`, which says nothing about what to
    /// do. The hint names the setting that fixes it.
    #[test]
    fn an_untrusted_certificate_comes_with_the_setting_that_fixes_it() {
        let h = hint("invalid peer certificate: UnknownIssuer").expect("no hint");
        assert!(
            h.contains("ca_file"),
            "the hint does not name the setting: {h}"
        );
        assert!(
            hint("connection refused (os error 10061)").is_none(),
            "hint on a non-TLS error"
        );
    }
}
