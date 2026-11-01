#![no_main]
//! Tabelas dos pontos de introdução e de encontro: qualquer sequência de pedidos deixa as tabelas dentro dos
//! limites e nunca entra em pânico. O tempo avança pelos próprios bytes.

use std::time::{Duration, Instant};

use clam::hidden::{IntroConn, IntroPoint, IntroRequest, MAX_SERVICES, MAX_WAITING, Messages, RvPoint, RvRequest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let start = Instant::now();
    let mut now = start;
    let mut intro = IntroPoint::<u8>::new();
    let mut rv = RvPoint::<u8>::new();
    let mut conns = [IntroConn::Fresh, IntroConn::Fresh, IntroConn::Fresh];
    let mut messages = Messages::new();
    for chunk in data.chunks(200) {
        let Ok(bodies) = messages.push(chunk) else { return };
        for body in bodies {
            // O tempo anda de 0 a 255 s por mensagem, a partir do último byte do corpo.
            now += Duration::from_secs(u64::from(body.last().copied().unwrap_or(0)));
            let slot = usize::from(body.first().copied().unwrap_or(0)) % conns.len();
            if let Some(request) = IntroRequest::parse(&body) {
                if let Some(state) = conns.get_mut(slot) {
                    intro.handle(state, slot as u64, &(slot as u8), request, now, [slot as u8; 32]);
                }
            } else if let Some(request) = RvRequest::parse(&body) {
                match request {
                    RvRequest::Establish { cookie } => {
                        rv.establish(cookie, 0, now);
                    }
                    RvRequest::Join { cookie } => {
                        rv.join(&cookie, now);
                    }
                }
            } else if let Some(state) = conns.get(slot) {
                intro.leave(state, slot as u64);
            }
            assert!(intro.len() <= MAX_SERVICES && rv.len() <= MAX_WAITING);
        }
    }
});
