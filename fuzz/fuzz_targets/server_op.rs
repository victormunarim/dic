#![no_main]
//! Operações do registro de servidor: `Op::decode` nunca entra em pânico e a operação aceita reproduz a entrada.

use clam::server::op::Op;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(op) = Op::decode(data) else { return };
    assert_eq!(op.encode(), data, "operação aceita deve reproduzir a entrada");
    // A assinatura nunca vale para chaves que o fuzzer inventou, mas conferir não pode entrar em pânico.
    let _ = op.verify(&[0; 16], &[0; 32]);
    if let Some(vk) = data.first_chunk::<32>() {
        let _ = op.verify(&[9; 16], vk);
    }
});
