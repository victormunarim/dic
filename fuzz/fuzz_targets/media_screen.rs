#![no_main]
//! Remontagem de tela: pacotes autênticos com carga arbitrária (cabeçalho, pedidos de reenvio, fragmentos
//! fora de ordem, duplicados, totais que mudam) nunca derrubam o `ScreenStream`, e os pacotes que ele
//! devolve têm sempre 200 B.

use arbitrary::Arbitrary;
use clam::{
    media::{
        keys::{CallKeys, Ephemeral},
        packet::{MAX_PAYLOAD, PACKET_LEN, Sealer},
        screen::ScreenStream,
    },
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Step {
    /// Carga que o outro lado seleou (qualquer coisa até o tamanho máximo).
    Payload(Vec<u8>),
    /// Pacote de bytes quaisquer, que a AEAD recusa.
    Garbage(Vec<u8>),
    Push { data: Vec<u8>, keyframe: bool },
    Next,
    Report,
    Poll,
}

fn pair() -> (CallKeys, CallKeys) {
    let (a, b) = (StaticSecret::from([1u8; 32]), StaticSecret::from([2u8; 32]));
    let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
    let first = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).expect("chaves válidas");
    let second = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).expect("chaves válidas");
    (first, second)
}

fuzz_target!(|steps: Vec<Step>| {
    let (far, near) = pair();
    let mut remote = Sealer::new(&far);
    let mut stream = ScreenStream::new(&near);
    for step in steps.into_iter().take(256) {
        match step {
            Step::Payload(mut p) => {
                p.truncate(MAX_PAYLOAD);
                if let Ok(packet) = remote.seal(&p) {
                    let _ = stream.receive(&packet);
                }
            }
            Step::Garbage(g) => {
                let _ = stream.receive(&g);
            }
            Step::Push { mut data, keyframe } => {
                data.truncate(20_000);
                let _ = stream.push_frame(&data, keyframe);
            }
            Step::Next => {
                if let Ok(p) = stream.next_packet() {
                    assert_eq!(p.len(), PACKET_LEN);
                }
            }
            Step::Report => {
                if let Some(Ok(p)) = stream.report_packet() {
                    assert_eq!(p.len(), PACKET_LEN);
                }
            }
            Step::Poll => {
                let _ = stream.poll_frame();
                let _ = stream.take_keyframe_request();
                let _ = stream.take_report();
            }
        }
    }
});
