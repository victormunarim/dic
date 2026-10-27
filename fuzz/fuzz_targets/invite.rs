#![no_main]
//! Convites: `Invite::open` nunca entra em pânico, com texto qualquer, com bytes atrás de um cabeçalho que
//! chega até a decifragem ou com um convite selado por quem convida com campos fora do formato (endereço
//! comprido, nome inválido). O que abre é válido e, selado de novo, abre igual.

use arbitrary::Arbitrary;
use clam::{
    media::update::{Reach, valid_addr},
    server::{
        invite::{Hint, Invite, PREFIX},
        valid_name,
    },
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Fields {
    server: [u8; 16],
    name: String,
    epoch: u16,
    secret: [u8; 32],
    owner_vk: [u8; 32],
    addr: String,
    onion: [u8; 32],
    link: [u8; 32],
    hints: Vec<([u8; 32], String)>,
}

#[derive(Arbitrary, Debug)]
enum Input {
    Text(String),
    /// Cabeçalho certo (servidor, quem convida, convidado) e bytes arbitrários depois: chega até a AEAD.
    Body([u8; 16], Vec<u8>),
    Sealed(Fields),
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fuzz_target!(|input: Input| {
    let inviter = StaticSecret::from([1u8; 32]);
    let me = StaticSecret::from([2u8; 32]);
    let (inviter_pk, me_pk) = (PublicKey::from(&inviter).to_bytes(), PublicKey::from(&me).to_bytes());

    match input {
        Input::Text(t) => {
            let _ = Invite::open(&t, &me);
        }
        Input::Body(server, body) => {
            let mut raw = server.to_vec();
            raw.extend_from_slice(&inviter_pk);
            raw.extend_from_slice(&me_pk);
            raw.extend_from_slice(&body);
            let _ = Invite::open(&format!("{PREFIX}{}", hex(&raw)), &me);
        }
        Input::Sealed(f) => {
            let hints: Vec<Hint> = f.hints.into_iter().map(|(key, addr)| Hint { key, reach: Reach { addr, onion: key, link: key } }).collect();
            let invite = Invite {
                server: f.server,
                name: f.name,
                epoch: f.epoch,
                secret: f.secret,
                owner_vk: f.owner_vk,
                invitee: me_pk,
                inviter: Hint { key: inviter_pk, reach: Reach { addr: f.addr, onion: f.onion, link: f.link } },
                hints,
            };
            let Some(text) = invite.seal(&inviter) else { return };
            let opened = Invite::open(&text, &me);
            let fits = valid_name(&invite.name) && valid_addr(&invite.inviter.reach.addr) && invite.hints.iter().all(|h| valid_addr(&h.reach.addr));
            if fits {
                assert_eq!(opened.as_ref(), Ok(&invite), "convite dentro do formato deve abrir igual");
            }
            if let Ok(got) = opened {
                assert!(valid_name(&got.name) && valid_addr(&got.inviter.reach.addr) && got.hints.iter().all(|h| valid_addr(&h.reach.addr)));
                let again = got.seal(&inviter).expect("convite aberto deve selar de novo");
                assert_eq!(Invite::open(&again, &me).as_ref(), Ok(&got));
            }
        }
    }
});
