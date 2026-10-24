//! Modos de caminho da mídia. O padrão é o privado, de 2 saltos.

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathMode {
    /// você → relay A → relay B → contato. A só vê o seu IP; B só o do contato.
    #[default]
    TwoHop,
    /// você → relay → contato. O relay vê os dois IPs.
    OneHop,
    /// você → contato. O contato vê o seu IP.
    Direct,
}

impl PathMode {
    /// Número de relays no caminho.
    pub fn relay_count(self) -> usize {
        match self {
            PathMode::TwoHop => 2,
            PathMode::OneHop => 1,
            PathMode::Direct => 0,
        }
    }

    /// Alvo de latência de voz (ms) para o modo.
    pub fn voice_latency_target_ms(self) -> u32 {
        match self {
            PathMode::Direct => 150,
            PathMode::OneHop | PathMode::TwoHop => 200,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathError {
    #[error("modo direto expõe o seu IP ao contato: exige contato de confiança e aviso confirmado")]
    DirectNotAllowed,
}

/// Pedido de modo feito pelo usuário para um contato.
#[derive(Debug, Clone, Copy)]
pub struct PathRequest {
    pub mode: PathMode,
    pub contact_trusted: bool,
    pub ip_exposure_warning_acked: bool,
}

impl PathRequest {
    /// Pedido padrão: privado, sem exceções.
    pub fn private() -> Self {
        Self {
            mode: PathMode::TwoHop,
            contact_trusted: false,
            ip_exposure_warning_acked: false,
        }
    }

    /// Valida o pedido; o modo direto só passa com confiança e aviso confirmado.
    pub fn resolve(self) -> Result<PathMode, PathError> {
        if self.mode == PathMode::Direct
            && !(self.contact_trusted && self.ip_exposure_warning_acked)
        {
            return Err(PathError::DirectNotAllowed);
        }
        Ok(self.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_two_hop() {
        assert_eq!(PathMode::default(), PathMode::TwoHop);
        assert_eq!(PathRequest::private().resolve(), Ok(PathMode::TwoHop));
    }

    #[test]
    fn direct_requires_trust_and_ack() {
        let base = PathRequest { mode: PathMode::Direct, contact_trusted: false, ip_exposure_warning_acked: false };
        assert_eq!(base.resolve(), Err(PathError::DirectNotAllowed));
        assert_eq!(
            PathRequest { contact_trusted: true, ..base }.resolve(),
            Err(PathError::DirectNotAllowed)
        );
        assert_eq!(
            PathRequest { contact_trusted: true, ip_exposure_warning_acked: true, ..base }.resolve(),
            Ok(PathMode::Direct)
        );
    }

    #[test]
    fn relay_counts_and_targets() {
        assert_eq!(PathMode::TwoHop.relay_count(), 2);
        assert_eq!(PathMode::OneHop.relay_count(), 1);
        assert_eq!(PathMode::Direct.relay_count(), 0);
        assert_eq!(PathMode::Direct.voice_latency_target_ms(), 150);
        assert_eq!(PathMode::TwoHop.voice_latency_target_ms(), 200);
    }
}
