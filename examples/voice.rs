//! Chamada de voz simulada: tom de 440 Hz → µ-law → cifra → rede ruim → jitter → PCM.
//!
//! ```text
//! cargo run --example voice -- [--loss 5] [--jitter-ms 60] [--seconds 10]
//! ```
//! Sem rede real: mostra as estatísticas da camada de mídia. Para usar sobre o circuito, envie
//! cada `VoiceCall::send` com `CircuitClient::send` e entregue cada `Event::Data` a `receive`.

use clam::media::{
    VoiceCall,
    codec::Mulaw,
    keys::{CallKeys, Ephemeral},
};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};

fn opt(args: &[String], name: &str, default: u64) -> u64 {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (loss, jitter, secs) = (opt(&args, "--loss", 5), opt(&args, "--jitter-ms", 60), opt(&args, "--seconds", 10));

    let (a, b) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
    let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
    let ka = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).expect("chaves");
    let kb = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).expect("chaves");
    let (mut alice, mut bob) = (VoiceCall::new(&ka, Mulaw::new()), VoiceCall::new(&kb, Mulaw::new()));

    let mut state = 0x2545F4914F6CDD1Du64;
    let mut rnd = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let frames = secs * 50;
    let mut wire = vec![];
    for n in 0..frames {
        let pcm: Vec<i16> = (0..160).map(|i| (8000.0 * (std::f64::consts::TAU * 440.0 * (n * 160 + i) as f64 / 8000.0).sin()) as i16).collect();
        let p = alice.send(&pcm).expect("envio");
        if rnd() % 100 >= loss {
            wire.push((n * 20 + 40 + rnd() % (jitter + 1), p));
        }
    }
    wire.sort_by_key(|(t, _)| *t);
    let mut wire = wire.into_iter().peekable();
    for t in 0..frames * 20 + 400 {
        while let Some((at, p)) = wire.next_if(|(at, _)| *at <= t) {
            let _ = bob.receive(at, &p);
        }
        bob.playout(t);
    }
    let (s, r) = (alice.stats(), bob.stats());
    println!("enviados {}  recebidos {}  tocados {}  escondidos {}  atrasados {}  atraso final {} ms", s.sent, r.received, r.played, r.concealed, r.late, r.delay_ms);
}
