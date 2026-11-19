//! Certificates for the loopback RTMPS server, made when a test runs, so no private key is kept in
//! the repository: a root certificate authority and a server certificate for `localhost` and
//! 127.0.0.1 that it signs, both with ECDSA P-256 keys (RFC 5280 structures in DER).

use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1D, 0x11];
const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13];
const KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x0F];
const EXT_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x25];
const SERVER_AUTH: &[u8] = &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else if n <= 0xFF {
        out.extend_from_slice(&[0x81, n as u8]);
    } else {
        out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]);
    }
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

fn oid(v: &[u8]) -> Vec<u8> {
    tlv(0x06, v)
}

fn name(cn: &str) -> Vec<u8> {
    seq(&[tlv(0x31, &seq(&[oid(COMMON_NAME), tlv(0x0C, cn.as_bytes())]))])
}

fn extension(id: &[u8], critical: bool, value: Vec<u8>) -> Vec<u8> {
    let mut parts = vec![oid(id)];
    if critical {
        parts.push(tlv(0x01, &[0xFF]));
    }
    parts.push(tlv(0x04, &value));
    seq(&parts)
}

/// A certificate for `subject_key`, signed by `issuer_key`.
fn certificate(serial: u8, issuer: &str, subject: &str, subject_key: &EcdsaKeyPair, issuer_key: &EcdsaKeyPair, extensions: Vec<Vec<u8>>) -> Vec<u8> {
    let algorithm = seq(&[oid(ECDSA_WITH_SHA256)]);
    let mut point = vec![0];
    point.extend_from_slice(subject_key.public_key().as_ref());
    let spki = seq(&[seq(&[oid(EC_PUBLIC_KEY), oid(PRIME256V1)]), tlv(0x03, &point)]);
    let validity = seq(&[tlv(0x17, b"200101000000Z"), tlv(0x18, b"20991231235959Z")]);
    let tbs = seq(&[
        tlv(0xA0, &tlv(0x02, &[2])),
        tlv(0x02, &[serial]),
        algorithm.clone(),
        name(issuer),
        validity,
        name(subject),
        spki,
        tlv(0xA3, &seq(&extensions)),
    ]);
    let signature = issuer_key.sign(&SystemRandom::new(), &tbs).unwrap();
    let mut bits = vec![0];
    bits.extend_from_slice(signature.as_ref());
    seq(&[tbs, algorithm, tlv(0x03, &bits)])
}

fn key_pair() -> (EcdsaKeyPair, Vec<u8>) {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    (pair, pkcs8.as_ref().to_vec())
}

/// A root and a server certificate it signs, with the server's private key.
pub struct Pki {
    pub root: CertificateDer<'static>,
    pub leaf: CertificateDer<'static>,
    pub leaf_key: PrivateKeyDer<'static>,
}

impl Pki {
    pub fn new() -> Pki {
        let (ca, _) = key_pair();
        let (server, server_pkcs8) = key_pair();
        let root = certificate(
            1,
            "craft-rtmp test root",
            "craft-rtmp test root",
            &ca,
            &ca,
            vec![
                extension(BASIC_CONSTRAINTS, true, seq(&[tlv(0x01, &[0xFF])])),
                // keyCertSign and cRLSign.
                extension(KEY_USAGE, true, tlv(0x03, &[0x01, 0x06])),
            ],
        );
        let san = seq(&[tlv(0x82, b"localhost"), tlv(0x87, &[127, 0, 0, 1])]);
        let leaf = certificate(
            2,
            "craft-rtmp test root",
            "localhost",
            &server,
            &ca,
            vec![
                extension(SUBJECT_ALT_NAME, false, san),
                extension(EXT_KEY_USAGE, false, seq(&[oid(SERVER_AUTH)])),
                // digitalSignature.
                extension(KEY_USAGE, true, tlv(0x03, &[0x07, 0x80])),
            ],
        );
        Pki {
            root: CertificateDer::from(root),
            leaf: CertificateDer::from(leaf),
            leaf_key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_pkcs8)),
        }
    }
}
