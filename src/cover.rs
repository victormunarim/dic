//! Perfis de custo do cover traffic.
//!
//! Um perfil escolhe o intervalo do padding de **link** (um quadro de 279 bytes por
//! intervalo, em cada sentido) e o do cover de **circuito** (uma célula por intervalo).
//! Quanto menor o intervalo, mais banda e menos atraso: o quadro real espera em média
//! meio intervalo por link. O circuito nunca vai abaixo de 20 ms, a cadência de um quadro
//! Opus; o link nunca fica mais lento que o circuito, senão a fila de saída cresce.
//!
//! O perfil é a parte *sans-IO*: o transporte (`CircuitClient::set_profile`) o aplica,
//! inclusive em links já abertos, e o aplicativo decide quando trocar (Wi-Fi para dados
//! móveis, bateria baixa) com [`CoverProfile::for_budget`].

use std::{fmt, str::FromStr, time::Duration};

/// Tamanho de um quadro de link no fio, em bytes.
pub const FRAME_BYTES: u64 = 279;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoverProfile {
    /// Sem cover: nada de banda extra, tudo visível a quem observa.
    Off,
    /// Link e circuito a 20 ms: ≈ 112 kbps por sentido. Esconde o ritmo dos relays e de quem
    /// vê o link, com mais atraso (até 20 ms por link).
    Economy,
    /// Link a 10 ms e circuito a 20 ms: ≈ 223 kbps por sentido. É o padrão.
    Standard,
    /// Link a 5 ms e circuito a 20 ms: ≈ 446 kbps por sentido, com o menor atraso.
    Strong,
}

impl CoverProfile {
    pub const ALL: [CoverProfile; 4] = [Self::Off, Self::Economy, Self::Standard, Self::Strong];

    /// Intervalo do padding de link; `None` desliga.
    pub fn link_interval(self) -> Option<Duration> {
        match self {
            Self::Off => None,
            Self::Economy => Some(Duration::from_millis(20)),
            Self::Standard => Some(Duration::from_millis(10)),
            Self::Strong => Some(Duration::from_millis(5)),
        }
    }

    /// Intervalo do cover de circuito; `None` desliga.
    pub fn circuit_interval(self) -> Option<Duration> {
        match self {
            Self::Off => None,
            _ => Some(Duration::from_millis(20)),
        }
    }

    /// Banda do cliente em kbps, por sentido, no link com o primeiro relay. O cover de
    /// circuito vai dentro desses quadros e não soma.
    pub fn cost_kbps(self) -> f64 {
        self.link_interval().map_or(0.0, |d| FRAME_BYTES as f64 * 8.0 / d.as_secs_f64() / 1000.0)
    }

    /// O perfil mais forte que cabe em `budget_kbps` por sentido (reserve também a banda da
    /// voz, ≈ 100 kbps, fora deste orçamento). Abaixo do custo de `Economy` devolve `Off`:
    /// um cover pela metade não esconde nada.
    pub fn for_budget(budget_kbps: f64) -> Self {
        Self::ALL.into_iter().rev().find(|p| p.cost_kbps() <= budget_kbps).unwrap_or(Self::Off)
    }
}

impl fmt::Display for CoverProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Economy => "economy",
            Self::Standard => "standard",
            Self::Strong => "strong",
        })
    }
}

impl FromStr for CoverProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|p| p.to_string() == s)
            .ok_or_else(|| format!("perfil '{s}' desconhecido (off, economy, standard, strong)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_are_ordered_and_match_the_readme() {
        let c: Vec<f64> = CoverProfile::ALL.iter().map(|p| p.cost_kbps()).collect();
        assert!(c.windows(2).all(|w| w[0] < w[1]), "{c:?}");
        assert_eq!(c[0], 0.0);
        assert!((c[2] - 223.2).abs() < 0.1);
    }

    #[test]
    fn link_is_never_slower_than_circuit() {
        for p in CoverProfile::ALL {
            if let (Some(l), Some(c)) = (p.link_interval(), p.circuit_interval()) {
                assert!(l <= c, "{p}: o link precisa escoar a cadência do circuito");
                assert!(c >= Duration::from_millis(20), "{p}: abaixo da cadência da voz");
            }
        }
    }

    #[test]
    fn budget_picks_the_strongest_that_fits() {
        assert_eq!(CoverProfile::for_budget(10_000.0), CoverProfile::Strong);
        assert_eq!(CoverProfile::for_budget(446.4), CoverProfile::Strong);
        assert_eq!(CoverProfile::for_budget(300.0), CoverProfile::Standard);
        assert_eq!(CoverProfile::for_budget(150.0), CoverProfile::Economy);
        assert_eq!(CoverProfile::for_budget(100.0), CoverProfile::Off);
        assert_eq!(CoverProfile::for_budget(-1.0), CoverProfile::Off);
    }

    #[test]
    fn names_roundtrip() {
        for p in CoverProfile::ALL {
            assert_eq!(p.to_string().parse::<CoverProfile>(), Ok(p));
        }
        assert!("forte".parse::<CoverProfile>().is_err());
    }
}
