//! IVK crypto sanity tests.
//!
//! These are protocol-level checks around pk_ivk derivation and the
//! (epk, ct) scanning/decryption flow. They are intentionally kept out of
//! `src/hash.rs` to keep the library code test-free.

use midnight_privacy::{ivk_sk_from_sk, pk_from_sk, pk_ivk_from_sk, Hash32, PrivacyAddress};

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    Key, XChaCha20Poly1305, XNonce,
};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

fn clamp_x25519_scalar(mut scalar: Hash32) -> [u8; 32] {
    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    scalar
}

fn ivk_aead_key_nonce(domain: &Hash32, dh: &[u8; 32]) -> (Key, XNonce) {
    const INFO: &[u8] = b"MP_IVK_AEAD_V1";
    let hk = Hkdf::<Sha256>::new(Some(domain), dh);
    let mut okm = [0u8; 56]; // 32 bytes key + 24 bytes nonce
    hk.expand(INFO, &mut okm).expect("HKDF expand");

    let mut key = [0u8; 32];
    key.copy_from_slice(&okm[..32]);
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&okm[32..]);
    (Key::from(key), XNonce::from(nonce))
}

#[test]
fn pk_ivk_matches_x25519_base_of_ivk_sk() {
    let domain: Hash32 = [1u8; 32];
    let spend_sk: Hash32 = [42u8; 32];

    let ivk_sk = ivk_sk_from_sk(&domain, &spend_sk);
    let clamped = clamp_x25519_scalar(ivk_sk);
    let secret = StaticSecret::from(clamped);
    let public = PublicKey::from(&secret);

    assert_eq!(*public.as_bytes(), pk_ivk_from_sk(&domain, &spend_sk));
}

#[test]
fn x25519_dh_roundtrip_smoke() {
    let domain: Hash32 = [1u8; 32];
    let spend_sk: Hash32 = [42u8; 32];

    let ivk_sk = ivk_sk_from_sk(&domain, &spend_sk);
    let ivk_secret = StaticSecret::from(clamp_x25519_scalar(ivk_sk));
    let pk_ivk = PublicKey::from(&ivk_secret);

    let esk_seed: Hash32 = [7u8; 32];
    let esk_secret = StaticSecret::from(clamp_x25519_scalar(esk_seed));
    let epk = PublicKey::from(&esk_secret);

    // Sender: dh = X25519(esk, receiver_pk_ivk)
    let dh_sender = esk_secret.diffie_hellman(&pk_ivk);
    // Receiver: dh = X25519(ivk_sk, epk)
    let dh_receiver = ivk_secret.diffie_hellman(&epk);
    assert_eq!(dh_sender.as_bytes(), dh_receiver.as_bytes());

    // Negative: using pk_spend bytes in place of pk_ivk breaks the DH.
    let pk_spend_bytes = pk_from_sk(&spend_sk);
    let pk_spend_as_pk_ivk = PublicKey::from(pk_spend_bytes);
    let dh_sender_wrong = esk_secret.diffie_hellman(&pk_spend_as_pk_ivk);
    assert_ne!(dh_sender_wrong.as_bytes(), dh_receiver.as_bytes());
}

#[test]
fn ivk_encrypt_decrypt_roundtrip_from_privacy_address() {
    let domain: Hash32 = [1u8; 32];

    // Receiver has spend_sk and publishes a privacy address containing (pk_spend, pk_ivk).
    let spend_sk: Hash32 = [42u8; 32];
    let pk_spend = pk_from_sk(&spend_sk);
    let pk_ivk = pk_ivk_from_sk(&domain, &spend_sk);
    assert_ne!(pk_spend, pk_ivk, "test expects a v2 (pk_spend, pk_ivk) address");

    let addr = PrivacyAddress::from_keys(&pk_spend, &pk_ivk).to_string();

    // Sender learns pk_ivk from the receiver's public privacy address (privpool1...).
    let parsed: PrivacyAddress = addr.parse().expect("valid bech32m v2 address");
    assert_eq!(parsed.to_pk(), pk_spend);
    assert_eq!(parsed.pk_ivk(), pk_ivk);
    let receiver_pk_ivk = PublicKey::from(parsed.pk_ivk());

    // Sender-side: per-output ephemeral keypair (esk, epk). The tx includes epk + ciphertext.
    let esk_seed: Hash32 = [7u8; 32];
    let esk = StaticSecret::from(clamp_x25519_scalar(esk_seed));
    let epk = PublicKey::from(&esk);

    let dh_sender = esk.diffie_hellman(&receiver_pk_ivk);
    let (key, nonce) = ivk_aead_key_nonce(&domain, dh_sender.as_bytes());
    let cipher = XChaCha20Poly1305::new(&key);

    let plaintext = b"ivk note payload";
    let ct = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: epk.as_bytes(), // bind to epk (part of tx)
            },
        )
        .expect("encrypt");

    // Receiver-side: derive ivk_sk from spend_sk, compute the same DH using epk from tx, then decrypt.
    let ivk_secret =
        StaticSecret::from(clamp_x25519_scalar(ivk_sk_from_sk(&domain, &spend_sk)));
    let dh_receiver = ivk_secret.diffie_hellman(&epk);
    let (key2, nonce2) = ivk_aead_key_nonce(&domain, dh_receiver.as_bytes());
    let cipher2 = XChaCha20Poly1305::new(&key2);
    let pt = cipher2
        .decrypt(
            &nonce2,
            Payload {
                msg: &ct,
                aad: epk.as_bytes(),
            },
        )
        .expect("decrypt");

    assert_eq!(pt, plaintext);
}

