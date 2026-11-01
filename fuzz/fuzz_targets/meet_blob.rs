#![no_main]
//! Blob de introdução: bytes arbitrários nunca entram em pânico em `open_meeting`, e um blob válido adulterado em
//! qualquer byte nunca é aceito.

use clam::{
    circuit::manager::Candidate,
    meet::{MEETING_LEN, open_meeting, seal_meeting},
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let callee = StaticSecret::from([7u8; 32]);
    let now = 1_000_000;
    // Bytes soltos: o tamanho, a versão e a cifra recusam sem pânico.
    let _ = open_meeting(&callee, data, now);

    // Blob selado de verdade, com um byte trocado pelo que o fuzzer mandar.
    let caller = StaticSecret::from([3u8; 32]);
    let rv = Candidate { addr: "203.0.113.7:4001".to_string(), public: [1; 32], link_public: [2; 32], score: 1.0 };
    let Ok(mut blob) = seal_meeting(&caller, &PublicKey::from(&callee), &rv, [9; 32], now) else { return };
    assert_eq!(blob.len(), MEETING_LEN);
    let (at, flip) = (usize::from(data.first().copied().unwrap_or(0)) * 7 % MEETING_LEN, data.get(1).copied().unwrap_or(1) | 1);
    if let Some(byte) = blob.get_mut(at) {
        *byte ^= flip;
    }
    assert!(open_meeting(&callee, &blob, now).is_none(), "blob adulterado foi aceito");
});
