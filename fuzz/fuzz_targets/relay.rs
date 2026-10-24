#![no_main]
//! Relay com um circuito legítimo aberto, alimentado com células arbitrárias em vários
//! links. A entrada é fatiada em células de 261 bytes; o primeiro byte de cada fatia escolhe
//! o link. Nada pode entrar em pânico, e o número de circuitos continua limitado.

use clam::circuit::{
    Cell, CELL_LEN,
    client::ClientCircuit,
    relay::{RelayConfig, RelayNode},
};
use clam::x25519_dalek::StaticSecret;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let cfg = RelayConfig { max_circuits: 8, ..Default::default() };
    let mut node = RelayNode::new(StaticSecret::from([7u8; 32]), cfg, 0);
    // Circuito de verdade em (link 1, circ 1), para as mutações chegarem às camadas AEAD.
    let (_cc, create) = ClientCircuit::create(1, &node.public_key());
    let _ = node.handle_cell(1, create, 0);

    let mut now = 0u64;
    for chunk in data.chunks(Cell::WIRE_LEN + 1) {
        if chunk.len() < Cell::WIRE_LEN + 1 {
            break;
        }
        now += u64::from(chunk[0]) * 3;
        let link = u64::from(chunk[0] & 3);
        let Ok(cell) = Cell::from_bytes(&chunk[1..]) else { continue };
        let _ = node.handle_cell(link, cell, now);
        let _ = node.tick(now);
    }
    assert!(node.circuit_count() <= 8);
    let _ = CELL_LEN;
});
