#![no_main]
//! Texto claro do registro de presença v2: bytes arbitrários nunca entram em pânico, e o que for aceito reproduz
//! exatamente a entrada ao ser montado de novo (nenhum texto diferente vale o mesmo registro).

use clam::presence::{decode_points_v2, encode_points_v2};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(points) = decode_points_v2(data) {
        let again = encode_points_v2(&points).expect("ponto aceito deve poder ser remontado");
        assert_eq!(again.as_slice(), data);
    }
});
