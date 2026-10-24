#![no_main]
//! Registro de presença vindo da DHT: assinatura e decifra nunca entram em pânico.

use clam::{
    ed25519_dalek::SigningKey,
    pairing::PresenceKeys,
    presence,
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let a = StaticSecret::from([1u8; 32]);
    let b = StaticSecret::from([2u8; 32]);
    let keys = PresenceKeys::derive(&a, &PublicKey::from(&b));
    let owner = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
    let _ = presence::open(&keys, &owner, &[9u8; 32], data);
});
