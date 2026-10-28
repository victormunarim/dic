//! Pequenas funções de apoio do nó: relógio, notas de rede, sorteio e espera do circuito.

use std::time::Duration;

use rand_core::{OsRng, RngCore};

use crate::transport::CircuitClient;

pub(super) fn unix_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

pub(super) fn unix_secs() -> u64 {
    (unix_millis() / 1000) as u64
}

/// Acrescenta um aviso à nota de rede, separando por `; `.
pub(super) fn push_note(note: &mut Option<String>, add: String) {
    *note = Some(match note.take() {
        Some(n) => format!("{n}; {add}"),
        None => add,
    });
}

/// Embaralha `list` (Fisher-Yates com o gerador do sistema).
pub(super) fn shuffle<T>(list: &mut [T]) {
    for i in (1..list.len()).rev() {
        list.swap(i, OsRng.next_u32() as usize % (i + 1));
    }
}

/// Espera o circuito do cliente ficar pronto, até `limit`.
pub(super) async fn ready_within(client: &CircuitClient, limit: Duration) -> bool {
    tokio::time::timeout(limit, async {
        loop {
            if client.status().await.is_some_and(|s| s.active.is_some_and(|a| a.ready)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .is_ok()
}
