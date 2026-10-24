#![no_main]
//! Gerente de circuitos (cover ligado) recebendo células arbitrárias e relógio irregular.

use clam::circuit::{
    Cell,
    manager::{Candidate, CircuitManager, ManagerConfig},
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let cands: Vec<Candidate> = (0..4u8)
        .map(|i| Candidate { addr: format!("n{i}"), public: [i + 1; 32], link_public: [i + 1; 32], score: 1.0 })
        .collect();
    let mut mgr = CircuitManager::new(ManagerConfig { cover_ms: Some(20), ..Default::default() }, "dest:1", cands);
    let mut now = 0u64;
    let _ = mgr.start(now);
    for chunk in data.chunks(Cell::WIRE_LEN + 1) {
        if chunk.len() < Cell::WIRE_LEN + 1 {
            break;
        }
        now += u64::from(chunk[0]) * 2;
        let _ = mgr.tick(now);
        if let Ok(mut cell) = Cell::from_bytes(&chunk[1..]) {
            cell.circ = 1 + u32::from(chunk[0] & 3); // ids que o gerente realmente usa
            let _ = mgr.handle_cell(cell, now);
        }
        let _ = mgr.send(&chunk[1..40]);
    }
});
