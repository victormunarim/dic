//! Configuração salva, formulário e funções puras (códigos, contatos, chaveiro).

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


/// Ajustes de áudio que a pessoa escolhe na janela e que ficam salvos.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AudioPrefs {
    /// Volume do alto-falante e ganho do microfone (1.0 = sem alteração).
    pub(crate) volume: f32,
    pub(crate) mic_gain: f32,
    /// Corta os graves abaixo de ~100 Hz (ventilador, estrondo de mesa, sopro).
    pub(crate) high_pass: bool,
    /// Porta de ruído: quando o microfone fica abaixo de `gate_db` (dBFS) o som é reduzido em `gate_depth_db`.
    pub(crate) gate: bool,
    pub(crate) gate_db: f32,
    pub(crate) gate_depth_db: f32,
    /// Volume automático: nivela a voz, reforçando quem fala baixo e contendo quem fala alto.
    pub(crate) agc: bool,
    /// Remoção de ruído por rede neural (RNNoise): tira teclado, ventilador, vozes ao fundo; também diz à porta o que é voz.
    pub(crate) denoise: bool,
    /// Cancelamento de eco do WebRTC: tira do microfone o que saiu pelo alto-falante.
    pub(crate) echo_cancel: bool,
    /// Supressão do eco que sobra: com o outro lado falando, abaixa o microfone enquanto ele só está captando
    /// o eco (bem mais baixo que a voz que sai pelo alto-falante).
    pub(crate) echo_suppress: bool,
}

impl AudioPrefs {
    pub(crate) const VOLUME: std::ops::RangeInclusive<f32> = 0.0..=2.0;
    pub(crate) const MIC_GAIN: std::ops::RangeInclusive<f32> = 0.0..=3.0;
    pub(crate) const GATE_DB: std::ops::RangeInclusive<f32> = -60.0..=-20.0;
    pub(crate) const GATE_DEPTH_DB: std::ops::RangeInclusive<f32> = 6.0..=60.0;

    /// Lê os ajustes de `kv`; o que faltar ou vier estragado fica no padrão e os números são limitados à faixa válida.
    fn from_kv(kv: &HashMap<String, String>) -> Self {
        let d = Self::default();
        let num = |k: &str, range: &std::ops::RangeInclusive<f32>, default: f32| {
            kv.get(k).and_then(|v| v.trim().parse::<f32>().ok()).filter(|v| v.is_finite()).map_or(default, |v| v.clamp(*range.start(), *range.end()))
        };
        let flag = |k: &str, default: bool| kv.get(k).map_or(default, |v| v.trim() == "1");
        Self {
            volume: num("volume", &Self::VOLUME, d.volume),
            mic_gain: num("mic_gain", &Self::MIC_GAIN, d.mic_gain),
            high_pass: flag("high_pass", d.high_pass),
            gate: flag("gate", d.gate),
            gate_db: num("gate_db", &Self::GATE_DB, d.gate_db),
            gate_depth_db: num("gate_depth_db", &Self::GATE_DEPTH_DB, d.gate_depth_db),
            agc: flag("agc", d.agc),
            denoise: flag("denoise", d.denoise),
            echo_cancel: flag("echo_cancel", d.echo_cancel),
            echo_suppress: flag("echo_suppress", d.echo_suppress),
        }
    }

    fn to_conf(self) -> String {
        format!(
            "volume={:.2}\nmic_gain={:.2}\nhigh_pass={}\ngate={}\ngate_db={:.0}\ngate_depth_db={:.0}\nagc={}\ndenoise={}\necho_cancel={}\necho_suppress={}\n",
            self.volume,
            self.mic_gain,
            u8::from(self.high_pass),
            u8::from(self.gate),
            self.gate_db,
            self.gate_depth_db,
            u8::from(self.agc),
            u8::from(self.denoise),
            u8::from(self.echo_cancel),
            u8::from(self.echo_suppress)
        )
    }
}

impl Default for AudioPrefs {
    fn default() -> Self {
        AudioPrefs { volume: 1.0, mic_gain: 1.0, high_pass: true, gate: false, gate_db: -45.0, gate_depth_db: 40.0, agc: true, denoise: true, echo_cancel: true, echo_suppress: true }
    }
}

#[derive(Default, Clone)]
pub(crate) struct Form {
    pub(crate) secret: String,
    /// Uma linha por contato: `[nome] chave-pública-hex`.
    pub(crate) contacts: String,
    pub(crate) peer: String,
    pub(crate) public_addr: String,
    /// Guarda o endereço na DHT do BitTorrent e a consulta ao ligar (veja `NodeConfig::dht`).
    pub(crate) use_dht: bool,
    /// Fala com a DHT por circuito de dois relays de contatos (veja `DhtOptions::circuit`).
    pub(crate) dht_circuit: bool,
    /// Como esta pessoa se apresenta a quem ela adiciona (veja `NodeConfig::name`).
    pub(crate) my_name: String,
    /// Aceita quem adiciona esta pessoa e vira contato sozinho (veja `NodeConfig::introductions`).
    pub(crate) intros: bool,
    /// Serve de âncora para os registros de endereço dos outros (veja `NodeConfig::anchor`).
    pub(crate) anchor: bool,
    /// Âncoras que usa, uma por linha: `ip:porta#chave-do-circuito#chave-do-link`.
    pub(crate) anchors: String,
    pub(crate) use_audio: bool,
    pub(crate) audio: AudioPrefs,
    /// Microfone e alto-falante escolhidos (vazio = padrão do sistema).
    pub(crate) input_dev: String,
    pub(crate) output_dev: String,
    pub(crate) new_name: String,
    pub(crate) new_code: String,
    /// O arquivo de configuração existia e foi lido ao abrir (para a janela mostrar de onde veio).
    pub(crate) loaded: bool,
}

/// Código de contato: a chave pública (64 hex), com o relay de quem o gerou depois de `#` (veja
/// [`split_direct`]). Devolve só a chave.
pub(crate) fn split_code(t: &str) -> Option<[u8; 32]> {
    split_direct(t).map(|(k, _)| k)
}

/// Código de contato direto: `<chave>#ip:porta#chave-do-circuito#chave-do-link` (as duas chaves em 64 hex;
/// o endereço pode ser um nome). Com ele a primeira ligação já sabe onde achar o contato. Um `#` com o resto
/// torto invalida o código inteiro, para um código cortado na colagem não ser aceito pela metade.
pub(crate) fn split_direct(t: &str) -> Option<([u8; 32], Option<clam::node::RelayInfo>)> {
    let t = t.trim();
    let (head, relay) = match t.split_once('#') {
        None => (t, None),
        Some((h, r)) => {
            let mut p = r.split('#');
            let (addr, onion, link) = (p.next()?, p.next()?, p.next()?);
            if p.next().is_some() || addr.is_empty() || addr.len() > 60 || !addr.contains(':') || addr.contains(char::is_whitespace) {
                return None;
            }
            (h, Some(clam::node::RelayInfo { addr: addr.to_string(), onion: unhex(onion).ok()?, link: unhex(link).ok()? }))
        }
    };
    // Código antigo, `<chave>@servidor`: o servidor não existe mais, fica só a chave.
    let key = head.split_once('@').map_or(head, |(k, _)| k);
    unhex(key).ok().map(|k| (k, relay))
}

/// Relays que os códigos diretos da lista de contatos trazem, por chave.
pub(crate) fn contact_relays(text: &str) -> Vec<([u8; 32], clam::node::RelayInfo)> {
    text.split_whitespace().filter_map(split_direct).filter_map(|(k, r)| Some((k, r?))).collect()
}

/// Contatos da lista: `(nome, chave)`. Linhas sem uma chave de 64 hex são ignoradas.
pub(crate) fn parse_contacts(text: &str) -> Vec<(String, [u8; 32])> {
    text.lines()
        .filter_map(|l| {
            let mut name = vec![];
            let mut key = None;
            for t in l.split_whitespace() {
                match split_code(t) {
                    Some(k) => key = Some(k),
                    None => name.push(t),
                }
            }
            key.map(|k| (if name.is_empty() { hex(&k)[..8].to_string() } else { name.join(" ") }, k))
        })
        .collect()
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

/// Endereços de relay de contatos, num arquivo à parte (`chave addr onion link` por linha). Linhas que
/// terminam em `dica` vieram do convite que o contato mandou, sem confirmação de uma chamada; as que
/// terminam em `antigo` são endereços que o contato já teve (do mais novo ao mais velho, por contato).
pub(crate) fn known_path() -> PathBuf {
    conf_path().with_file_name(".clam-gui.known")
}

pub(crate) type Stored = Vec<([u8; 32], clam::node::RelayInfo)>;

/// O que o arquivo de endereços guarda.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Book {
    /// Confirmados por uma chamada, pela DHT ou pelo aviso do próprio contato.
    pub known: Stored,
    pub hints: Stored,
    pub history: Stored,
}

impl Book {
    /// Mesma coisa em ordem estável por contato (a ordem dentro do histórico de um contato se mantém).
    pub fn sorted(&self) -> Self {
        let sort = |list: &Stored| {
            let mut v = list.clone();
            v.sort_by_key(|(k, _)| *k);
            v
        };
        Self { known: sort(&self.known), hints: sort(&self.hints), history: sort(&self.history) }
    }
}

/// Lê o arquivo.
pub(crate) fn load_known() -> Book {
    parse_known(&std::fs::read_to_string(known_path()).unwrap_or_default())
}

fn parse_known(text: &str) -> Book {
    let mut book = Book::default();
    for l in text.lines() {
        let mut p = l.split_whitespace();
        let entry = (|| {
            let (k, addr, o, link) = (p.next()?, p.next()?, p.next()?, p.next()?);
            Some((unhex(k).ok()?, clam::node::RelayInfo { addr: addr.to_string(), onion: unhex(o).ok()?, link: unhex(link).ok()? }))
        })();
        let tail = p.next();
        match entry {
            Some(e) if tail == Some("dica") => book.hints.push(e),
            Some(e) if tail == Some("antigo") => book.history.push(e),
            Some(e) => book.known.push(e),
            None => {}
        }
    }
    book
}

fn format_known(book: &Book) -> String {
    let line = |(k, r): &([u8; 32], clam::node::RelayInfo), tail: &str| format!("{} {} {} {}{tail}\n", hex(k), r.addr, hex(&r.onion), hex(&r.link));
    let all = book.known.iter().map(|e| line(e, "")).chain(book.hints.iter().map(|e| line(e, " dica"))).chain(book.history.iter().map(|e| line(e, " antigo")));
    all.collect()
}

/// Contato removido com o nó parado: tira os endereços dele de `.clam-gui.known`.
pub(crate) fn forget_known(key: &[u8; 32]) -> std::io::Result<()> {
    let mut book = load_known();
    for list in [&mut book.known, &mut book.hints, &mut book.history] {
        list.retain(|(k, _)| k != key);
    }
    save_known(&book)
}

/// Contato adicionado com o nó parado: o relay do código direto dele entra em `.clam-gui.known` como dica.
pub(crate) fn remember_hint(key: [u8; 32], relay: clam::node::RelayInfo) -> std::io::Result<()> {
    let mut book = load_known();
    book.hints.retain(|(k, _)| *k != key);
    book.hints.push((key, relay));
    save_known(&book)
}

/// Contatos a quem a apresentação ainda não foi confirmada, num arquivo à parte (uma chave em hex por linha).
/// Ao ficar online o nó volta a entregá-las; o que for entregue (ou removido) sai do arquivo.
pub(crate) fn pending_path() -> PathBuf {
    conf_path().with_file_name(".clam-gui.intro")
}

pub(crate) fn load_pending() -> Vec<[u8; 32]> {
    std::fs::read_to_string(pending_path()).unwrap_or_default().lines().filter_map(|l| unhex(l.trim()).ok()).collect()
}

pub(crate) fn save_pending(keys: &[[u8; 32]]) -> std::io::Result<()> {
    let mut keys = keys.to_vec();
    keys.sort();
    keys.dedup();
    write_private(&pending_path(), &keys.iter().map(|k| format!("{}\n", hex(k))).collect::<String>())
}

/// Contato adicionado ou removido com o nó parado: a apresentação entra na fila do próximo início, ou sai dela.
pub(crate) fn mark_pending(key: [u8; 32], pending: bool) -> std::io::Result<()> {
    let mut keys = load_pending();
    keys.retain(|k| *k != key);
    if pending {
        keys.push(key);
    }
    save_pending(&keys)
}

/// Âncoras do campo de texto: `ip:porta#chave-do-circuito#chave-do-link` por linha (linhas tortas são ignoradas).
pub(crate) fn parse_anchors(text: &str) -> Vec<clam::node::RelayInfo> {
    text.split_whitespace()
        .filter_map(|t| {
            let mut p = t.split('#');
            let (addr, onion, link) = (p.next()?, p.next()?, p.next()?);
            if p.next().is_some() || addr.is_empty() || addr.len() > 60 || !addr.contains(':') {
                return None;
            }
            Some(clam::node::RelayInfo { addr: addr.to_string(), onion: unhex(onion).ok()?, link: unhex(link).ok()? })
        })
        .collect()
}

/// O texto que outra pessoa cola em "Âncoras" para usar o relay `r` como âncora.
pub(crate) fn anchor_code(r: &clam::node::RelayInfo) -> String {
    format!("{}#{}#{}", r.addr, hex(&r.onion), hex(&r.link))
}

/// A linha de contato de quem se apresentou: o nome que ele deu (limpo) e o código que a apresentação traz.
pub(crate) fn intro_line(name: &str, key: &[u8; 32], reach: &clam::media::update::Reach) -> String {
    use clam::media::update::Reach;
    let name: String = name.chars().filter(|c| !c.is_control() && !matches!(c, ';' | '#' | '@')).collect();
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = if name.is_empty() { hex(key)[..8].to_string() } else { name };
    let Reach { addr, onion, link } = reach;
    format!("{name} {}#{addr}#{}#{}", hex(key), hex(onion), hex(link))
}

/// Chaves do relay deste computador, num arquivo à parte (`onion link`, em hex). Mantê-las entre execuções
/// faz o endereço que os contatos guardaram de você continuar valendo depois de reabrir ou recompilar.
pub(crate) fn relay_keys_path() -> PathBuf {
    conf_path().with_file_name(".clam-gui.relay")
}

/// Lê as chaves do relay; sem arquivo (ou ilegível) gera um par novo e grava. O segundo valor é o erro de
/// gravação, se houve: sem o arquivo as chaves mudam a cada execução e os endereços que os contatos
/// guardaram de você deixam de valer, então a janela precisa avisar.
pub(crate) fn load_relay_keys() -> (clam::node::RelayKeys, Option<std::io::Error>) {
    let read = || {
        let text = std::fs::read_to_string(relay_keys_path()).ok()?;
        let mut p = text.split_whitespace();
        let (onion, link) = (unhex(p.next()?).ok()?, unhex(p.next()?).ok()?);
        Some(clam::node::RelayKeys { onion: StaticSecret::from(onion), link: StaticSecret::from(link) })
    };
    if let Some(keys) = read() {
        return (keys, None);
    }
    let keys = clam::node::RelayKeys::random();
    let saved = write_private(&relay_keys_path(), &format!("{} {}\n", hex(&keys.onion.to_bytes()), hex(&keys.link.to_bytes())));
    (keys, saved.err())
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

pub(crate) fn save_known(book: &Book) -> std::io::Result<()> {
    write_private(&known_path(), &format_known(book))
}

impl Form {
    /// Primeira abertura: já nasce com uma identidade, para a pessoa não precisar gerar nada.
    pub(crate) fn first_run() -> Self {
        Form { secret: hex(&StaticSecret::random_from_rng(OsRng).to_bytes()), intros: true, use_dht: true, anchor: true, ..Form::default() }
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
            contacts: get("contacts").replace(';', "\n"),
            peer: get("peer"),
            public_addr: get("public_addr"),
            // Ligadas de início: o arquivo antigo, sem a chave, também aceita.
            // As chaves guardam o "não": o `dht=0` e o `anchor=0` do tempo em que desligadas era o padrão são ignorados.
            use_dht: get("no_dht") != "1",
            dht_circuit: get("dht_circuit") == "1",
            my_name: get("name"),
            // Ligado de início: o arquivo antigo, sem a chave, também aceita.
            intros: get("intros") != "0",
            anchor: get("no_anchor") != "1",
            anchors: get("anchors").replace(';', "\n"),
            use_audio: get("audio") == "1",
            input_dev: get("input_dev"),
            output_dev: get("output_dev"),
            audio: AudioPrefs::from_kv(&kv),
            loaded: true,
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
            "audio={}\nsecret={}\ncontacts={}\npeer={}\npublic_addr={}\nno_dht={}\ndht_circuit={}\nname={}\nintros={}\nno_anchor={}\nanchors={}\ninput_dev={}\noutput_dev={}\n{}",
            u8::from(self.use_audio),
            secret,
            self.contacts.trim().replace('\n', ";"),
            self.peer.trim(),
            self.public_addr.trim(),
            u8::from(!self.use_dht),
            u8::from(self.dht_circuit),
            self.my_name.trim().replace(['\n', ';'], " "),
            u8::from(self.intros),
            u8::from(!self.anchor),
            self.anchors.trim().replace('\n', ";"),
            self.input_dev.trim(),
            self.output_dev.trim(),
            self.audio.to_conf()
        );
        // Contém a chave secreta: só o dono lê.
        write_private(&conf_path(), &text)
    }

    /// Nomes dos contatos, para mostrar quem liga ou transmite a tela.
    pub(crate) fn names(&self) -> Vec<(String, [u8; 32])> {
        parse_contacts(&self.contacts)
    }

    #[cfg(feature = "audio")]
    pub(crate) fn devices(&self) -> clam::audio::Devices {
        clam::audio::Devices { input: self.input_dev.trim().to_string(), output: self.output_dev.trim().to_string() }
    }

    pub(crate) fn identity(&self) -> Result<StaticSecret, String> {
        unhex(&self.secret).map(StaticSecret::from).map_err(|e| format!("identidade: {e}"))
    }

    /// Meu código de contato: a chave pública e, com o nó online, o relay `relay` desta máquina (veja
    /// [`split_direct`]); `None` no relay enquanto ele não existe (offline).
    pub(crate) fn my_code(&self, relay: Option<&clam::node::RelayInfo>) -> Option<String> {
        let key = hex(PublicKey::from(&self.identity().ok()?).as_bytes());
        Some(match relay {
            Some(r) => format!("{key}#{}#{}#{}", r.addr, hex(&r.onion), hex(&r.link)),
            None => key,
        })
    }

    /// Relay que um código direto (nos contatos ou no campo "ligar para") informa para `key`.
    pub(crate) fn relay_for(&self, key: &[u8; 32]) -> Option<clam::node::RelayInfo> {
        let from_peer = self.peer.split_whitespace().filter_map(split_direct);
        let from_contacts = self.contacts.split_whitespace().filter_map(split_direct);
        from_peer.chain(from_contacts).find(|(k, _)| k == key).and_then(|(_, r)| r)
    }

    pub(crate) fn target(&self) -> Result<[u8; 32], String> {
        let p = self.peer.trim();
        if let Some(k) = split_code(p) {
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
    fn known_addresses_hints_and_history_round_trip_and_stay_apart() {
        let r = |addr: &str, n: u8| clam::node::RelayInfo { addr: addr.into(), onion: [n; 32], link: [n + 1; 32] };
        let book = Book {
            known: vec![([1; 32], r("10.0.0.1:4001", 5))],
            hints: vec![([2; 32], r("[::1]:4001", 8))],
            history: vec![([1; 32], r("10.0.0.9:4001", 11)), ([1; 32], r("10.0.0.8:4001", 12))],
        };
        assert_eq!(parse_known(&format_known(&book)), book);
        // Arquivo de versão anterior (sem as marcas) e linha estragada.
        let old = format_known(&Book { known: book.known.clone(), ..Book::default() }) + "lixo\n";
        assert_eq!(parse_known(&old), Book { known: book.known, ..Book::default() });
    }

    #[test]
    fn audio_prefs_round_trip_and_survive_bad_values() {
        let prefs = AudioPrefs { volume: 1.5, mic_gain: 2.25, high_pass: false, gate: true, gate_db: -38.0, gate_depth_db: 24.0, agc: true, denoise: false, echo_cancel: false, echo_suppress: false };
        let kv = |text: &str| -> HashMap<String, String> { text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        assert_eq!(AudioPrefs::from_kv(&kv(&prefs.to_conf())), prefs);
        // Arquivo de versão anterior, sem nenhum ajuste: tudo no padrão.
        assert_eq!(AudioPrefs::from_kv(&kv("audio=1\n")), AudioPrefs::default());
        // Fora da faixa, ilegível ou não finito: limita ou volta ao padrão.
        let bad = AudioPrefs::from_kv(&kv("volume=99\nmic_gain=abc\ngate_db=NaN\ngate_depth_db=-5\n"));
        assert_eq!((bad.volume, bad.mic_gain, bad.gate_db, bad.gate_depth_db), (2.0, 1.0, -45.0, 6.0));
    }

    #[test]
    fn contact_code_parses() {
        let k = "ab".repeat(32);
        assert_eq!(split_code(&k), Some([0xab; 32]));
        assert_eq!(split_code(&format!(" {k} ")), Some([0xab; 32]));
        assert!(split_code("nome").is_none());
        // O código direto passa por split_code sem o relay.
        let direct = format!("{k}#203.0.113.7:4001#{}#{}", "11".repeat(32), "22".repeat(32));
        assert_eq!(split_code(&direct), Some([0xab; 32]));
        // Código antigo, com o servidor que não existe mais: fica a chave.
        assert_eq!(parse_contacts(&format!("ana {k}@h:1")), vec![("ana".to_string(), [0xab; 32])]);
    }

    #[test]
    fn direct_code_round_trips_and_rejects_bad_tails() {
        let k = "ab".repeat(32);
        let (onion, link) = ("11".repeat(32), "22".repeat(32));
        let (key, relay) = split_direct(&format!(" {k}#203.0.113.7:4001#{onion}#{link} ")).unwrap();
        assert_eq!(key, [0xab; 32]);
        assert_eq!(relay, Some(clam::node::RelayInfo { addr: "203.0.113.7:4001".into(), onion: [0x11; 32], link: [0x22; 32] }));
        let (_, relay) = split_direct(&format!("{k}#[::1]:4001#{onion}#{link}")).unwrap();
        assert_eq!(relay.unwrap().addr, "[::1]:4001");
        let (_, relay) = split_direct(&format!("{k}#casa.exemplo.com:4001#{onion}#{link}")).unwrap();
        assert_eq!(relay.unwrap().addr, "casa.exemplo.com:4001");
        assert_eq!(split_direct(&k), Some(([0xab; 32], None)));
        // Cortado, sobrando pedaço, endereço sem porta ou chave torta: nada de aceitar pela metade.
        for bad in [format!("{k}#203.0.113.7:4001#{onion}"), format!("{k}#203.0.113.7:4001#{onion}#{link}#x"), format!("{k}#203.0.113.7#{onion}#{link}"), format!("{k}#1.2.3.4:5#{onion}#zz"), format!("{k}#")] {
            assert!(split_direct(&bad).is_none(), "{bad}");
        }
        let f = Form { contacts: format!("ana {k}#203.0.113.7:4001#{onion}#{link}\nbia {}", "cd".repeat(32)), ..Default::default() };
        assert_eq!(contact_relays(&f.contacts).len(), 1);
        assert!(f.relay_for(&[0xab; 32]).is_some() && f.relay_for(&[0xcd; 32]).is_none());
        assert_eq!(parse_contacts(&f.contacts).len(), 2);
    }

    #[test]
    fn my_code_carries_the_relay_once_online() {
        let f = Form { secret: "07".repeat(32), ..Default::default() };
        let key = f.my_code(None).unwrap();
        let relay = clam::node::RelayInfo { addr: "10.0.0.2:4001".into(), onion: [1; 32], link: [2; 32] };
        let direct = f.my_code(Some(&relay)).unwrap();
        assert_eq!(split_direct(&direct).unwrap().1, Some(relay));
        assert!(direct.starts_with(&key));
    }

    #[test]
    fn remove_contact_drops_only_that_entry() {
        let (a, b) = (hex(&[1u8; 32]), hex(&[2u8; 32]));
        let text = format!("ana {a}\nlixo\nbia {b}");
        assert_eq!(remove_contact(&text, 0), format!("lixo\nbia {b}"));
        assert_eq!(remove_contact(&text, 1), format!("ana {a}\nlixo"));
    }

    #[test]
    fn an_introduction_becomes_a_contact_line_that_parses_back() {
        use clam::media::update::Reach;
        let key = [9u8; 32];
        let relay = Reach { addr: "203.0.113.7:4001".into(), onion: [1; 32], link: [2; 32] };
        let line = intro_line("Ana  Maria; #x", &key, &relay);
        assert_eq!(parse_contacts(&line), vec![("Ana Maria x".to_string(), key)]);
        let (k, r) = split_direct(line.split_whitespace().last().unwrap()).unwrap();
        assert_eq!((k, r.unwrap().addr), (key, "203.0.113.7:4001".to_string()));
        let line = intro_line("", &key, &relay);
        assert_eq!(parse_contacts(&line), vec![(hex(&key)[..8].to_string(), key)]);
    }

    #[test]
    fn anchors_round_trip_and_skip_bad_lines() {
        let r = clam::node::RelayInfo { addr: "198.51.100.2:4001".into(), onion: [3; 32], link: [4; 32] };
        let text = format!("{}\nlixo\n198.51.100.3:4001#curto#curto\n", anchor_code(&r));
        assert_eq!(parse_anchors(&text), vec![r]);
    }
}
