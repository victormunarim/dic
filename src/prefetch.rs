//! Captura adiantada: a fonte da tela roda numa thread própria, no ritmo do vídeo, e o codificador
//! pega sempre a imagem mais nova pronta.
//!
//! Capturar e codificar na mesma thread soma os dois tempos (≈ 10 ms de captura + ≈ 30 ms de openh264
//! em 1080p estouram os 33 ms de 30 fps e a transmissão cai para ≈ 14 quadros/s); em threads separadas
//! o ritmo passa a ser o do mais lento. Se o codificador atrasa, a imagem velha é trocada pela nova em
//! vez de empilhar: nunca há fila nem atraso acumulado.

use crate::{sync::LockExt, video::{RawFrame, ScreenSource}};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Quanto [`ScreenSource::grab`] espera por uma imagem nova antes de devolver `None`.
const WAIT: Duration = Duration::from_millis(10);

type Slot = Arc<(Mutex<Option<RawFrame>>, Condvar)>;

/// Envolve uma [`ScreenSource`] e a captura numa thread à parte.
pub struct Prefetch {
    latest: Slot,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Prefetch {
    /// `fps`: quantas vezes por segundo a fonte é consultada.
    pub fn new(mut source: Box<dyn ScreenSource + Send>, fps: f32) -> Self {
        let latest: Slot = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::new(AtomicU64::new(0));
        let every = Duration::from_secs_f32(1.0 / fps.max(1.0));
        let thread = {
            let (latest, stop, captured) = (latest.clone(), stop.clone(), captured.clone());
            std::thread::spawn(move || {
                let mut next = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    if let Some(frame) = source.grab() {
                        captured.fetch_add(1, Ordering::Relaxed);
                        let (slot, news) = &*latest;
                        *slot.lock_unpoisoned() = Some(frame);
                        news.notify_one();
                    }
                    next += every;
                    // Atrasada (a captura levou mais que o período): recomeça daqui, sem rajada para alcançar.
                    let now = Instant::now();
                    if next < now {
                        next = now;
                    }
                    std::thread::sleep(next - now);
                }
            })
        };
        Self { latest, stop, captured, thread: Some(thread) }
    }
}

impl ScreenSource for Prefetch {
    fn grab(&mut self) -> Option<RawFrame> {
        let (slot, news) = &*self.latest;
        let guard = slot.lock_unpoisoned();
        let (mut guard, _) = news.wait_timeout_while(guard, WAIT, |f| f.is_none()).ok()?;
        guard.take()
    }

    fn captured(&self) -> u64 {
        self.captured.load(Ordering::Relaxed)
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fonte lenta que numera as imagens pela largura.
    struct Slow(usize, Duration);

    impl ScreenSource for Slow {
        fn grab(&mut self) -> Option<RawFrame> {
            std::thread::sleep(self.1);
            self.0 += 1;
            RawFrame::new(self.0 * 2, 2, vec![0; self.0 * 2 * 2 * 4]).ok()
        }
    }

    fn wait_for_frame(p: &mut Prefetch) -> RawFrame {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(f) = p.grab() {
                return f;
            }
            assert!(Instant::now() < end, "nenhuma imagem em 5 s");
        }
    }

    #[test]
    fn the_encoder_takes_the_newest_image_and_never_the_same_one_twice() {
        let mut p = Prefetch::new(Box::new(Slow(0, Duration::from_millis(5))), 200.0);
        let first = wait_for_frame(&mut p);
        // O codificador demora: as imagens velhas são trocadas pela mais nova, não empilhadas.
        std::thread::sleep(Duration::from_millis(100));
        let second = wait_for_frame(&mut p);
        assert!(second.width >= first.width + 4, "{} depois de {}", second.width, first.width);
        assert!(p.latest.0.lock_unpoisoned().is_none(), "a imagem entregue sai do escaninho");
    }

    #[test]
    fn it_counts_the_images_the_source_produced() {
        let mut p = Prefetch::new(Box::new(Slow(0, Duration::from_millis(1))), 200.0);
        wait_for_frame(&mut p);
        let end = Instant::now() + Duration::from_secs(5);
        while p.captured() < 3 {
            assert!(Instant::now() < end, "a fonte não produziu 3 imagens em 5 s");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn dropping_stops_the_capture_thread() {
        let p = Prefetch::new(Box::new(Slow(0, Duration::from_millis(1))), 100.0);
        drop(p);
    }
}
