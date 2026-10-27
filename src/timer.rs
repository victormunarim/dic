//! Resolução do relógio do sistema.
//!
//! No Windows, o temporizador padrão tem passo de ~15,6 ms: um `interval` de 20 ms dispara a cada ~31 ms
//! e um de 10 ms a cada ~15,6 ms. A voz (um pacote a cada 20 ms) e o cover (10/20 ms) ficariam com cerca
//! de 60% da cadência pretendida. [`fine_timers`] pede passo de 1 ms (`timeBeginPeriod`), uma vez por processo;
//! o sistema desfaz o pedido quando o processo termina. Nas demais plataformas não faz nada.

/// Pede temporizadores de 1 ms ao sistema. Idempotente e barato; chamado pelos construtores de rede.
#[allow(unsafe_code)]
pub fn fine_timers() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            #[link(name = "winmm")]
            unsafe extern "system" {
                fn timeBeginPeriod(period_ms: u32) -> u32;
            }
            // SAFETY: função da winmm sem ponteiros; o argumento é um valor em ms.
            unsafe {
                timeBeginPeriod(1);
            }
        });
    }
}
