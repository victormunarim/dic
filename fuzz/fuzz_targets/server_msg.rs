#![no_main]
//! Quadros de servidor: `wire::decode` e `wire::open` nunca entram em pânico; a mensagem aceita sobrevive a
//! codificar de novo e a ir num quadro selado de A para B.

use clam::{
    server::wire::{self, Msg},
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

const SERVER: [u8; 16] = [5; 16];

fuzz_target!(|data: &[u8]| {
    let a = StaticSecret::from([1u8; 32]);
    let b = StaticSecret::from([2u8; 32]);
    let (pa, pb) = (PublicKey::from(&a).to_bytes(), PublicKey::from(&b).to_bytes());

    if let Some(msg) = wire::decode(data) {
        let again: Option<Msg> = wire::decode(&wire::encode(&msg));
        assert_eq!(again.as_ref(), Some(&msg), "codificar e ler de novo deve dar a mesma mensagem");
        if let Some(frame) = wire::seal(&a, &pb, &SERVER, &msg) {
            assert_eq!(wire::open(&b, &pa, &SERVER, &frame), Some(msg), "o quadro selado deve abrir igual");
        }
    }

    // Quadro de fora: quase sempre morre na cabeça ou na autenticação, mas nada disso pode derrubar o processo.
    let _ = wire::open(&b, &pa, &SERVER, data);
    let _ = wire::head(data);
    let _ = wire::nonce_tag(data);
});
