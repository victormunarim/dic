//! Chamada de voz de ponta a ponta, em memória, com rede ruim simulada.

use clam::media::{
    VoiceCall,
    codec::{Codec, Mulaw},
    keys::{CallKeys, Ephemeral},
    packet::PACKET_LEN,
};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};

fn calls() -> (VoiceCall<Mulaw>, VoiceCall<Mulaw>) {
    let (a, b) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
    let (ea, eb) = (Ephemeral::generate(), Ephemeral::generate());
    let ka = CallKeys::initiator(&a, &ea, &PublicKey::from(&b), &eb.public()).unwrap();
    let kb = CallKeys::responder(&b, &eb, &PublicKey::from(&a), &ea.public()).unwrap();
    (VoiceCall::new(&ka, Mulaw::new()), VoiceCall::new(&kb, Mulaw::new()))
}

fn tone(n: usize, frame: usize) -> Vec<i16> {
    (0..160).map(|i| (9000.0 * (2.0 * std::f64::consts::PI * 440.0 * (n * 160 + i + frame) as f64 / 8000.0).sin()) as i16).collect()
}

/// Gerador determinístico simples (xorshift) para atraso e perda.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn clean_network_plays_every_frame() {
    let (mut a, mut b) = calls();
    let mut played = 0;
    for n in 0..100u64 {
        let p = a.send(&tone(n as usize, 0)).unwrap();
        assert_eq!(p.len(), PACKET_LEN);
        b.receive(n * 20 + 30, &p).unwrap(); // 30 ms de rede
        played += b.playout(n * 20 + 30).len();
    }
    played += b.playout(100 * 20 + 40).len();
    let s = b.stats();
    assert_eq!((s.received, s.concealed, s.rejected), (100, 0, 0));
    assert!(played >= 98, "{played}");
}

#[test]
fn jittery_lossy_network_keeps_the_call_going() {
    let (mut a, mut b) = calls();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut wire: Vec<(u64, [u8; PACKET_LEN])> = vec![];
    for n in 0..500u64 {
        let p = a.send(&tone(n as usize, 0)).unwrap();
        if rng.next() % 100 < 5 {
            continue; // 5% de perda
        }
        wire.push((n * 20 + 40 + rng.next() % 60, p)); // 40..100 ms
    }
    wire.sort_by_key(|(t, _)| *t);
    let mut frames = 0;
    let mut wire = wire.into_iter().peekable();
    for t in 0..12_000u64 {
        while wire.peek().is_some_and(|(at, _)| *at <= t) {
            let (at, p) = wire.next().unwrap();
            // Duplicatas do atacante/rede são rejeitadas.
            b.receive(at, &p).unwrap();
            assert!(b.receive(at, &p).is_err());
        }
        frames += b.playout(t).len();
    }
    let s = b.stats();
    assert!(s.received >= 460, "{s:?}");
    assert_eq!(s.rejected, s.received); // as duplicatas
    assert!(s.played as f64 >= 0.80 * 500.0, "{s:?}");
    assert!(frames >= 480, "{frames}");
    assert!(s.delay_ms <= 200);
}

#[test]
fn forged_and_foreign_packets_never_reach_the_speaker() {
    let (mut a, mut b) = calls();
    let (mut other, _) = calls();
    let mut p = a.send(&tone(0, 0)).unwrap();
    p[50] ^= 0x40;
    assert!(b.receive(0, &p).is_err());
    assert!(b.receive(0, &other.send(&tone(0, 0)).unwrap()).is_err());
    assert!(b.receive(0, &[0u8; 10]).is_err());
    assert_eq!(b.playout(10_000).len(), 0);
    assert_eq!(b.stats().rejected, 3);
}

#[test]
fn audio_survives_the_whole_path() {
    let (mut a, mut b) = calls();
    let mut reference = Mulaw::new();
    let mut want = vec![];
    let mut got = vec![];
    for n in 0..50u64 {
        let pcm = tone(n as usize, 0);
        let enc = reference.encode(&pcm).unwrap();
        want.push(reference.decode(Some(&enc)));
        b.receive(n * 20, &a.send(&pcm).unwrap()).unwrap();
        got.extend(b.playout(n * 20 + 40));
    }
    got.extend(b.playout(50 * 20 + 40));
    // Fora o preenchimento do fim, o áudio é bit a bit o do codec: a cifra não altera nada.
    assert_eq!(got[..50], want[..]);
}
