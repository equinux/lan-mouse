//! Exact certificate pins replace public-PKI hostname validation for paired,
//! self-signed LAN identities. Rustls still verifies proof of private-key ownership.
use rustls::{
    DigitallySignedStruct, DistinguishedName, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use webrtc_dtls::crypto::Certificate;

#[derive(Debug)]
struct PinnedPeer {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

pub(super) fn parse_pin(value: &str) -> Result<[u8; 32], &'static str> {
    let components: Vec<_> = value.split(':').collect();
    if components.len() != 32 || components.iter().any(|s| s.len() != 2) {
        return Err("clipboard peer_fingerprint must be a colon-separated SHA-256 fingerprint");
    }
    let mut pin = [0; 32];
    for (byte, component) in pin.iter_mut().zip(components) {
        *byte = u8::from_str_radix(component, 16).map_err(|_| "invalid clipboard fingerprint")?;
    }
    Ok(pin)
}

pub(super) fn fingerprint(cert: &CertificateDer<'_>) -> [u8; 32] {
    Sha256::digest(cert.as_ref()).into()
}

impl PinnedPeer {
    fn check(&self, cert: &CertificateDer<'_>, chain: &[CertificateDer<'_>]) -> Result<(), Error> {
        if !chain.is_empty() || fingerprint(cert) != self.pin {
            return Err(Error::General(
                "clipboard peer certificate does not match its pin".into(),
            ));
        }
        Ok(())
    }
    fn signature13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn signature12(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
impl ServerCertVerifier for PinnedPeer {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.check(cert, chain)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        s: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.signature12(m, c, s)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        s: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.signature13(m, c, s)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}
impl ClientCertVerifier for PinnedPeer {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check(cert, chain)?;
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        s: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.signature12(m, c, s)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        s: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.signature13(m, c, s)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

pub(super) fn configs(
    cert: &Certificate,
    pin: [u8; 32],
) -> Result<(Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>), Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let peer = Arc::new(PinnedPeer {
        pin,
        provider: provider.clone(),
    });
    let key = || PrivatePkcs8KeyDer::from(cert.private_key.serialized_der.clone()).into();
    let mut client = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(peer.clone())
        .with_client_auth_cert(cert.certificate.clone(), key())?;
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(peer)
        .with_single_cert(cert.certificate.clone(), key())?;
    client.alpn_protocols = vec![b"lan-mouse-clipboard/1".to_vec()];
    server.alpn_protocols = client.alpn_protocols.clone();
    // Fresh authentication each connection; no 0-RTT or resumable sessions.
    client.resumption = rustls::client::Resumption::disabled();
    server.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    server.send_tls13_tickets = 0;
    Ok((Arc::new(client), Arc::new(server)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::{TlsAcceptor, TlsConnector};
    fn cert() -> Certificate {
        Certificate::generate_self_signed(["ignored".to_owned()]).unwrap()
    }
    async fn handshake(
        client: Arc<rustls::ClientConfig>,
        server: Arc<rustls::ServerConfig>,
    ) -> (bool, bool) {
        let (left, right) = tokio::io::duplex(16 * 1024);
        let connector = TlsConnector::from(client);
        let acceptor = TlsAcceptor::from(server);
        let (a, b) = tokio::join!(
            connector.connect(ServerName::try_from("ignored").unwrap(), left),
            acceptor.accept(right)
        );
        (a.is_ok(), b.is_ok())
    }
    #[tokio::test]
    async fn mutual_pins_accept_only_the_paired_identities() {
        let a = cert();
        let b = cert();
        let rogue = cert();
        let ap = fingerprint(&a.certificate[0]);
        let bp = fingerprint(&b.certificate[0]);
        let (client, _) = configs(&a, bp).unwrap();
        let (_, server) = configs(&b, ap).unwrap();
        assert_eq!(handshake(client, server).await, (true, true));
        // Replaced server certificate / MITM.
        let (client, _) = configs(&a, bp).unwrap();
        let (_, server) = configs(&rogue, ap).unwrap();
        let (accepted, _) = handshake(client, server).await;
        assert!(!accepted);
        // Unauthorized client, even when it knows the correct server pin.
        let (client, _) = configs(&rogue, bp).unwrap();
        let (_, server) = configs(&b, ap).unwrap();
        let (_, accepted) = handshake(client, server).await;
        assert!(!accepted);
    }
    #[test]
    fn malformed_pins_and_extra_certificates_are_rejected() {
        for pin in ["", "00", &"00:".repeat(32), &"zz:".repeat(31)] {
            assert!(parse_pin(pin).is_err());
        }
        let a = cert();
        let pin = parse_pin(&crate::crypto::certificate_fingerprint(&a)).unwrap();
        let verifier = PinnedPeer {
            pin,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        };
        assert!(verifier.check(&a.certificate[0], &[]).is_ok());
        assert!(verifier.check(&a.certificate[0], &a.certificate).is_err());
    }
}
