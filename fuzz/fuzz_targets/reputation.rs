#![no_main]
//! Arquivo de reputação em disco (não confiável): importar nunca entra em pânico, e o que
//! entra sai de novo importável com valores finitos.

use clam::reputation::{Reputation, ReputationConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut r) = Reputation::import(ReputationConfig::default(), data, 0) {
        let out = r.export(0);
        assert!(Reputation::import(ReputationConfig::default(), &out, 0).is_ok());
    }
});
