#![no_main]
//! Convite e resposta de chamada: `Incoming::parse` e `Caller::answer` nunca entram em pânico, com bytes
//! quaisquer ou com um quadro verdadeiro adulterado. O quadro verdadeiro, sem adulterar, abre.

use arbitrary::Arbitrary;
use clam::{
    media::signal::{Caller, Incoming, RelayHint},
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Input {
    Raw(Vec<u8>),
    /// Convite verdadeiro com um byte trocado: chega até a AEAD, ao bloco de dica e à etiqueta do codec.
    Tweaked { at: usize, xor: u8, hint: bool },
}

fuzz_target!(|input: Input| {
    let caller = StaticSecret::from([1u8; 32]);
    let me = StaticSecret::from([2u8; 32]);
    let me_pk = PublicKey::from(&me);

    match input {
        Input::Raw(data) => {
            let _ = Incoming::parse(&me, &data);
            if let Ok((c, _)) = Caller::invite(&caller, &me_pk, 0) {
                let _ = c.answer(&caller, &data);
            }
        }
        Input::Tweaked { at, xor, hint } => {
            let hint = hint.then(|| RelayHint { addr: "203.0.113.7:4001".parse().unwrap(), onion: [3; 32], link: [4; 32] });
            let Ok((_, mut frame)) = Caller::invite_with_hint(&caller, &me_pk, 0, hint.as_ref()) else { return };
            assert!(Incoming::parse(&me, &frame).is_ok(), "convite verdadeiro deve abrir");
            let len = frame.len();
            if let Some(b) = frame.get_mut(at % len) {
                *b ^= xor;
            }
            let _ = Incoming::parse(&me, &frame);
        }
    }
});
