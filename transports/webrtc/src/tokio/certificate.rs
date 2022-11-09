// Copyright 2022 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use hkdf::Hkdf;
use libp2p_core::identity;
use rand::{CryptoRng, Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use ring::test::rand::FixedSliceSequenceRandom;
use sha2::Sha256;
use webrtc::dtls::crypto::{CryptoPrivateKey, CryptoPrivateKeyKind};
use webrtc::peer_connection::certificate::RTCCertificate;

use crate::tokio::fingerprint::Fingerprint;

use std::cell::UnsafeCell;
use std::time::{Duration, SystemTime};

#[derive(Clone, PartialEq)]
pub struct Certificate {
    inner: RTCCertificate,
}
impl Certificate {
    /// Derives a new certificate from the provided keypair.
    ///
    /// This derivation is pure and will yield the same certificate for the same keypair.
    pub fn new(identity: &identity::Keypair) -> Result<Self, CertificateError> {
        let ed25519_keypair = match identity {
            identity::Keypair::Ed25519(inner) => inner,
            _ => return Err(CertificateError(Kind::UnsupportedKeypair)),
        };

        let hk = Hkdf::<Sha256>::from_prk(ed25519_keypair.secret().as_ref())
            .expect("key length to be valid");
        let mut seed = [0u8; 32];
        hk.expand(b"libp2p-webrtc-certificate".as_slice(), &mut seed)
            .expect("32 is a valid length for Sha256 to output");

        let mut rng = ChaCha20Rng::from_seed(seed);

        let (key_pair, key_pair_der_bytes) = make_keypair(&mut rng)?;

        let (certificate, expiry) = make_minimal_certificate(&mut rng, &key_pair)?;

        let certificate = RTCCertificate::from_existing(
            webrtc::dtls::crypto::Certificate {
                certificate: vec![rustls::Certificate(certificate.clone())],
                private_key: CryptoPrivateKey {
                    kind: CryptoPrivateKeyKind::Ecdsa256(key_pair),
                    serialized_der: key_pair_der_bytes,
                },
            },
            &pem::encode_config(
                &pem::Pem {
                    tag: "CERTIFICATE".to_string(),
                    contents: certificate,
                },
                pem::EncodeConfig {
                    line_ending: pem::LineEnding::LF,
                },
            ),
            expiry,
        );

        Ok(Self { inner: certificate })
    }

    /// Returns SHA-256 fingerprint of this certificate.
    ///
    /// # Panics
    ///
    /// This function will panic if there's no fingerprint with the SHA-256 algorithm (see
    /// [`RTCCertificate::get_fingerprints`]).
    pub fn fingerprint(&self) -> Fingerprint {
        let fingerprints = self.inner.get_fingerprints();
        let sha256_fingerprint = fingerprints
            .iter()
            .find(|f| f.algorithm == "sha-256")
            .expect("a SHA-256 fingerprint");

        Fingerprint::try_from_rtc_dtls(sha256_fingerprint).expect("we filtered by sha-256")
    }

    /// Returns this certificate in PEM format.
    #[cfg(test)]
    pub fn to_pem(&self) -> &str {
        self.inner.pem()
    }

    /// Extract the [`RTCCertificate`] from this wrapper.
    ///
    /// This function is `pub(crate)` to avoid leaking the `webrtc` dependency to our users.
    pub(crate) fn to_rtc_certificate(&self) -> RTCCertificate {
        self.inner.clone()
    }
}

/// The year 2000.
const UNIX_2000: i64 = 946645200;

/// The year 3000.
const UNIX_3000: i64 = 32503640400;

/// OID for the organisation name. See <http://oid-info.com/get/2.5.4.10>.
const ORGANISATION_NAME_OID: [u64; 4] = [2, 5, 4, 10];

/// OID for Elliptic Curve Public Key Cryptography. See <http://oid-info.com/get/1.2.840.10045.2.1>.
const EC_OID: [u64; 6] = [1, 2, 840, 10045, 2, 1];

/// OID for 256-bit Elliptic Curve Cryptography (ECC) with the P256 curve. See <http://oid-info.com/get/1.2.840.10045.3.1.7>.
const P256_OID: [u64; 7] = [1, 2, 840, 10045, 3, 1, 7];

/// OID for the ECDSA signature algorithm with using SHA256 as the hash function. See <http://oid-info.com/get/1.2.840.10045.4.3.2>.
const ECDSA_SHA256_OID: [u64; 7] = [1, 2, 840, 10045, 4, 3, 2];

/// Create a deterministic ECDSA keypair from the provided randomness source.
fn make_keypair<R>(rng: &mut R) -> Result<(EcdsaKeyPair, Vec<u8>), Kind>
where
    R: CryptoRng + Rng,
{
    let rng = FixedSliceSequenceRandom {
        bytes: &[&rng.gen::<[u8; 32]>(), &rng.gen::<[u8; 32]>()],
        current: UnsafeCell::new(0),
    };

    let document = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)?;
    let der_bytes = document.as_ref().to_owned();

    let key_pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der_bytes, &rng)?;

    Ok((key_pair, der_bytes))
}

/// Constructs a minimal x509 certificate.
///
/// The returned bytes are DER-encoded.
fn make_minimal_certificate<R>(
    rng: &mut R,
    key_pair: &EcdsaKeyPair,
) -> Result<(Vec<u8>, SystemTime), Kind>
where
    R: CryptoRng + Rng,
{
    let mut rand = [0u8; 32];
    rng.fill(&mut rand);

    let rng = FixedSliceSequenceRandom {
        bytes: &[&rand],
        current: UnsafeCell::new(0),
    };

    let certificate = simple_x509::X509::builder()
        .issuer_utf8(Vec::from(ORGANISATION_NAME_OID), "rust-libp2p")
        .subject_utf8(Vec::from(ORGANISATION_NAME_OID), "rust-libp2p")
        .not_before_gen(UNIX_2000)
        .not_after_gen(UNIX_3000)
        .pub_key_ec(
            Vec::from(EC_OID),
            key_pair.public_key().as_ref().to_owned(),
            Vec::from(P256_OID),
        )
        .sign_oid(Vec::from(ECDSA_SHA256_OID))
        .build()
        .sign(
            |cert, _| Some(key_pair.sign(&rng, cert).ok()?.as_ref().to_owned()),
            &vec![], // We close over the keypair so no need to pass it.
        )
        .ok_or(Kind::FailedToSign)?;

    let der_bytes = certificate.x509_enc().ok_or(Kind::FailedToEncode)?;

    let expiry = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(UNIX_3000 as u64))
        .expect("year 3000 to be representable by SystemTime");

    Ok((der_bytes, expiry))
}

#[derive(thiserror::Error, Debug)]
#[error("Failed to generate certificate")]
pub struct CertificateError(#[from] Kind);

#[derive(thiserror::Error, Debug)]
enum Kind {
    #[error(transparent)]
    KeyRejected(#[from] ring::error::KeyRejected),
    #[error(transparent)]
    Unspecified(#[from] ring::error::Unspecified),
    #[error("Failed to sign certificate")]
    FailedToSign,
    #[error("Failed to DER-encode certificate")]
    FailedToEncode,
    #[error("Only ED25519 keys are supported")]
    UnsupportedKeypair,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    #[test]
    fn certificate_generation_is_deterministic() {
        let keypair_seed = [0u8; 32];
        let cert_seed = [1u8; 32];

        let (keypair1, _) = make_keypair(&mut ChaCha20Rng::from_seed(keypair_seed)).unwrap();
        let (keypair2, _) = make_keypair(&mut ChaCha20Rng::from_seed(keypair_seed)).unwrap();

        let (bytes1, _) =
            make_minimal_certificate(&mut ChaCha20Rng::from_seed(cert_seed), &keypair1).unwrap();
        let (bytes2, _) =
            make_minimal_certificate(&mut ChaCha20Rng::from_seed(cert_seed), &keypair2).unwrap();

        assert_eq!(bytes1, bytes2)
    }

    #[test]
    fn different_seed_yields_different_certificate() {
        let keypair_seed = [0u8; 32];
        let cert1_seed = [1u8; 32];
        let cert2_seed = [2u8; 32];

        let (keypair1, _) = make_keypair(&mut ChaCha20Rng::from_seed(keypair_seed)).unwrap();
        let (keypair2, _) = make_keypair(&mut ChaCha20Rng::from_seed(keypair_seed)).unwrap();

        let (bytes1, _) =
            make_minimal_certificate(&mut ChaCha20Rng::from_seed(cert1_seed), &keypair1).unwrap();
        let (bytes2, _) =
            make_minimal_certificate(&mut ChaCha20Rng::from_seed(cert2_seed), &keypair2).unwrap();

        assert_ne!(bytes1, bytes2)
    }

    #[test]
    fn keypair_generation_is_deterministic() {
        let (key1, bytes1) = make_keypair(&mut ChaCha20Rng::from_seed([0u8; 32])).unwrap();
        let (key2, bytes2) = make_keypair(&mut ChaCha20Rng::from_seed([0u8; 32])).unwrap();

        assert_eq!(bytes1, bytes2);
        assert_eq!(key1.public_key().as_ref(), key2.public_key().as_ref());
    }

    // YOU MUST NOT EVER CHANGE THE ASSERTION IN THIS TEST.
    // THIS TEST FAILING MEANS YOU BROKE DETERMINISTIC CERTIFICATION GENERATION.
    //
    // libp2p essentially performs certificate pinning through the `/certhash` protocol
    // in multiaddresses. Peers expect boot nodes to have a certain certificate hash which
    // must not change after that nodes has been deployed.
    #[test]
    fn certificate_snapshot_test() {
        let ed25519_keypair = identity::ed25519::Keypair::decode(
            [
                53, 9, 7, 245, 172, 170, 239, 116, 74, 71, 187, 78, 240, 228, 186, 179, 151, 77,
                254, 157, 252, 125, 55, 88, 122, 71, 32, 138, 208, 235, 105, 116, 112, 48, 166,
                200, 235, 76, 139, 188, 249, 115, 178, 226, 0, 75, 229, 47, 222, 197, 68, 15, 212,
                233, 54, 5, 236, 0, 54, 166, 0, 75, 188, 237,
            ]
            .as_mut(),
        )
        .unwrap();

        let certificate = Certificate::new(&identity::Keypair::Ed25519(ed25519_keypair)).unwrap();

        assert_eq!(
            certificate.to_pem(),
            "-----BEGIN CERTIFICATE-----
MIIBEDCBvgIBADAKBggqhkjOPQQDAjAWMRQwEgYDVQQKDAtydXN0LWxpYnAycDAi
GA8xOTk5MTIzMTEzMDAwMFoYDzI5OTkxMjMxMTMwMDAwWjAWMRQwEgYDVQQKDAty
dXN0LWxpYnAycDBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABP7qJcaJtDqlVagl
J8ZwSDFiI996MW8W4W05WboigwMfpDpBCfcOjeoERjjFepm/CAFhmxW6dt2mK67V
w1hIsWkwCgYIKoZIzj0EAwIDQQB77aXmXxlEpqUSJbn/Xp+W+5Oje+lXHojeOTAN
UaEhlIypY2gCreXJ0o2MsiCsT5NXfyHQJ5sSgiZtPx+ldICa
-----END CERTIFICATE-----
"
        )
    }

    #[test]
    fn cloned_certificate_is_equivalent() {
        let certificate = Certificate::new(&identity::Keypair::generate_ed25519()).unwrap();

        let cloned_certificate = certificate.clone();

        assert!(certificate == cloned_certificate)
    }
}
