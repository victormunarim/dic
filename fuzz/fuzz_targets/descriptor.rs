#![no_main]
//! Descritor de relay vindo da rede: decodificar e verificar nunca entram em pânico.

use clam::descriptor::RelayDescriptor;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(d) = RelayDescriptor::from_bytes(data) {
        assert_eq!(d.to_bytes(), data);
        let _ = d.verify(1_795_000_000, 300);
        let _ = d.expires_at();
    }
});
