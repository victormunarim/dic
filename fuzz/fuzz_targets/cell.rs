#![no_main]
//! Parsers de célula: nunca entram em pânico e, quando aceitam, reproduzem a entrada.

use clam::circuit::{Cell, CELL_LEN, RelayCell, relay_cell_len};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(cell) = Cell::from_bytes(data) {
        assert_eq!(cell.to_bytes(), data, "célula aceita deve reproduzir a entrada");
    }
    if let Ok(rc) = RelayCell::decode(data) {
        // O que decodifica precisa recodificar sem pânico num corpo do tamanho da célula.
        let _ = rc.encode(relay_cell_len(1).max(data.len()).min(CELL_LEN));
    }
});
