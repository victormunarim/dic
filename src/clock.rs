//! Hora usada nos convites (feature `net`).
//!
//! O convite de chamada não depende mais do relógio de quem o recebe (a repetição é barrada por uma
//! memória de convites vistos), então a hora errada de uma máquina não impede a chamada, com ou sem
//! diretório. Esta hora ainda segue no convite, para telefones de versão anterior que a conferem em 60 s:
//! cada contato com o diretório, que manda a hora dele no `hello`, ajusta um desvio comum, e todos que
//! falam com o mesmo diretório passam a usar a hora dele, não a do sistema.

use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static OFFSET: AtomicI64 = AtomicI64::new(0);

fn system_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Segundos Unix pela hora do diretório (ou do sistema, se nunca falou com um).
pub fn now_secs() -> u64 {
    (system_secs() + OFFSET.load(Ordering::Relaxed)).max(0) as u64
}

/// Registra a hora que o diretório informou agora.
pub fn sync_to(server_secs: u64) {
    OFFSET.store(server_secs as i64 - system_secs(), Ordering::Relaxed);
}

/// Desvio atual em relação ao relógio do sistema, em segundos (positivo: o sistema está atrasado).
pub fn offset_secs() -> i64 {
    OFFSET.load(Ordering::Relaxed)
}
