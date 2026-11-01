#![no_main]
//! Mensagens de `intro:9` e `rv:9`: os parsers nunca entram em pânico e, quando aceitam, reproduzem a entrada;
//! o acumulador de pedaços respeita os limites qualquer que seja o corte dos bytes.

use clam::hidden::{BadFrame, CHUNK, IntroReply, IntroRequest, MAX_BODY, Messages, RvReply, RvRequest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(request) = IntroRequest::parse(data) {
        assert_eq!(request.encode(), data, "pedido aceito deve reproduzir a entrada");
    }
    if let Some(reply) = IntroReply::parse(data) {
        assert_eq!(reply.encode(), data, "resposta aceita deve reproduzir a entrada");
    }
    if let Some(request) = RvRequest::parse(data) {
        assert_eq!(request.encode(), data, "pedido de encontro aceito deve reproduzir a entrada");
    }
    if let Some(reply) = RvReply::parse(data) {
        assert_eq!(reply.encode(), data);
    }
    // O primeiro byte escolhe o tamanho dos pedaços; o resto são os bytes do circuito.
    let Some((&cut, rest)) = data.split_first() else { return };
    let mut messages = Messages::new();
    for chunk in rest.chunks(usize::from(cut) % (CHUNK + 20) + 1) {
        match messages.push(chunk) {
            Ok(bodies) => {
                for body in bodies {
                    assert!(!body.is_empty() && body.len() <= MAX_BODY);
                }
                assert!(messages.pending() <= MAX_BODY + 2 + CHUNK);
            }
            Err(BadFrame) => return,
        }
    }
});
