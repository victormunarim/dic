//! Descoberta de relays por GossipSub (feature `net`).
//!
//! Os relays anunciam o próprio [`RelayDescriptor`](crate::descriptor::RelayDescriptor)
//! no tópico público [`RELAY_TOPIC`]; todo nó que assina o tópico ouve, valida e
//! guarda no [`RelayDirectory`]. O app lê [`Discovery::candidates`] de tempos em
//! tempos e entrega ao [`CircuitManager`](crate::circuit::manager::CircuitManager).
//!
//! # Validação antes de repassar
//!
//! O [`Node`](crate::net::Node) usa validação manual do GossipSub: um descritor
//! só é repassado aos vizinhos (e só chega aqui) se tiver assinatura válida,
//! validade em dia e endereço permitido, conforme `NetConfig::relay_policy`.
//! Lixo e descritores forjados morrem no primeiro nó que os recebe, em vez de
//! inundar a rede. O que fica por conta do [`RelayDirectory`] é o estado:
//! versão mais nova, um endereço por identidade e teto de tamanho.
//!
//! # Fluxos
//!
//! - **Relay**: passe um [`Announcer`]. Publica o descritor logo (e de novo
//!   após 2 s, 10 s e 30 s, porque a malha do GossipSub leva um tempo para
//!   se formar), depois a cada `announce_every`. Também reenvia alguns
//!   descritores que conhece, para quem entrou depois ficar sabendo.
//! - **Cliente**: passe `None`. Só ouve. Para começar sem esperar a rede,
//!   carregue sementes com [`Discovery::load_seeds`].
//!
//! # Limites
//!
//! - O tópico é público: quem o assina vê quais relays existem. Isso é
//!   intencional (relays são públicos); o que se protege é quem usa quais.
//! - Um cliente sem nenhum vizinho no GossipSub não descobre nada: mantenha
//!   ao menos um `bootstrap` ou sementes.
//! - Sybil: veja [`crate::descriptor`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use rand_core::{OsRng, RngCore};
use tokio::{sync::mpsc, task::JoinHandle, time::interval};

use crate::{
    circuit::manager::Candidate,
    descriptor::{DirectoryConfig, InsertError, Inserted, RelayDirectory},
    net::Gossip,
    runner::Clock,
};

/// Tópico GossipSub dos descritores de relays.
pub const RELAY_TOPIC: &str = "clam/relays/v1";

/// Produz o descritor assinado do próprio relay para o instante `now` (segundos Unix).
/// Devolva `None` para não anunciar naquele momento. Reassine a cada chamada
/// (`issued_at = now`), para a versão nova substituir a antiga.
pub type Announcer = Arc<dyn Fn(u64) -> Option<Vec<u8>> + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct DiscoveryConfig {
    /// Intervalo entre anúncios do próprio descritor. Padrão: 10 min.
    pub announce_every: Duration,
    /// Intervalo entre reenvios de descritores de outros relays. Padrão: 5 min.
    pub regossip_every: Duration,
    /// Quantos descritores reenviar por rodada. Padrão: 8.
    pub regossip_count: usize,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            announce_every: Duration::from_secs(600),
            regossip_every: Duration::from_secs(300),
            regossip_count: 8,
        }
    }
}

/// Diretório compartilhado, alimentado pela rede e pelas sementes.
pub struct Discovery {
    dir: Mutex<RelayDirectory>,
    clock: Clock,
}

impl Discovery {
    pub fn new(cfg: DirectoryConfig, clock: Clock) -> Self {
        Self { dir: Mutex::new(RelayDirectory::new(cfg)), clock }
    }

    /// Valida e guarda um descritor recebido por qualquer canal.
    pub fn ingest(&self, bytes: &[u8]) -> Result<Inserted, InsertError> {
        let now = (self.clock)();
        self.dir.lock().expect("lock").insert_bytes(bytes, now)
    }

    /// Carrega sementes: um descritor em hexadecimal por linha; `#` inicia comentário.
    /// Devolve quantos foram aceitos.
    pub fn load_seeds(&self, text: &str) -> usize {
        text.lines()
            .map(|l| l.split('#').next().unwrap_or("").trim())
            .filter(|l| !l.is_empty())
            .filter_map(from_hex)
            .filter(|b| self.ingest(b).is_ok())
            .count()
    }

    /// Relays válidos agora, no formato do gerente de circuitos.
    pub fn candidates(&self) -> Vec<Candidate> {
        let now = (self.clock)();
        self.dir.lock().expect("lock").candidates(now)
    }

    pub fn len(&self) -> usize {
        self.dir.lock().expect("lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn export(&self) -> Vec<Vec<u8>> {
        self.dir.lock().expect("lock").export()
    }
}

/// Escreve um descritor como linha de semente (hexadecimal).
pub fn to_seed_line(descriptor: &[u8]) -> String {
    descriptor.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Tarefa que ouve a rede e anuncia. Encerra ao ser descartada.
pub struct DiscoveryRunner {
    task: JoinHandle<()>,
}

impl DiscoveryRunner {
    /// `incoming`: fila de [`Node::take_descriptors`](crate::net::Node::take_descriptors).
    pub fn spawn(
        discovery: Arc<Discovery>,
        gossip: Gossip,
        mut incoming: mpsc::UnboundedReceiver<Vec<u8>>,
        announcer: Option<Announcer>,
        cfg: DiscoveryConfig,
    ) -> Self {
        let _ = gossip.subscribe(RELAY_TOPIC);
        let task = tokio::spawn(async move {
            // Atrasos extras logo após subir: a malha do GossipSub ainda está se formando.
            let mut warmup = [30u64, 10, 2].into_iter();
            let mut next_announce = tokio::time::Instant::now();
            let mut regossip = interval(cfg.regossip_every);
            regossip.tick().await; // o primeiro tick é imediato; pula
            loop {
                tokio::select! {
                    msg = incoming.recv() => match msg {
                        Some(bytes) => { let _ = discovery.ingest(&bytes); }
                        None => return,
                    },
                    _ = tokio::time::sleep_until(next_announce), if announcer.is_some() => {
                        let now = (discovery.clock)();
                        if let Some(bytes) = announcer.as_ref().and_then(|a| a(now)) {
                            let _ = gossip.publish(RELAY_TOPIC, bytes);
                        }
                        next_announce = tokio::time::Instant::now()
                            + warmup.next_back().map_or(cfg.announce_every, Duration::from_secs);
                    }
                    _ = regossip.tick(), if announcer.is_some() => {
                        let mut all = discovery.export();
                        // Sorteio simples: embaralha um pouco e pega os primeiros.
                        for i in (1..all.len()).rev() {
                            all.swap(i, OsRng.next_u32() as usize % (i + 1));
                        }
                        for b in all.into_iter().take(cfg.regossip_count) {
                            let _ = gossip.publish(RELAY_TOPIC, b);
                        }
                    }
                }
            }
        });
        Self { task }
    }

    pub fn stop(self) {}
}

impl Drop for DiscoveryRunner {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::RelayDescriptor;
    use ed25519_dalek::SigningKey;

    const NOW: u64 = 1_800_000_000;

    fn disc() -> Discovery {
        Discovery::new(DirectoryConfig::default(), Arc::new(|| NOW))
    }

    fn desc(addr: &str) -> Vec<u8> {
        RelayDescriptor::sign(&SigningKey::generate(&mut OsRng), [5; 32], [6; 32], addr, 1000, NOW, 3600).unwrap().to_bytes()
    }

    #[test]
    fn seeds_load_good_lines_and_skip_comments_and_junk() {
        let d = disc();
        let text = format!(
            "# relays da equipe\n{}  # sp1\n\nnao-e-hex\n{}\nabc\n{}\n",
            to_seed_line(&desc("203.0.113.1:4001")),
            to_seed_line(&desc("127.0.0.1:4002")), // privado: recusado
            to_seed_line(&desc("203.0.113.3:4003")),
        );
        assert_eq!(d.load_seeds(&text), 2);
        assert_eq!(d.candidates().len(), 2);
    }

    #[test]
    fn ingest_rejects_garbage() {
        let d = disc();
        assert!(d.ingest(b"lixo").is_err());
        assert!(d.is_empty());
    }
}
