//! `Mutex` que sobrevive a um pânico alheio.
//!
//! O `std` envenena o `Mutex` quando uma thread entra em pânico segurando o guard, e daí em diante todo
//! `lock().unwrap()` também entra em pânico: um bug numa tarefa derrubaria o relay, o telefone e a interface
//! juntos. As estruturas guardadas aqui (filas, mapas, contadores, estados de chamada) nunca ficam inválidas
//! no nível da memória, e o pior caso de seguir em frente é um estado atrasado de uma operação que o pânico
//! interrompeu, que a próxima mensagem ou o próximo tick corrige. Por isso o programa segue com o valor que
//! estava lá em vez de propagar o pânico.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// `lock()` que ignora o envenenamento.
pub trait LockExt<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_poisoned_mutex_still_locks_with_the_last_value() {
        let m = Arc::new(Mutex::new(vec![1, 2]));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock_unpoisoned();
            g.push(3);
            panic!("pânico com o guard na mão");
        })
        .join();
        assert!(m.lock().is_err(), "o std envenenou o mutex");
        assert_eq!(*m.lock_unpoisoned(), vec![1, 2, 3]);
        m.lock_unpoisoned().push(4);
        assert_eq!(m.lock_unpoisoned().len(), 4);
    }
}
