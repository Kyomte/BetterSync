//! TLS configuration with pinned device IDs: a peer is accepted only if its
//! certificate's Ed25519 key is a paired device ID (invariant 6). There is
//! no certificate authority; the key *is* the identity.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use bsync_core::DeviceId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::{Identity, NetError};

pub const ALPN: &[u8] = b"bsync/1";

/// The set of paired device IDs, shared with the verifiers so pairing
/// changes apply to new connections immediately.
pub type PairedSet = Arc<RwLock<BTreeSet<DeviceId>>>;

const ED25519_OID: &str = "1.3.101.112";

/// The device ID in a certificate, if it carries an Ed25519 key.
pub fn device_id_from_cert(cert: &CertificateDer<'_>) -> Option<DeviceId> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref()).ok()?;
    let spki = parsed.public_key();
    if spki.algorithm.algorithm.to_id_string() != ED25519_OID {
        return None;
    }
    let raw: [u8; 32] = spki.subject_public_key.data.as_ref().try_into().ok()?;
    Some(DeviceId::from_bytes(raw))
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn is_paired(paired: &PairedSet, id: &DeviceId) -> bool {
    paired.read().map(|s| s.contains(id)).unwrap_or(false)
}

fn reject(why: &str) -> rustls::Error {
    rustls::Error::General(why.to_string())
}

#[derive(Debug)]
struct PinnedVerifier {
    paired: PairedSet,
    /// For outgoing connections: the device we meant to reach.
    expected: Option<DeviceId>,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedVerifier {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let id =
            device_id_from_cert(cert).ok_or_else(|| reject("certificate has no Ed25519 key"))?;
        if self.expected.is_some_and(|e| e != id) {
            return Err(reject("peer is not the device we dialed"));
        }
        if !is_paired(&self.paired, &id) {
            return Err(reject("peer is not paired"));
        }
        Ok(())
    }

    fn tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(reject("TLS 1.2 is not supported"))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for PinnedVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(reject("TLS 1.2 is not supported"))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }
}

fn verifier(paired: &PairedSet, expected: Option<DeviceId>) -> Arc<PinnedVerifier> {
    Arc::new(PinnedVerifier {
        paired: Arc::clone(paired),
        expected,
        algs: provider().signature_verification_algorithms,
    })
}

pub fn server_config(
    identity: &Identity,
    paired: &PairedSet,
) -> Result<rustls::ServerConfig, NetError> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(NetError::tls)?
        .with_client_cert_verifier(verifier(paired, None))
        .with_single_cert(vec![identity.cert()], identity.private_key())
        .map_err(NetError::tls)?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}

pub fn client_config(
    identity: &Identity,
    paired: &PairedSet,
    expected: DeviceId,
) -> Result<rustls::ClientConfig, NetError> {
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(NetError::tls)?
        .dangerous()
        .with_custom_certificate_verifier(verifier(paired, Some(expected)))
        .with_client_auth_cert(vec![identity.cert()], identity.private_key())
        .map_err(NetError::tls)?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}
