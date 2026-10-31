//! Modos de caminho da mídia, decididos pela opção "2 relays" (veja `docs/DOIS-RELAYS.md`).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathMode {
    /// você → relay A → relay B → contato. A só vê o seu IP; B só o do contato.
    #[default]
    TwoHop,
    /// você → contato. O contato vê o seu IP.
    Direct,
}

impl PathMode {
    /// Número de relays no caminho.
    pub fn relay_count(self) -> usize {
        match self {
            PathMode::TwoHop => 2,
            PathMode::Direct => 0,
        }
    }

    /// Alvo de latência de voz (ms) para o modo.
    pub fn voice_latency_target_ms(self) -> u32 {
        match self {
            PathMode::Direct => 150,
            PathMode::TwoHop => 200,
        }
    }
}

impl PathMode {
    /// O caminho que a opção "2 relays" pede: ligada, por relays de terceiros; desligada, direto ao contato.
    /// Não há exceção por contato nem por tipo de mídia: a opção vale para todo fluxo.
    #[must_use]
    pub fn from_two_relays(on: bool) -> Self {
        if on { PathMode::TwoHop } else { PathMode::Direct }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_two_hop() {
        assert_eq!(PathMode::default(), PathMode::TwoHop);
    }

    #[test]
    fn the_option_decides_the_path() {
        assert_eq!(PathMode::from_two_relays(true), PathMode::TwoHop);
        assert_eq!(PathMode::from_two_relays(false), PathMode::Direct);
    }

    #[test]
    fn relay_counts_and_targets() {
        assert_eq!(PathMode::TwoHop.relay_count(), 2);
        assert_eq!(PathMode::Direct.relay_count(), 0);
        assert_eq!(PathMode::Direct.voice_latency_target_ms(), 150);
        assert_eq!(PathMode::TwoHop.voice_latency_target_ms(), 200);
    }
}
