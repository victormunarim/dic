//! Publicação automática de presença (feature `net`).
//!
//! Uma tarefa tokio mantém, para você:
//! - o registro de presença de cada contato, republicado a cada
//!   [`RunnerConfig::refresh_secs`];
//! - o registro da **próxima** época, publicado nos últimos
//!   [`RunnerConfig::lead_secs`] da época atual, para o endereço novo já existir
//!   quando o relógio virar;
//! - a assinatura dos tópicos de cada círculo (época atual e seguinte), com
//!   cancelamento dos tópicos vencidos.
//!
//! Mudar contatos, círculos ou pontos de introdução republica na hora.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ed25519_dalek::SigningKey;
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    circle::CircleSecret,
    pairing::PresenceKeys,
    presence::{self, PresenceError},
    service::{self, PresenceConfig},
    store::{RecordStore, TopicControl},
};

/// Relógio em segundos Unix. Injetável para testes.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

#[derive(Debug, Clone, Copy)]
pub struct RunnerConfig {
    pub presence: PresenceConfig,
    /// Intervalo entre republicações dentro de uma época. Padrão: 900 s.
    pub refresh_secs: u64,
    /// Janela antes da virada em que o registro da próxima época já é publicado.
    /// Padrão: 300 s. Limitada a metade da época.
    pub lead_secs: u64,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self { presence: PresenceConfig::default(), refresh_secs: 900, lead_secs: 300 }
    }
}

#[derive(Clone, Default)]
struct State {
    contacts: Vec<PresenceKeys>,
    circles: Vec<CircleSecret>,
    intro_points: Vec<Vec<u8>>,
}

/// Alça da tarefa. Descartá-la (ou chamar [`PresenceRunner::stop`]) encerra a tarefa.
pub struct PresenceRunner {
    state: watch::Sender<State>,
    task: JoinHandle<()>,
}

impl Drop for PresenceRunner {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PresenceRunner {
    /// Inicia a tarefa. Precisa rodar dentro de um runtime tokio.
    /// Começa sem contatos, círculos nem pontos de introdução: preencha com os `set_*`.
    pub fn spawn<S>(store: Arc<S>, signer: SigningKey, cfg: RunnerConfig, clock: Clock) -> Self
    where
        S: RecordStore + TopicControl + Send + Sync + 'static,
    {
        let (state, rx) = watch::channel(State::default());
        let task = tokio::spawn(run(store, signer, cfg, clock, rx));
        Self { state, task }
    }

    /// Um registro de presença por contato, cada um com as chaves do par.
    pub fn set_contacts(&self, contacts: Vec<PresenceKeys>) {
        self.state.send_modify(|s| s.contacts = contacts);
    }

    pub fn set_circles(&self, circles: Vec<CircleSecret>) {
        self.state.send_modify(|s| s.circles = circles);
    }

    /// Valida (máx. 8 pontos, 1 a 64 bytes cada) e republica na hora.
    pub fn set_intro_points(&self, intro_points: Vec<Vec<u8>>) -> Result<(), PresenceError> {
        presence::validate_intro_points(&intro_points)?;
        self.state.send_modify(|s| s.intro_points = intro_points);
        Ok(())
    }

    pub fn stop(self) {}
}

async fn run<S>(
    store: Arc<S>,
    signer: SigningKey,
    cfg: RunnerConfig,
    clock: Clock,
    mut rx: watch::Receiver<State>,
) where
    S: RecordStore + TopicControl + Send + Sync + 'static,
{
    let epoch_secs = cfg.presence.epoch_secs.max(1);
    let lead = cfg.lead_secs.min(epoch_secs / 2);
    let mut subscribed: HashSet<String> = HashSet::new();
    loop {
        let now = clock();
        let state = rx.borrow_and_update().clone();
        tick(&*store, &signer, &cfg, &state, now, lead, &mut subscribed);

        let wait = next_wait(now, epoch_secs, lead, cfg.refresh_secs);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            changed = rx.changed() => if changed.is_err() { return },
        }
    }
}

fn tick<S: RecordStore + TopicControl>(
    store: &S,
    signer: &SigningKey,
    cfg: &RunnerConfig,
    state: &State,
    now: u64,
    lead: u64,
    subscribed: &mut HashSet<String>,
) {
    let epoch_secs = cfg.presence.epoch_secs.max(1);
    let in_lead_window = now % epoch_secs >= epoch_secs - lead;
    for keys in &state.contacts {
        // Pontos de introdução já validados em `set_intro_points`.
        let _ = service::publish(store, &cfg.presence, keys, signer, now, &state.intro_points);
        if in_lead_window {
            let _ = service::publish(store, &cfg.presence, keys, signer, now + epoch_secs, &state.intro_points);
        }
    }

    let wanted: HashSet<String> = state
        .circles
        .iter()
        .flat_map(|c| service::circle_topics(c, &cfg.presence, now))
        .collect();
    for topic in wanted.difference(subscribed) {
        store.subscribe(topic);
    }
    for topic in subscribed.difference(&wanted) {
        store.unsubscribe(topic);
    }
    *subscribed = wanted;
}

/// Segundos até a próxima passada: o que vier primeiro entre o intervalo de
/// republicação, o início da janela de antecedência e a virada da época.
fn next_wait(now: u64, epoch_secs: u64, lead: u64, refresh_secs: u64) -> u64 {
    let t = now % epoch_secs;
    let lead_start = epoch_secs - lead;
    let boundary = if t < lead_start { lead_start } else { epoch_secs };
    refresh_secs.min(boundary - t).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{epoch_of, DEFAULT_EPOCH_SECS as E};
    use rand_core::OsRng;
    use std::sync::Mutex;
    use tokio::time::Instant;
    use x25519_dalek::{PublicKey, StaticSecret};

    #[derive(Default)]
    struct Fake {
        puts: Mutex<Vec<([u8; 32], Vec<u8>)>>,
        topics: Mutex<HashSet<String>>,
    }

    impl RecordStore for Fake {
        fn put(&self, key: [u8; 32], value: Vec<u8>) {
            self.puts.lock().unwrap().push((key, value));
        }
        fn get(&self, _: &[u8; 32]) -> Option<Vec<u8>> {
            None
        }
    }

    impl TopicControl for Fake {
        fn subscribe(&self, t: &str) {
            self.topics.lock().unwrap().insert(t.into());
        }
        fn unsubscribe(&self, t: &str) {
            self.topics.lock().unwrap().remove(t);
        }
    }

    impl Fake {
        fn put_count(&self) -> usize {
            self.puts.lock().unwrap().len()
        }
        fn has_put(&self, key: &[u8; 32]) -> bool {
            self.puts.lock().unwrap().iter().any(|(k, _)| k == key)
        }
    }

    /// Relógio que anda junto com o tempo pausado do tokio, a partir de `base`.
    fn test_clock(base: u64) -> Clock {
        let start = Instant::now();
        Arc::new(move || base + start.elapsed().as_secs())
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    async fn advance(secs: u64) {
        tokio::time::advance(Duration::from_secs(secs)).await;
        settle().await;
    }

    struct Rig {
        store: Arc<Fake>,
        runner: PresenceRunner,
        keys: PresenceKeys,
        circle: CircleSecret,
        signer: SigningKey,
    }

    /// `base` é múltiplo da época: o teste começa no início de uma época.
    async fn rig(base: u64) -> Rig {
        let store = Arc::new(Fake::default());
        let signer = SigningKey::generate(&mut OsRng);
        let runner = PresenceRunner::spawn(store.clone(), signer.clone(), RunnerConfig::default(), test_clock(base));
        let a = StaticSecret::random_from_rng(OsRng);
        let b = StaticSecret::random_from_rng(OsRng);
        let keys = PresenceKeys::derive(&a, &PublicKey::from(&b));
        let circle = CircleSecret::from_bytes([3; 32]);
        runner.set_contacts(vec![keys.clone()]);
        runner.set_circles(vec![circle.clone()]);
        runner.set_intro_points(vec![b"relay:4001".to_vec()]).unwrap();
        settle().await;
        Rig { store, runner, keys, circle, signer }
    }

    const BASE: u64 = 100 * E;

    #[tokio::test(start_paused = true)]
    async fn publishes_immediately_for_current_epoch_only() {
        let r = rig(BASE).await;
        let id = r.keys.lookup_id(epoch_of(BASE, E));
        assert!(r.store.has_put(&id));
        assert!(!r.store.has_put(&r.keys.lookup_id(epoch_of(BASE, E) + 1)));
        let rec = r.store.puts.lock().unwrap().iter().find(|(k, _)| *k == id).unwrap().1.clone();
        let got = presence::open(&r.keys, &r.signer.verifying_key(), &id, &rec).unwrap();
        assert_eq!(got, vec![b"relay:4001".to_vec()]);
        assert_eq!(r.store.topics.lock().unwrap().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn republishes_every_refresh_interval() {
        let r = rig(BASE).await;
        let before = r.store.put_count();
        advance(RunnerConfig::default().refresh_secs + 1).await;
        assert!(r.store.put_count() > before);
    }

    #[tokio::test(start_paused = true)]
    async fn publishes_next_epoch_in_lead_window() {
        let r = rig(BASE).await;
        let next = r.keys.lookup_id(epoch_of(BASE, E) + 1);
        advance(E - 600).await; // antes da janela de 300 s
        assert!(!r.store.has_put(&next));
        advance(400).await; // dentro da janela
        assert!(r.store.has_put(&next));
    }

    #[tokio::test(start_paused = true)]
    async fn rotates_topics_after_epoch_change() {
        let r = rig(BASE).await;
        let e = epoch_of(BASE, E);
        let old = r.circle.topic(crate::circle::TopicKind::Presence, e);
        let newest = r.circle.topic(crate::circle::TopicKind::Presence, e + 2);
        advance(E + E / 2).await;
        let topics = r.store.topics.lock().unwrap();
        assert!(!topics.contains(&old));
        assert!(topics.contains(&newest));
        assert_eq!(topics.len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn intro_point_change_republishes_now() {
        let r = rig(BASE).await;
        let before = r.store.put_count();
        r.runner.set_intro_points(vec![b"outro:4001".to_vec()]).unwrap();
        settle().await;
        assert!(r.store.put_count() > before);
    }

    #[tokio::test(start_paused = true)]
    async fn rejects_invalid_intro_points_without_republishing() {
        let r = rig(BASE).await;
        let before = r.store.put_count();
        assert_eq!(r.runner.set_intro_points(vec![vec![]]), Err(PresenceError::BadIntroPoint));
        settle().await;
        assert_eq!(r.store.put_count(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_ends_publishing() {
        let r = rig(BASE).await;
        let store = r.store.clone();
        r.runner.stop();
        settle().await;
        let before = store.put_count();
        advance(3 * E).await;
        assert_eq!(store.put_count(), before);
    }

    #[test]
    fn next_wait_picks_nearest_deadline() {
        assert_eq!(next_wait(0, 3600, 300, 900), 900);
        assert_eq!(next_wait(3000, 3600, 300, 900), 300); // até a janela
        assert_eq!(next_wait(3400, 3600, 300, 900), 200); // até a virada
        assert_eq!(next_wait(3599, 3600, 300, 900), 1);
    }
}
