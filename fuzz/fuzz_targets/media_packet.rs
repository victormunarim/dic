#![no_main]
//! Pacote de mídia: `Opener::open` nunca entra em pânico com bytes quaisquer; o que o `Sealer` sela abre
//! igual, uma vez só (a repetição é recusada), e um byte trocado nunca abre.

use arbitrary::Arbitrary;
use clam::{
    media::{
        keys::{CallKeys, Ephemeral},
        packet::{MAX_PAYLOAD, Opened, Opener, Sealer},
    },
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Input {
    Raw(Vec<u8>),
    Voice { payload: Vec<u8>, at: usize, xor: u8 },
}

fn pair() -> (CallKeys, CallKeys) {
    let (a, b) = (StaticSecret::from([1u8; 32]), StaticSecret::from([2u8; 32]));
    let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
    let first = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).expect("chaves válidas");
    let second = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).expect("chaves válidas");
    (first, second)
}

fuzz_target!(|input: Input| {
    let (tx_keys, rx_keys) = pair();
    let mut opener = Opener::new(&rx_keys);
    match input {
        Input::Raw(data) => {
            let _ = opener.open(&data);
        }
        Input::Voice { mut payload, at, xor } => {
            payload.truncate(MAX_PAYLOAD);
            let mut sealer = Sealer::new(&tx_keys);
            let packet = sealer.seal(&payload).expect("cabe no pacote");
            let mut bad = packet;
            if xor != 0 {
                let len = bad.len();
                if let Some(b) = bad.get_mut(at % len) {
                    *b ^= xor;
                }
                assert!(opener.open(&bad).is_err(), "pacote adulterado não pode abrir");
            }
            match opener.open(&packet) {
                Ok((_, Opened::Voice(got))) => assert_eq!(got, payload),
                other => panic!("pacote verdadeiro deve abrir como voz: {other:?}"),
            }
            assert!(opener.open(&packet).is_err(), "repetição deve ser recusada");
        }
    }
});
