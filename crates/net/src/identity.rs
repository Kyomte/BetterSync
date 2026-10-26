//! Device identity: an Ed25519 key pair. The public key is the device ID;
//! the TLS certificate is self-signed with the same key.

use std::io;
use std::path::Path;

use bsync_core::DeviceId;
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::NetError;

pub struct Identity {
    key_pkcs8: Vec<u8>,
    cert_der: CertificateDer<'static>,
    device_id: DeviceId,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn generate() -> Result<Self, NetError> {
        let key = KeyPair::generate_for(&PKCS_ED25519).map_err(NetError::tls)?;
        Self::from_key(&key)
    }

    pub fn from_pkcs8(der: &[u8]) -> Result<Self, NetError> {
        let key = KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(der.to_vec()),
            &PKCS_ED25519,
        )
        .map_err(NetError::tls)?;
        Self::from_key(&key)
    }

    fn from_key(key: &KeyPair) -> Result<Self, NetError> {
        let raw: [u8; 32] = key
            .public_key_raw()
            .try_into()
            .map_err(|_| NetError::Tls("unexpected Ed25519 public key length".into()))?;
        let device_id = DeviceId::from_bytes(raw);
        let params = CertificateParams::new(vec!["bsync".to_string()]).map_err(NetError::tls)?;
        let cert = params.self_signed(key).map_err(NetError::tls)?;
        Ok(Self {
            key_pkcs8: key.serialize_der(),
            cert_der: cert.der().clone(),
            device_id,
        })
    }

    /// Load the key from `path`, creating it (readable only by this user
    /// where the OS supports that) if it does not exist.
    pub fn load_or_create(path: &Path) -> Result<Self, NetError> {
        match std::fs::read(path) {
            Ok(der) => Self::from_pkcs8(&der),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                write_private(path, &id.key_pkcs8)?;
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The private key as PKCS#8 DER (what [`Identity::load_or_create`] stores).
    pub fn pkcs8(&self) -> &[u8] {
        &self.key_pkcs8
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn cert(&self) -> CertificateDer<'static> {
        self.cert_der.clone()
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_cert_carries_device_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/identity.key");
        let a = Identity::load_or_create(&path).unwrap();
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.device_id(), b.device_id());
        assert_eq!(
            crate::tls::device_id_from_cert(&a.cert()),
            Some(a.device_id())
        );
        assert_ne!(Identity::generate().unwrap().device_id(), a.device_id());
    }
}
