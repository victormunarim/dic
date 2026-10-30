#![no_main]
//! Atualização de endereço, apresentação, custódia e caixa de correio, remoção, confirmação e itens da DHT: os `open_*` nunca entram em pânico
//! com bytes quaisquer, e o que o dono sela abre igual para o contato (e só para ele).

use arbitrary::Arbitrary;
use clam::{
    mailbox::Custody,
    media::update::{
        AddressRecord, Chat, CUSTODY_DATA, Intro, MAIL_SLOTS, Reach, invite_mail_item, join_custody, mail_author, mail_item, open_mail_item, seal_custody, mail_slot, mailbox_master, mailbox_target, reply_master, open_ack, open_custody, open_intro, open_mail, open_record, open_chat, open_removal, seal_chat, open_server_record, open_update, seal_ack, seal_intro, seal_record, seal_removal,
        seal_server_record, seal_update, valid_addr,
    },
    x25519_dalek::{PublicKey, StaticSecret},
};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Fields {
    seq: u64,
    addr: String,
    name: String,
    onion: [u8; 32],
    link: [u8; 32],
}

#[derive(Arbitrary, Debug)]
enum Input {
    Raw(Vec<u8>),
    /// Quadro com a etiqueta e a chave do autor certas e o resto arbitrário: chega até a AEAD.
    Framed { tag: u8, rest: Vec<u8> },
    Sealed(Fields),
}

fuzz_target!(|input: Input| {
    let me = StaticSecret::from([1u8; 32]);
    let peer = StaticSecret::from([2u8; 32]);
    let (me_pk, peer_pk) = (PublicKey::from(&me), PublicKey::from(&peer));

    match input {
        Input::Raw(data) => {
            let _ = open_update(&me, [peer_pk.to_bytes()], &data);
            let _ = open_intro(&me, &data);
            let _ = open_custody(&me, &data);
            let _ = open_mail(&me, &data, 1);
            let _ = open_mail_item(&me, &data, 1);
            let _ = (mail_item(&data), mail_author(&data));
            // Resposta achada no buraco de um item: o guardião só a guarda se for uma apresentação inteira; o buraco é determinístico.
            assert_eq!(reply_master(&data), reply_master(&data));
            let _ = Custody::new().accept_reply([5; 32], 10, [6; 32], &data, 10);
            let chunks: Vec<[u8; CUSTODY_DATA]> = data.chunks_exact(CUSTODY_DATA).filter_map(|c| c.try_into().ok()).collect();
            let _ = join_custody(&chunks);
            let _ = open_removal(&me, &data, 1);
            let _ = open_chat(&me, &data, 1);
            let _ = open_ack(&me, &peer_pk, 1, &data);
            let _ = open_record(&me, &peer_pk, &data);
            let _ = open_server_record(&[3; 32], &[4; 16], &peer_pk.to_bytes(), &data);
        }
        Input::Framed { tag, rest } => {
            let mut frame = vec![tag];
            frame.extend_from_slice(peer_pk.as_bytes());
            frame.extend_from_slice(&rest);
            frame.resize(200, 0);
            let _ = open_update(&me, [peer_pk.to_bytes()], &frame);
            let _ = open_intro(&me, &frame);
            let _ = open_custody(&me, &frame);
            let _ = open_mail(&me, &frame, 1);
            let _ = open_mail_item(&me, &frame, 1);
            let _ = open_removal(&me, &frame, 1);
            let _ = open_chat(&me, &frame, 1);
            let _ = open_ack(&me, &peer_pk, 1, &frame);
        }
        Input::Sealed(f) => {
            let record = AddressRecord { seq: f.seq, addr: f.addr.clone(), onion: f.onion, link: f.link };
            let fits = valid_addr(&f.addr);
            // Sela de `peer` para `me`; `me` abre.
            match seal_update(&peer, &me_pk, &record) {
                Ok(frame) => {
                    let opened = open_update(&me, [peer_pk.to_bytes()], &frame);
                    if fits {
                        assert_eq!(opened, Some((peer_pk.to_bytes(), record.clone())));
                    }
                    assert_eq!(open_update(&me, [[9; 32]], &frame), None, "só contato abre");
                }
                Err(_) => assert!(!fits || f.addr.is_empty() || f.addr.len() > 60),
            }
            if let Ok(value) = seal_record(&peer, &me_pk, &record) {
                if fits {
                    assert_eq!(open_record(&me, &peer_pk, &value), Some(record.clone()));
                }
            }
            if let Ok(value) = seal_server_record(&[3; 32], &[4; 16], &peer_pk.to_bytes(), &record) {
                if fits {
                    assert_eq!(open_server_record(&[3; 32], &[4; 16], &peer_pk.to_bytes(), &value), Some(record.clone()));
                }
            }
            let intro = Intro { seq: f.seq, name: f.name, reach: Reach { addr: f.addr, onion: f.onion, link: f.link } };
            if let Ok(frame) = seal_intro(&peer, &me_pk, &intro) {
                if let Some((from, got)) = open_intro(&me, &frame) {
                    assert_eq!(from, peer_pk.to_bytes());
                    assert!(valid_addr(&got.reach.addr) && !got.name.chars().any(char::is_control));
                    assert_eq!(got.seq, intro.seq);
                }
            }
            if let Ok(frame) = seal_removal(&peer, &me_pk, f.seq) {
                assert_eq!(open_removal(&me, &frame, f.seq), Some((peer_pk.to_bytes(), f.seq)));
                assert_eq!(open_removal(&[9u8; 32].into(), &frame, f.seq), None, "só o destinatário abre");
            }
            if let Ok(frame) = seal_chat(&peer, &me_pk, &Chat { id: f.seq, text: intro.name.clone() }) {
                let (from, got) = open_chat(&me, &frame, f.seq).expect("chat abre");
                assert_eq!((from, got.id, got.text), (peer_pk.to_bytes(), f.seq, intro.name.clone()));
                assert_eq!(open_chat(&[9u8; 32].into(), &frame, f.seq), None, "só o destinatário abre");
            }
            if let Ok(mail) = seal_intro(&peer, &me_pk, &intro) {
                let master = mailbox_master(&me_pk.to_bytes());
                assert_eq!(mailbox_target(&master, f.seq, 0), mailbox_target(&master, f.seq, 0));
                assert!(mail_slot(&mail) < MAIL_SLOTS);
                if let Ok(frames) = seal_custody(&peer, &me_pk, f.seq, &master, &mail) {
                    let mut parts = vec![[0u8; CUSTODY_DATA]; frames.len()];
                    for (frame, slot) in frames.iter().zip(parts.iter_mut()) {
                        let (from, part) = open_custody(&me, frame).expect("custódia abre");
                        assert_eq!((from, part.id), (peer_pk.to_bytes(), f.seq));
                        *slot = part.data;
                    }
                    assert_eq!(join_custody(&parts), Some((master, mail.to_vec())));
                }
            }
            // Convite de servidor na caixa: de 80 a 997 bytes, em até CUSTODY_MAX_PARTS quadros.
            let len = 80 + (f.seq % 918) as usize;
            let raw = [&[1u8; 16][..], &peer_pk.to_bytes(), &vec![f.seq as u8; len - 48]].concat();
            if let Some(item) = invite_mail_item(&raw) {
                let master = mailbox_master(&me_pk.to_bytes());
                assert!(mail_slot(&item) < MAIL_SLOTS);
                assert_eq!(mail_author(&item), Some(peer_pk.to_bytes()));
                let frames = seal_custody(&peer, &me_pk, f.seq, &master, &item).expect("convite sela");
                let mut parts = vec![[0u8; CUSTODY_DATA]; frames.len()];
                for (frame, slot) in frames.iter().zip(parts.iter_mut()) {
                    *slot = open_custody(&me, frame).expect("custódia abre").1.data;
                }
                assert_eq!(join_custody(&parts), Some((master, item)));
            }
            let ack = seal_ack(&peer, &me_pk, f.seq).expect("ack sela");
            assert!(open_ack(&me, &peer_pk, f.seq, &ack));
            assert!(!open_ack(&me, &peer_pk, f.seq.wrapping_add(1), &ack));
        }
    }
});
