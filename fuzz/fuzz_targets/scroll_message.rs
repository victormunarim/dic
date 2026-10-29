#![no_main]
//! Mensagens de rolagem, de banda e de rolagem de região da tela: bytes quaisquer nunca derrubam o `parse`, e o que passa pelo
//! `parse` nunca faz `apply` estourar nem alterar uma imagem a que não se aplica.

use clam::{scroll, video::RawFrame};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let canvas = RawFrame::new(64, 48, vec![7u8; 64 * 48 * 4]).expect("medidas válidas");
    if let Some(m) = scroll::parse(data) {
        let strip = RawFrame::new(64, 16, vec![9u8; 64 * 16 * 4]).expect("medidas válidas");
        let mut c = canvas.clone();
        if !scroll::apply(&mut c, m.dy, &strip) {
            assert_eq!(c, canvas);
        }
    }
    if let Some(p) = scroll::parse_patch(data) {
        let strip = RawFrame::new(64, 16, vec![9u8; 64 * 16 * 4]).expect("medidas válidas");
        let mut c = canvas.clone();
        if !scroll::apply_patch(&mut c, p.y, p.rows, &strip) {
            assert_eq!(c, canvas);
        }
    }
    if let Some(r) = scroll::parse_region(data) {
        // Faixa da largura do retângulo (o caso que cabe) e de uma largura fixa (o que não cabe).
        for w in [(r.rect.w.min(256) & !1).max(2), 64] {
            let strip = RawFrame::new(w, 64, vec![9u8; w * 64 * 4]).expect("medidas válidas");
            let mut c = canvas.clone();
            if !scroll::apply_region(&mut c, r.rect, r.dy, &strip) {
                assert_eq!(c, canvas);
            }
        }
    }
});
