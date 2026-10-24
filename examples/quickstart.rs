//! Fluxo completo: pareamento, publicação e busca de presença, modo de caminho
//! e tópicos de círculo. Rode com `cargo run --example quickstart`.

use clam::{
    circle::CircleSecret,
    ed25519_dalek::SigningKey,
    pairing::PresenceKeys,
    path::{PathMode, PathRequest},
    service::{self, PresenceConfig},
    store::MemoryStore,
    x25519_dalek::{PublicKey, StaticSecret},
};
use clam::rand_core::OsRng;

fn main() {
    // 1. Pareamento único: cada lado tem um segredo X25519 e troca só a parte pública
    //    (e a chave de verificação ed25519 de quem publica presença).
    let alice_secret = StaticSecret::random_from_rng(OsRng);
    let bob_secret = StaticSecret::random_from_rng(OsRng);
    let alice_signer = SigningKey::generate(&mut OsRng);

    let alice_keys = PresenceKeys::derive(&alice_secret, &PublicKey::from(&bob_secret));
    let bob_keys = PresenceKeys::derive(&bob_secret, &PublicKey::from(&alice_secret));

    // 2. Alice publica seus pontos de introdução (relays), nunca o IP.
    let store = MemoryStore::new(); // trocar pelo adaptador da DHT real
    let cfg = PresenceConfig::default();
    let now = 1_795_000_000; // segundos Unix
    let intro_points = vec![b"relay-a.example:4001".to_vec(), b"relay-b.example:4001".to_vec()];
    service::publish(&store, &cfg, &alice_keys, &alice_signer, now, &intro_points).unwrap();

    // 3. Bob procura Alice (com consultas falsas misturadas).
    let found = service::lookup(&store, &cfg, &bob_keys, &alice_signer.verifying_key(), now);
    println!("Bob achou Alice: {:?}", found.is_some());
    assert_eq!(found, Some(intro_points));

    // 4. Modo de caminho: o padrão é privado (2 saltos).
    let mode = PathRequest::private().resolve().unwrap();
    assert_eq!(mode, PathMode::TwoHop);
    println!("caminho: {mode:?}, {} relays, alvo {} ms", mode.relay_count(), mode.voice_latency_target_ms());

    // 5. Círculo: tópicos GossipSub derivados do segredo do grupo.
    let circle = CircleSecret::from_bytes([42; 32]);
    for topic in service::circle_topics(&circle, &cfg, now) {
        println!("assinar tópico: {topic}");
    }
}
