//! Configuração salva, formulário e funções puras (códigos, contatos, salas, chaveiro).

use std::{collections::HashMap, path::PathBuf};

use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return Err(format!("chave deve ter 64 caracteres hex, veio {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}


#[derive(Default, Clone)]
pub(crate) struct Form {
    pub(crate) secret: String,
    pub(crate) directory: String,
    /// Uma linha por contato: `[nome] chave-pública-hex`.
    pub(crate) contacts: String,
    pub(crate) peer: String,
    pub(crate) public_addr: String,
    pub(crate) use_audio: bool,
    /// Microfone e alto-falante escolhidos (vazio = padrão do sistema).
    pub(crate) input_dev: String,
    pub(crate) output_dev: String,
    /// Apelidos, um por linha: `chave-hex apelido`. Valem só neste computador.
    pub(crate) nicks: String,
    /// Salas guardadas, uma por linha: `nome segredo-hex`.
    pub(crate) rooms: String,
    pub(crate) room_name: String,
    pub(crate) room_code: String,
    pub(crate) new_name: String,
    pub(crate) new_code: String,
}

/// Código de contato: a chave pública (64 hex), com o diretório opcional depois de `@`
/// (`<chave>@host:porta`). Devolve a chave e o diretório, se houver.
pub(crate) fn split_code(t: &str) -> Option<([u8; 32], Option<String>)> {
    let (key, dir) = match t.trim().split_once('@') {
        Some((k, d)) => (k, Some(d.trim().to_string()).filter(|d| !d.is_empty())),
        None => (t.trim(), None),
    };
    unhex(key).ok().map(|k| (k, dir))
}

/// Contatos da lista: `(nome, chave)`. Linhas sem uma chave de 64 hex são ignoradas.
pub(crate) fn parse_contacts(text: &str) -> Vec<(String, [u8; 32])> {
    text.lines()
        .filter_map(|l| {
            let mut name = vec![];
            let mut key = None;
            for t in l.split_whitespace() {
                match split_code(t) {
                    Some((k, _)) => key = Some(k),
                    None => name.push(t),
                }
            }
            key.map(|k| (if name.is_empty() { hex(&k)[..8].to_string() } else { name.join(" ") }, k))
        })
        .collect()
}

/// Apelidos: `chave-hex apelido` por linha.
pub(crate) fn parse_nicks(text: &str) -> Vec<(String, [u8; 32])> {
    text.lines()
        .filter_map(|l| {
            let (key, nick) = l.trim().split_once(' ')?;
            Some((nick.trim().to_string(), unhex(key).ok()?)).filter(|(n, _)| !n.is_empty())
        })
        .collect()
}

/// Sala guardada: o dono (quem criou) é quem pode remover membros; salas antigas não têm dono registrado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedRoom {
    pub(crate) name: String,
    pub(crate) secret: [u8; 32],
    pub(crate) owner: Option<[u8; 32]>,
}

/// Código de sala: `segredo[:dono][@host:porta]`, tudo em hex. Devolve o segredo e o dono, se houver.
pub(crate) fn split_room_code(t: &str) -> Option<([u8; 32], Option<[u8; 32]>)> {
    let t = t.trim();
    let t = t.split_once('@').map_or(t, |(k, _)| k);
    match t.split_once(':') {
        Some((s, o)) => Some((unhex(s).ok()?, Some(unhex(o).ok()?))),
        None => Some((unhex(t).ok()?, None)),
    }
}

/// Salas guardadas, uma por linha: `nome segredo[:dono]`. Linhas sem um segredo de 64 hex são ignoradas.
pub(crate) fn parse_rooms(text: &str) -> Vec<SavedRoom> {
    text.lines()
        .filter_map(|l| {
            let (name, code) = l.trim().rsplit_once(' ')?;
            let (secret, owner) = split_room_code(code)?;
            Some(SavedRoom { name: name.trim().to_string(), secret, owner })
        })
        .collect()
}

pub(crate) fn format_rooms(rooms: &[SavedRoom]) -> String {
    rooms.iter().map(|r| format!("{} {}", r.name, r.code())).collect::<Vec<_>>().join("\n")
}

impl SavedRoom {
    /// `segredo[:dono]`, o que vai para o arquivo e para quem recebe o convite.
    pub(crate) fn code(&self) -> String {
        match &self.owner {
            Some(o) => format!("{}:{}", hex(&self.secret), hex(o)),
            None => hex(&self.secret),
        }
    }
}

/// Valor de `secret=` no arquivo quando a chave de verdade está no chaveiro do sistema.
pub(crate) const KEYRING_MARK: &str = "@keyring";

pub(crate) fn vault() -> Option<keyring::Entry> {
    keyring::Entry::new("clam", "identity").ok()
}

pub(crate) fn vault_get() -> Option<String> {
    vault()?.get_password().ok().filter(|s| unhex(s).is_ok())
}

/// Guarda a identidade no chaveiro e confere que dá para ler de volta.
pub(crate) fn vault_set(secret: &str) -> bool {
    vault().is_some_and(|e| e.set_password(secret).is_ok()) && vault_get().as_deref() == Some(secret)
}

pub(crate) fn conf_path() -> PathBuf {
    // No Windows `HOME` só existe em shells estilo Unix (Git Bash) e vale algo como `/c/Users/x`, que o
    // sistema não resolve: a gravação falhava calada e a configuração sumia a cada abertura.
    let vars: [&str; 2] = if cfg!(windows) { ["USERPROFILE", "HOME"] } else { ["HOME", "USERPROFILE"] };
    let home = vars.iter().find_map(|v| std::env::var_os(v).filter(|h| !h.is_empty())).map(PathBuf::from).unwrap_or_default();
    home.join(".clam-gui.conf")
}

/// Últimos endereços de relay dos contatos, num arquivo à parte (`chave addr onion link` por linha).
pub(crate) fn known_path() -> PathBuf {
    conf_path().with_file_name(".clam-gui.known")
}

pub(crate) fn load_known() -> Vec<([u8; 32], clam::directory::RelayInfo)> {
    let Ok(text) = std::fs::read_to_string(known_path()) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let mut p = l.split_whitespace();
            let (k, addr, o, link) = (p.next()?, p.next()?, p.next()?, p.next()?);
            Some((unhex(k).ok()?, clam::directory::RelayInfo { addr: addr.to_string(), onion: unhex(o).ok()?, link: unhex(link).ok()? }))
        })
        .collect()
}

/// Grava `text` em `path` de uma vez só (arquivo temporário + rename) e, no Unix, já com permissão 0600:
/// o conteúdo nunca fica legível por outros nem pela metade.
pub(crate) fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let result = opts.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes())).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub(crate) fn save_known(known: &[([u8; 32], clam::directory::RelayInfo)]) {
    let text: String = known.iter().map(|(k, r)| format!("{} {} {} {}\n", hex(k), r.addr, hex(&r.onion), hex(&r.link))).collect();
    let _ = write_private(&known_path(), &text);
}

impl Form {
    /// Primeira abertura: já nasce com uma identidade, para a pessoa não precisar gerar nada.
    pub(crate) fn first_run() -> Self {
        Form { secret: hex(&StaticSecret::random_from_rng(OsRng).to_bytes()), ..Form::default() }
    }

    pub(crate) fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(conf_path()) else { return Form::first_run() };
        let kv: HashMap<_, _> = text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let get = |k: &str| kv.get(k).cloned().unwrap_or_default();
        let secret = match get("secret").as_str() {
            "" => hex(&StaticSecret::random_from_rng(OsRng).to_bytes()),
            // Guardada no chaveiro do sistema; se ele sumiu, gera outra (a identidade antiga se perde).
            KEYRING_MARK => vault_get().unwrap_or_else(|| hex(&StaticSecret::random_from_rng(OsRng).to_bytes())),
            plain => plain.to_string(),
        };
        Form {
            secret,
            directory: get("directory"),
            contacts: get("contacts").replace(';', "\n"),
            peer: get("peer"),
            public_addr: get("public_addr"),
            use_audio: get("audio") == "1",
            input_dev: get("input_dev"),
            output_dev: get("output_dev"),
            nicks: get("nicks").replace(';', "\n"),
            rooms: get("rooms").replace(';', "\n"),
            ..Form::default()
        }
    }

    pub(crate) fn save(&self) {
        if let Err(e) = self.save_checked() {
            eprintln!("não deu para gravar {}: {e}", conf_path().display());
        }
    }

    /// Como [`save`](Self::save), mas devolve o erro de gravação.
    pub(crate) fn save_checked(&self) -> std::io::Result<()> {
        // Chave de identidade no chaveiro do sistema; só cai para o arquivo se ele não estiver disponível.
        let secret = if vault_set(self.secret.trim()) { KEYRING_MARK } else { self.secret.trim() };
        let text = format!(
            "audio={}\nsecret={}\ndirectory={}\ncontacts={}\npeer={}\npublic_addr={}\nrooms={}\ninput_dev={}\noutput_dev={}\nnicks={}\n",
            u8::from(self.use_audio),
            secret,
            self.directory.trim(),
            self.contacts.trim().replace('\n', ";"),
            self.peer.trim(),
            self.public_addr.trim(),
            self.rooms.trim().replace('\n', ";"),
            self.input_dev.trim(),
            self.output_dev.trim(),
            self.nicks.trim().replace('\n', ";")
        );
        // Contém a chave secreta: só o dono lê.
        write_private(&conf_path(), &text)
    }

    /// Nomes conhecidos: contatos e apelidos (o apelido vale para quem não é contato).
    pub(crate) fn names(&self) -> Vec<(String, [u8; 32])> {
        let mut v = parse_contacts(&self.contacts);
        v.extend(parse_nicks(&self.nicks));
        v
    }

    pub(crate) fn nick(&self, key: &[u8; 32]) -> String {
        parse_nicks(&self.nicks).into_iter().find(|(_, k)| k == key).map(|(n, _)| n).unwrap_or_default()
    }

    /// Define (ou, vazio, remove) o apelido de `key`.
    pub(crate) fn set_nick(&mut self, key: &[u8; 32], nick: &str) {
        let mut v: Vec<(String, [u8; 32])> = parse_nicks(&self.nicks).into_iter().filter(|(_, k)| k != key).collect();
        let nick = nick.trim().replace(['\n', ';'], " ");
        if !nick.is_empty() {
            v.push((nick, *key));
        }
        self.nicks = v.iter().map(|(n, k)| format!("{} {n}", hex(k))).collect::<Vec<_>>().join("\n");
    }

    #[cfg(feature = "audio")]
    pub(crate) fn devices(&self) -> clam::audio::Devices {
        clam::audio::Devices { input: self.input_dev.trim().to_string(), output: self.output_dev.trim().to_string() }
    }

    /// Minha chave pública (a identidade que o diretório e as salas conhecem).
    pub(crate) fn my_key(&self) -> Option<[u8; 32]> {
        Some(PublicKey::from(&self.identity().ok()?).to_bytes())
    }

    /// O dono trocou o segredo de uma sala: guarda o novo no lugar do antigo.
    pub(crate) fn replace_room_secret(&mut self, old: &[u8; 32], new: &[u8; 32]) {
        let mut rooms = parse_rooms(&self.rooms);
        for r in rooms.iter_mut().filter(|r| r.secret == *old) {
            r.secret = *new;
        }
        self.rooms = format_rooms(&rooms);
    }

    pub(crate) fn identity(&self) -> Result<StaticSecret, String> {
        unhex(&self.secret).map(StaticSecret::from).map_err(|e| format!("identidade: {e}"))
    }

    /// Resolve o campo "ligar para": uma chave de 64 hex ou o nome de um contato.
    /// Diretório a usar: o campo, ou o que veio num código de contato (`chave@host:porta`).
    pub(crate) fn directory_addr(&self) -> String {
        let d = self.directory.trim();
        if !d.is_empty() {
            return d.to_string();
        }
        let codes = self.contacts.split_whitespace().chain(self.peer.split_whitespace());
        codes.filter_map(|t| split_code(t).and_then(|(_, d)| d)).next().unwrap_or_default()
    }

    /// Meu código de contato: chave pública e, se conhecido, o diretório.
    pub(crate) fn my_code(&self) -> Option<String> {
        let key = hex(PublicKey::from(&self.identity().ok()?).as_bytes());
        let dir = self.directory_addr();
        Some(if dir.is_empty() { key } else { format!("{key}@{dir}") })
    }

    pub(crate) fn target(&self) -> Result<[u8; 32], String> {
        let p = self.peer.trim();
        if let Some((k, _)) = split_code(p) {
            return Ok(k);
        }
        parse_contacts(&self.contacts)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(p))
            .map(|(_, k)| k)
            .ok_or_else(|| "informe o código de contato (ou chave de 64 hex) ou o nome de um contato".to_string())
    }
}


/// Contatos sem a linha `idx` (contando só as linhas com chave).
pub(crate) fn remove_contact(text: &str, idx: usize) -> String {
    let mut n = 0;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            if parse_contacts(l).is_empty() {
                return true;
            }
            n += 1;
            n - 1 != idx
        })
        .collect();
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_replaces_atomically_with_owner_only_mode() {
        let dir = std::env::temp_dir().join(format!("clam-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conf");
        write_private(&path, "um").unwrap();
        write_private(&path, "dois").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dois");
        assert!(!path.with_extension("tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn contact_code_parses() {
        let k = "ab".repeat(32);
        assert_eq!(split_code(&k).unwrap().1, None);
        let (key, dir) = split_code(&format!(" {k}@10.0.0.1:7777 ")).unwrap();
        assert_eq!((key, dir.as_deref()), ([0xab; 32], Some("10.0.0.1:7777")));
        assert!(split_code("nome").is_none());
        let c = parse_contacts(&format!("ana {k}@h:1"));
        assert_eq!(c, vec![("ana".to_string(), [0xab; 32])]);
    }

    #[test]
    fn rooms_parse_with_spaces_in_names() {
        let k = hex(&[7u8; 32]);
        let rooms = parse_rooms(&format!("noite de jogo {k}\nlixo\nsala {}", &k[..60]));
        assert_eq!(rooms, vec![SavedRoom { name: "noite de jogo".into(), secret: [7u8; 32], owner: None }]);
    }

    #[test]
    fn remove_contact_drops_only_that_entry() {
        let (a, b) = (hex(&[1u8; 32]), hex(&[2u8; 32]));
        let text = format!("ana {a}\nlixo\nbia {b}");
        assert_eq!(remove_contact(&text, 0), format!("lixo\nbia {b}"));
        assert_eq!(remove_contact(&text, 1), format!("ana {a}\nlixo"));
    }

    #[test]
    fn nicknames_set_replace_and_clear() {
        let mut f = Form::default();
        let k = [9u8; 32];
        f.set_nick(&k, " Zé da Silva ");
        f.set_nick(&[8u8; 32], "bia");
        assert_eq!(f.nick(&k), "Zé da Silva");
        f.set_nick(&k, "Zé");
        assert_eq!((f.nick(&k), f.nick(&[8u8; 32])), ("Zé".to_string(), "bia".to_string()));
        f.set_nick(&k, "");
        assert_eq!(f.nick(&k), "");
        assert_eq!(f.names(), vec![("bia".to_string(), [8u8; 32])]);
    }

    #[test]
    fn room_codes_carry_the_owner_and_rotate() {
        let (s, o) = (hex(&[1u8; 32]), hex(&[2u8; 32]));
        assert_eq!(split_room_code(&format!("{s}:{o}@10.0.0.1:7777")), Some(([1; 32], Some([2; 32]))));
        assert_eq!(split_room_code(&s), Some(([1; 32], None)));
        assert!(split_room_code(&format!("{s}:zz")).is_none());
        let mut f = Form { rooms: format!("a {s}:{o}\nb {}", hex(&[3u8; 32])), ..Form::default() };
        f.replace_room_secret(&[1; 32], &[9; 32]);
        let r = parse_rooms(&f.rooms);
        assert_eq!((r[0].secret, r[0].owner, r[1].secret), ([9; 32], Some([2; 32]), [3; 32]));
    }
}
