# Circuito de 2 saltos (Fase 3)

Relays, gerente de circuitos, transporte TCP, descritores, descoberta, reputação, cover traffic, medições e fuzzing.

[← Voltar ao README](../README.md)

Para ver como as peças se encaixam numa chamada (link, circuito, telefone, pacotes, tela, reencontro de contatos), leia antes [COMUNICACAO.md](COMUNICACAO.md). Este documento detalha cada peça do transporte.

**O que o aplicativo usa.** O `Node` (e a GUI) usa o circuito de 2 saltos, o link Noise, os dois covers, o gerente com reserva e failover e a política de saída. A reputação medida (`reputation`) é opcional e o `Node` não a liga, porque só conhece os relays dos contatos. Os relays são os dos próprios contatos, então o aplicativo **não** usa descritores assinados nem a descoberta por GossipSub (`descriptor`, `discovery`, `net`): eles são a camada de biblioteca para quem quiser uma rede aberta de relays e estão aqui por completude.

## Fase 3: circuito de 2 saltos (`clam::circuit`)

Núcleo do modo privado: você → relay A → relay B → destino. É *sans-IO*: as máquinas de estado recebem células e devolvem ações, e o transporte fica por sua conta. O transporte real (TCP + Noise NK, `clam::transport`) e o failover automático (`clam::circuit::manager`) já existem e estão descritos nas seções seguintes.

Garantias, cobertas por `tests/circuit.rs`:

- A só conhece o seu IP e o de B; B só conhece o de A e o destino. O destino só aparece dentro da camada de B.
- Handshake X25519 por salto, autenticado pela chave estática do relay (estilo ntor), com sigilo adiante. Um relay sem a chave certa é detectado pelo cliente.
- Células de tamanho fixo (`CELL_LEN = 256`), com preenchimento aleatório a cada salto: o tamanho no fio não revela a posição do salto.
- Contador explícito por camada e janela anti-replay de 64 células: repetição e adulteração são descartadas.
- Limites do relay: circuitos simultâneos, taxa de criação de circuitos, banda por circuito e total, expiração por ociosidade e derrubada após falhas de autenticação.
- Custo: cada célula carrega até **203 bytes** de dados (cabe um quadro Opus de 20 ms); voz bidirecional gasta cerca de 100 kbps por sentido, mais que os 40 a 60 kbps do Opus puro, por causa do tamanho fixo. Ajuste `CELL_LEN` em `src/circuit/mod.rs` para trocar banda por tamanho máximo de pacote.

### Cliente

```rust
use clam::circuit::client::{ClientCircuit, Event};

// Cada método devolve a célula a enviar ao PRIMEIRO relay (A).
let (mut c, create) = ClientCircuit::create(1, &chave_publica_de_a);
enviar_a_a(create);

// Para cada célula recebida de A:
match c.handle_cell(celula_recebida)? {
    Event::HopReady(0) => enviar_a_a(c.extend("relay-b.example:4001", &chave_publica_de_b)?),
    Event::HopReady(1) => enviar_a_a(c.begin("contato.example:9")?),
    Event::Data(bytes) => { /* áudio vindo do contato */ }
    Event::Pong { hop, nonce } => { /* latência até o salto: use um relógio seu */ }
    Event::Destroyed => { /* um relay caiu: monte outro circuito */ }
    _ => {}
}

enviar_a_a(c.send_data(&quadro_opus)?);   // até 203 bytes
enviar_a_a(c.ping(1, nonce)?);            // sinal de vida e medição de RTT
```

### Relay

```rust
use clam::circuit::relay::{Action, RelayConfig, RelayNode};

let mut relay = RelayNode::new(segredo_x25519, RelayConfig::default(), agora_ms);
// publique relay.public_key() no descritor do relay

let acoes = relay.handle_cell(link, celula, agora_ms);
for a in acoes {
    match a {
        Action::Send { link, cell } => { /* escreva a célula no link */ }
        Action::Connect { link, addr } => {
            /* conecte a addr; depois: relay.link_established(link), ou link_failed(link) */
        }
        Action::OpenStream { circ, dest } => { /* só no último salto: abra o fluxo até dest */ }
        Action::Deliver { circ, payload } => { /* entregue ao destino */ }
        Action::CloseStream { circ } => {}
    }
}
// Dados que voltam do destino: relay.stream_data(circ, &bytes)?
// A cada poucos segundos: relay.tick(agora_ms) derruba circuitos ociosos.
```

Contrato com o transporte: ids de link de entrada devem ser menores que `FIRST_OUTBOUND_LINK` (2^63); cada `Action::Connect` pede uma conexão **nova**. O `tests/circuit.rs` traz um simulador completo que você pode copiar.

### Gerente de circuitos (`clam::circuit::manager`)

O `CircuitManager` mantém um circuito **ativo** e outro **reserva**, já montado e verificado, por relays diferentes. Se o ativo falha, o reserva assume na mesma chamada de `tick`, sem novo handshake. Depois o gerente monta um novo reserva. É *sans-IO*: você alimenta com tempo (ms) e células, e ele devolve `Vec<Out>`.

```rust
use clam::circuit::manager::{Candidate, CircuitManager, ManagerConfig, Out, Event};

// `relays`: descritores dos relays (endereço, chave pública, pontuação).
let mut mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", relays);
let mut outs = mgr.start(agora_ms);

// Laço do app:
//   a cada ~10-20 ms:        outs = mgr.tick(agora_ms);
//   célula vinda de relay:   outs = mgr.handle_cell(cell, agora_ms);
//   quadro de áudio:         outs = mgr.send(&quadro)?;
for out in outs {
    match out {
        Out::Send { to, cell } => { /* envie `cell` à conexão com o relay `to` */ }
        Out::Event(Event::Data(bytes)) => { /* mídia do contato */ }
        Out::Event(Event::FailedOver { silence_ms }) => { /* só informativo */ }
        Out::Event(Event::CircuitFailed { first, second, reason }) => { /* alimente a reputação */ }
        Out::Event(_) => {}
    }
}
```

Como funciona:

- **Detecção**: o ativo recebe ping fim a fim a cada `ping_interval_ms` (100). Sem nenhum sinal por `dead_after_ms` (350), é dado como morto. Com o ping de 100 ms e o prazo de 350 ms, o silêncio do failover fica perto de 350 ms, abaixo do alvo de 500 ms.
- **Reserva**: pinga a cada 1 s para continuar vivo nos relays e abre o fluxo até o destino. Por isso o contato pode ver duas sessões; o app deve tolerar duplicatas (número de sequência no pacote de mídia).
- **Escolha de relays**: sorteia entre os 3 de maior pontuação. O reserva evita os relays do ativo; se não houver relays suficientes, cai para evitar só um deles.
- **Punição**: relay de um circuito que falhou fica `cooldown_ms` (30 s) fora das escolhas. Falha na montagem pune só o salto travado; falha em circuito pronto pune os dois, já que não dá para saber qual caiu.
- **Sem circuito**: se não há reserva, emite `Event::NoCircuit`, `send` devolve `SendError::Unavailable` e a montagem é refeita a cada `retry_ms`.
- **Ajuste de latência**: `dead_after_ms` precisa superar a ida e volta do circuito mais o intervalo de ping. Se a latência real passar de ~100 ms por travessia, aumente-o; o silêncio no failover cresce junto.

Os testes em `tests/manager.rs` derrubam o primeiro ou o segundo relay do ativo e verificam silêncio < 500 ms, reserva disjunto, remontagem sem os relays punidos e timeout de montagem. A rede simulada entrega sem atraso; a medição com latência real continua pendente.

### Transporte TCP (`clam::transport`, feature `net`)

Liga os motores *sans-IO* a sockets TCP reais.

**Rodar um relay:**

```rust
use clam::circuit::relay::{RelayConfig, RelayNode};
use clam::transport::{RelayServer, ServerConfig, allow_list};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};

let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
println!("chave do circuito: {:?}", node.public_key()); // vai no descritor (onion_key)

let mut cfg = ServerConfig::new("0.0.0.0:4001".parse()?);
// Só relays de saída precisam disto: quais destinos o segundo salto pode abrir.
cfg.exit = allow_list(vec![("contato:9".into(), "127.0.0.1:9000".into())]);
// Opcional: cfg.link_secret = Some(segredo_guardado) mantém a chave de link entre reinícios.
let relay = RelayServer::spawn(node, cfg).await?; // relay.stop() para encerrar
println!("chave do link: {:?}", relay.link_public_key()); // vai no descritor (link_key)
```

**Usar como cliente:**

```rust
use clam::circuit::manager::{Candidate, CircuitManager, Event, ManagerConfig};
use clam::transport::CircuitClient;

let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", candidatos);
let mut c = CircuitClient::spawn(mgr, std::time::Duration::from_secs(2));

while let Some(ev) = c.next_event().await {
    match ev {
        Event::Ready { .. } => c.send(b"quadro de audio").await?,
        Event::Data(bytes) => { /* mídia do contato */ }
        Event::CircuitFailed { first, second, reason } => { /* reputação */ }
        _ => {}
    }
}
```

Formato no fio: os links entre nós usam **Noise `NK`** (`clam::link`, crate `snow`) e carregam uma célula por mensagem cifrada; todo quadro no fio tem 279 bytes. O fluxo do relay de saída ao destino usa quadros `[u16 BE tamanho][dados]`, para preservar as fronteiras dos pacotes de mídia.

Cifra de link:

- **Autentica o relay**: quem disca conhece a **chave de link** do relay (`link_key` do descritor, `Candidate::link_public`). Com a chave errada o handshake falha e o link não abre. Relay para relay funciona igual: o cliente inclui a chave de link do próximo relay no pedido de extensão.
- **Oculta** ids de circuito, comandos e conteúdo de quem observa a rede, e impede injetar ou alterar células. O tamanho já é fixo; a temporização é coberta pelo cover traffic (seção abaixo).
- Nonces em ordem estrita por direção, handshake com prazo de 3 s, e `max_links` (512) conta também conexões ainda em handshake. `max_links_per_ip` (32) limita o que uma só origem (IPv4, ou /64 de IPv6) ocupa desse total; o loopback não conta. Quem manda célula em claro, sem handshake, é desconectado.
- **Duas chaves distintas**: a do circuito (`onion_key`, handshake estilo ntor) e a do link (`link_key`, Noise NK), ambas no descritor assinado. Vazar ou girar uma não afeta a outra. Por padrão `RelayServer` gera a chave de link a cada início; para mantê-la, passe `ServerConfig::link_secret` e publique um descritor novo se ela mudar. Usar a chave do circuito como chave de link não abre o link (há teste).

Outros limites:

- **`ExitPolicy`**: o padrão recusa tudo. Sem lista de destinos, um relay de saída seria um proxy aberto. Relays que só fazem o primeiro salto não precisam configurar nada.
- O trecho do relay de saída ao destino é TCP puro. A mídia já vai cifrada ponta a ponta pela camada acima.
- Fechar um link não derruba na hora os circuitos que passavam por ele: o relay os expira por ociosidade e o cliente os detecta por silêncio.

`tests/transport.rs` roda tudo em 127.0.0.1: eco de voz por 2 saltos, política de saída recusando destino fora da lista, recusa de relay com chave errada, relay que descarta célula em claro, e failover derrubando o primeiro relay do circuito ativo com mídia a cada 20 ms (silêncio < 500 ms, repetido várias vezes sem falhar). Em loopback não há latência de rede; a medição entre máquinas distintas tem roteiro em "Medir entre máquinas reais" (resultados ainda não coletados).

### Descritores assinados (`clam::descriptor`)

Cada relay publica um descritor assinado pela sua **identidade** (ed25519): endereço, chave de circuito e chave de link (X25519), banda declarada e validade. O `RelayDirectory` valida os que chegam e entrega ao gerente a lista de `Candidate`.

```rust
use clam::descriptor::{DEFAULT_TTL_SECS, DirectoryConfig, RelayDescriptor, RelayDirectory};
use clam::{ed25519_dalek::SigningKey, rand_core::OsRng};

// No relay (guarde `identity` em disco; ela é a identidade do relay):
let identity = SigningKey::generate(&mut OsRng);
let d = RelayDescriptor::sign(&identity, node.public_key(), relay.link_public_key(), "203.0.113.5:4001", 2_000, agora_s, DEFAULT_TTL_SECS)?;
let bytes = d.to_bytes(); // publique/repasse estes bytes; reassine antes de vencer

// No cliente, para cada descritor recebido (GossipSub, arquivo de sementes...):
let mut dir = RelayDirectory::new(DirectoryConfig::default());
dir.insert_bytes(&bytes, agora_s)?;          // Err se inválido, vencido, endereço proibido...
let candidatos = dir.candidates(agora_s);     // alimenta CircuitManager::new / set_relays
```

Formato v2: `[versão][identidade 32][chave de circuito 32][chave de link 32][banda kbps u32][emitido u64][ttl u32][tam. addr u8][addr][assinatura 64]`, assinatura sobre `"clam-relay-desc-v2" || corpo`.

O diretório aplica:

- **Assinatura e validade**: qualquer byte alterado é recusado; vencido ou emitido no futuro (tolerância de 300 s) também. `ttl` máximo aceito: 24 h.
- **Versão mais nova vence**: um descritor antigo reenviado volta como `Stale` e não ressuscita nada.
- **Um endereço, uma identidade**: quem chegou primeiro (enquanto válido) o mantém; outra identidade com o mesmo endereço recebe `AddressTaken`.
- **Endereços seguros**: `host:porta` com porta ≠ 0; literais de IP loopback, privados, de link local ou multicast, e `localhost`, são recusados, para o descritor não apontar relays a redes internas. Em redes de teste use `allow_private_addrs: true`.
- **Teto de tamanho**: `max_entries` (4096).
- **Pontuação**: cresce com o log da banda declarada, limitada a 10 000 kbps, porque a banda é autodeclarada.

Limites: a identidade é barata de gerar (ataque Sybil), então a assinatura prova *quem assinou*, não que o relay é bom. Reputação medida e relays semeados pela equipe continuam necessários. O descritor carrega as duas chaves do relay, a de circuito e a de link (formato v2, acima). Quem entrega descritores ao diretório é a descoberta por GossipSub (próxima seção), que o aplicativo não usa.

### Descoberta de relays por GossipSub (`clam::discovery`, feature `net`)

> Camada de biblioteca: o `Node` e a GUI não a usam (os relays são os dos contatos). Serve a quem monta uma rede aberta de relays.

Os relays anunciam o descritor no tópico público `clam/relays/v1`; todo nó que assina o tópico valida, guarda no `RelayDirectory` e alimenta o gerente de circuitos.

**Relay** (anuncia e ouve):

```rust
use std::sync::Arc;
use clam::discovery::{Announcer, Discovery, DiscoveryConfig, DiscoveryRunner};
use clam::net::{NetConfig, Node};
use clam::runner::system_clock;
use clam::descriptor::{DEFAULT_TTL_SECS, DirectoryConfig, RelayDescriptor};

let mut node = Node::start(NetConfig { /* listen, bootstrap */ ..Default::default() }).await?;
let discovery = Arc::new(Discovery::new(DirectoryConfig::default(), system_clock()));

// Reassina a cada chamada (issued_at = agora), para a versão nova substituir a antiga.
let announcer: Announcer = Arc::new(move |agora| {
    RelayDescriptor::sign(&identidade, chave_de_circuito, "203.0.113.5:4001", 2_000, agora, DEFAULT_TTL_SECS)
        .ok()
        .map(|d| d.to_bytes())
});
let _runner = DiscoveryRunner::spawn(
    discovery.clone(),
    node.gossip(),
    node.take_descriptors().unwrap(),
    Some(announcer),
    DiscoveryConfig::default(),
);
```

**Cliente** (só ouve): igual, com `None` no lugar do anunciante. Para começar sem esperar a rede, carregue sementes (um descritor em hexadecimal por linha, `#` para comentário; `to_seed_line` gera a linha):

```rust
discovery.load_seeds(&std::fs::read_to_string("relays.seeds")?);

// De tempos em tempos (ex.: a cada minuto) renove a lista do gerente:
mgr.set_relays(discovery.candidates());
```

Como funciona:

- **Validação antes de repassar.** O `Node` usa validação manual do GossipSub. Um descritor só é repassado, e só chega ao `Discovery`, se tiver assinatura válida, validade em dia e endereço permitido, conforme `NetConfig::relay_policy` (em redes de teste, `DirectoryConfig { allow_private_addrs: true, .. }`). Lixo, assinatura quebrada e descritor vencido morrem no primeiro nó que os recebe.
- **Anúncios.** O relay publica logo ao subir, de novo após 2 s, 10 s e 30 s (a malha do GossipSub leva um tempo para se formar) e depois a cada 10 min. A cada 5 min reenvia até 8 descritores que conhece, para quem entrou depois ficar sabendo.
- **Estado.** Fica no `RelayDirectory`: versão mais nova vence, um endereço por identidade, teto de 4096 entradas.
- As mensagens do tópico de relays vêm por `Node::take_descriptors()` e não passam por `next_message()`.

Limites: o tópico é público, então quem o assina vê quais relays existem (intencional; o que se protege é quem usa quais). Um cliente sem nenhum vizinho no GossipSub não descobre nada: mantenha `bootstrap` ou sementes. Sybil continua em aberto (veja "Descritores assinados").

### Reputação medida (`clam::reputation`)

A banda do descritor é declarada pelo próprio relay, então não prova nada. `Reputation` ajusta a pontuação com o que o seu cliente observa nos eventos do gerente:

```rust
let rep = Reputation::new(ReputationConfig::default());
let mut client = CircuitClient::spawn_with_reputation(manager, Duration::from_secs(3), rep);
// ao renovar a lista (descoberta): a reputação é aplicada pelo próprio cliente
client.set_relays(discovery.candidates()).await;
// diagnóstico
let stats = client.relay_stats("203.0.113.5:9000").await;
```

`spawn_with_reputation` liga tudo: o cliente alimenta a `Reputation` com cada `Rtt` e `CircuitFailed`, guarda a lista bruta de `set_relays` e reaplica as pontuações a cada falha e a cada 30 s (`REPUTATION_REFRESH`). Sem reputação (`spawn`), `set_relays` repassa a lista como veio. Quem preferir controle manual usa `Reputation::record` e `adjust` direto.

- **Acertos e erros** com decaimento exponencial (meia-vida de 1 h). Cada `Rtt` vale 0,02 acerto; cada falha, 1 erro. Falha de montagem em `create` culpa só o primeiro salto, em `extend` só o segundo; silêncio ou `Destroy` dividem 0,5 para cada um. Um relay ruim aparece em muitas falhas com parceiros diferentes, e a diferença se acumula.
- **Latência:** média móvel do RTT dos circuitos de que o relay participou. Até 100 ms não pesa; a cada 200 ms a mais, o fator cai pela metade.
- **Pontuação** = base × confiabilidade² × fator de latência. Relay sem histórico começa em 2/3 de confiabilidade: pode ser escolhido, mas perde para um relay que já provou ser estável.
- **Banimento local:** com pelo menos 4 de evidência e taxa de acerto abaixo de 20%, o relay sai da lista até as falhas decaírem.
- `CircuitClient::set_relays` troca a lista sem derrubar circuitos em uso. A lista só passa pela reputação depois do primeiro `set_relays`; antes disso vale a lista com que o gerente foi criado.

**Persistência.** A reputação sobrevive a reinícios por instantâneo em arquivo:

```rust
use clam::reputation::{load_file, save_file};

let path = Path::new("reputacao.bin");
let rep = load_file(ReputationConfig::default(), path, 0)?; // arquivo ausente = vazia
let client = CircuitClient::spawn_with_reputation(manager, timeout, rep);
// ... periodicamente (ex.: a cada minuto) e ao encerrar:
if let Some(bytes) = client.reputation_snapshot().await {
    save_file(path, &bytes)?;
}
```

- Formato `clamrep1`: endereço, acertos, erros e RTT por relay, mais a hora da gravação. Ao carregar, o decaimento cobre o tempo parado (desligado por uma meia-vida, as contagens caem pela metade).
- `save_file` é atômico (temporário + `rename`) e usa permissão `0600`: o arquivo revela quais relays você usou.
- O arquivo é tratado como não confiável: magia, tamanho, número de entradas (`max_entries`), UTF-8 do endereço e valores (finitos, entre 0 e 10 000) são validados; qualquer defeito vira `InvalidData`, nunca pânico. Quem tem acesso de escrita ao arquivo ainda pode forjar boa reputação para relays escolhidos; proteja o diretório como protegeria uma chave.
- Sem gravação automática: o chamador decide quando salvar.

Limites: só vale a experiência própria (nada de reputação trocada entre nós, que seria fácil de manipular), então a partida é fria. Um relay pode se comportar bem sob ping e mal sob carga. Sybil continua em aberto: quem cria muitas identidades começa todas com a prioridade de relay novo, o que só atrasa o ataque.

### Testes de abuso (`tests/fuzz.rs`)

Fuzzing determinístico, sem dependências: um PRNG fixo gera bytes aleatórios e mutações (bit trocado, byte zerado ou saturado, corte, extensão) de entradas válidas. Cobre o parser de células e de células de relay, o descritor, a importação de reputação, o relay, o cliente do circuito e o gerente. Os testes afirmam que nada entra em pânico, que as tabelas ficam limitadas e que descritor adulterado nunca passa na verificação.

```bash
cargo test --test fuzz                                    # rápido (padrão)
CLAM_FUZZ_SEED=7 CLAM_FUZZ_ITERS=400000 cargo test --release --test fuzz   # campanha longa
```

O que o fuzzing encontrou: **inundação de `Create`**. O limite de circuitos protegia a memória, mas quem abre e derruba circuitos em loop nunca enchia a tabela e fazia o relay gastar um Diffie-Hellman por célula. Agora o relay aceita no máximo `RelayConfig::creates_per_sec` criações por segundo (padrão 20, rajada `create_burst` = 40); o excesso recebe `Destroy` sem custo de criptografia e conta em `stats.refused_create_rate`. Também ficou provado que células adulteradas ou lixo no mesmo circuito não dessincronizam as camadas, e que 16 falhas de autenticação derrubam o circuito.

Limites: é fuzzing por amostragem, não guiado por cobertura; não substitui uma campanha longa com `cargo-fuzz` nem revisão por terceiros. O transporte (Noise e TCP) não está nestes testes: o `snow` cuida do parsing do link.

### Cover traffic (padding de link)

Sem cobertura, quem observa um link vê quando você fala e quando fica em silêncio (rajadas de 50 quadros/s contra quase nada). Com cover traffic ligado, cada link cifrado emite **exatamente um quadro por intervalo** (padrão 10 ms): a próxima célula da fila ou, se não houver, uma célula `Padding`. Depois do Noise, mídia, controle e preenchimento têm o mesmo tamanho (279 bytes) e a mesma cadência.

- **Cliente:** `CircuitClient::spawn` liga por padrão (`ClientOptions::cover = Some(DEFAULT_COVER)`). Para desligar ou mudar: `CircuitClient::spawn_with(mgr, timeout, ClientOptions { cover: None, ..Default::default() })`.
- **Relay:** abre padding nos links que ele mesmo disca (relay a relay) e **responde** com padding nos links de entrada cujo par mandou padding. Nunca inicia padding em link de entrada. `ServerConfig::cover` (`None` desliga) e `ServerConfig::max_cover_links` (padrão 64).
- **Descarte:** `Padding` é consumido na camada de link. Não chega ao relay lógico nem a circuitos, não gasta token bucket de circuito e não aparece como evento.
- **Custo:** 100 quadros/s × 279 bytes ≈ **223 kbps por sentido, por link, constantes** enquanto o link existe. Numa chamada de 2 saltos isso é bem mais que os ~100 kbps da voz sem cobertura. Aumente o intervalo (ex.: 20 ms) para gastar menos; abaixo da taxa de células reais (voz + ping ≈ 60/s) a fila de saída cresce e o atraso sobe, então não passe de ~15 ms com voz.
- **Latência:** o quadro real espera, em média, meio intervalo (5 ms no padrão). O teste de failover continua abaixo de 500 ms com cobertura ligada.

Limites, ditos com franqueza:

- É padding **por link**: protege contra quem observa um link (provedor do cliente, rede de um relay). O cover de ponta a ponta (seção seguinte) esconde o ritmo dos relays do circuito. Um adversário global que correlaciona entrada e saída continua fora do que o desenho promete.
- O padding acompanha o link, não a chamada: o início e o fim do link ainda revelam quando uma chamada começou e terminou.
- Um par pode forçar um relay a gastar banda mandando um padding. O teto `max_cover_links` limita o dano (64 × 223 kbps ≈ 14 Mbps por sentido no pior caso); relays com banda pequena devem baixar o teto ou desligar.
- Não há jitter aleatório: a cadência é fixa, o que é bom contra análise de temporização mas é uma assinatura reconhecível de "este é um link do clam".

### Medir entre máquinas reais (`examples/measure.rs`)

Em loopback não há latência de rede. O exemplo `measure` mede o circuito de 2 saltos em máquinas distintas e compara com TCP direto. Precisa de 3 máquinas (ou 3 processos em hosts diferentes) com portas TCP abertas: **C** (destino), **A** e **B** (relays). O cliente roda em qualquer outra, ou em C.

```sh
cargo build --release --features net --example measure   # em cada máquina; sempre release
M=target/release/examples/measure

# C: destino que ecoa os quadros
$M contact 0.0.0.0:9000

# A e B: relays. Argumentos: <escuta> <endereço público> contato:9=<IP-de-C>:9000
$M relay 0.0.0.0:4001 IP-DE-A:4001 contato:9=IP-DE-C:9000    # imprime: addr,onion,link
$M relay 0.0.0.0:4001 IP-DE-B:4001 contato:9=IP-DE-C:9000

# cliente: cole as duas linhas impressas por A e B
$M probe "LINHA-A" "LINHA-B" --seconds 120               # com cover (padrão)
$M probe "LINHA-A" "LINHA-B" --seconds 120 --no-cover    # sem cover
$M direct IP-DE-C:9000 --seconds 120                     # linha de base, sem relays
```

Cada relay imprime uma linha `addr,onion,link` (endereço e as duas chaves públicas em hex) na saída padrão. As chaves mudam a cada início do relay, então rode o `probe` com as linhas atuais. O `probe` manda um quadro de 128 B a cada 20 ms (`--interval-ms`) e relata perda e RTT (min, p50, p95, p99, máx, jitter), o tempo de montagem do circuito e failovers. Um quadro ainda em trânsito no fim conta como perda (≈ 1 em 250).

#### Medir entre duas máquinas (Linux e Windows) com os scripts

Se você só tem duas máquinas, os scripts de `scripts/` automatizam o roteiro acima. A divisão é: o **Windows** (ou qualquer outra) roda o destino e o relay B; esta máquina (**Linux**) roda o relay A e o cliente. Assim o `direct` mede a rede pura entre as duas.

1. Nas duas, instale o Rust e clone o projeto. Libere as portas TCP 4001 e 9000 no firewall da máquina remota.
2. No Windows, em um PowerShell (como administrador, se usar `-OpenFirewall`):

   ```powershell
   .\scripts\measure-windows.ps1 -PublicIp 192.168.0.20 -OpenFirewall
   ```

   `-PublicIp` é o IP do Windows alcançável pelo Linux. O script compila, sobe o destino e o relay B, e imprime o comando exato para rodar no Linux (com a linha `addr,onion,link` já preenchida). Ctrl+C encerra tudo e remove as regras de firewall.
3. No Linux, cole o comando impresso:

   ```sh
   scripts/measure.sh 192.168.0.20 '192.168.0.20:4001,<onion>,<link>' 60
   ```

   O script sobe o relay A, mede o `direct` e o circuito sem cover e com os perfis `economy`, `standard` e `strong` (60 s cada, mude o último argumento), e grava tudo em `measure-results-<data>.txt`.

Como ler: o `direct` é a rede pura Linux–Windows. Cada medição de circuito imprime `caminho: cliente -> X -> Y -> destino`. O gerente **sorteia a ordem dos relays** a cada execução, então o relay local pode ser o primeiro ou o de saída, e os dois caminhos não têm a mesma latência. Compare só medições com o mesmo caminho, ou repita o script até ter várias de cada. Os scripts foram testados só com os dois lados em `127.0.0.1`; o `measure-windows.ps1` não foi executado em Windows.

Como ler:

- **Referência**: o RTT do `direct` é a rede pura entre o cliente e C. O custo do circuito é `probe − direct`.
- **Orçamento de voz**: o gargalo é o trajeto de ida e volta. Para conversa fluida mire em p95 de ida (RTT/2) abaixo de ~150 ms; p99 do RTT muito acima do p50 indica fila ou perda.
- **Cover**: compare `--no-cover` com o padrão. O padding de 10 ms por link soma atraso (cada quadro espera o próximo instante de envio).
- **Failover**: derrube o relay A (Ctrl-C) durante um `probe` longo e veja a linha `failover (silêncio N ms)`. Meta: < 500 ms.
- Para um perfil de rede ruim, rode com `tc qdisc add dev eth0 root netem delay 40ms 10ms loss 1%` na máquina de um relay.

Referência em loopback (release, 1 máquina, 5 s), RTT p50 da voz:

| Configuração | p50 |
|---|---|
| TCP direto | 0,5 ms |
| Circuito sem nenhum cover (`relay ... --no-cover` e `probe --no-cover`) | 1,3 ms |
| Cover só entre os relays (padrão dos relays, `probe --no-cover`) | 18 ms |
| Cover em todos os links (padrão) | 25 ms |

O atraso de ~18 ms **não é bug**: é a quantização do cover. Com cover, cada link só envia no próximo instante do relógio de 10 ms, então um quadro espera em média 5 ms por link e por sentido (até 10). Com 3 links e 2 sentidos isso dá ~25 ms, e o relay liga cover por padrão nos links que ele abre, por isso `probe --no-cover` sozinho ainda mede 18 ms. Para medir só a rede, ligue `--no-cover` também nos relays. Reduzir o atraso custa banda: o intervalo de 5 ms corta a espera pela metade e dobra os ~223 kbps por sentido e por link.

### Cover traffic de ponta a ponta (circuito)

O padding de link esconde a temporização de quem observa um link, mas **os relays do circuito continuam vendo** quando há voz: o relay A enxerga as células do seu circuito e sabe se estão fluindo. O cover de circuito esconde isso: o cliente manda **exatamente uma célula de circuito a cada 20 ms** (padrão), seja mídia, ping ou um `RelayCmd::Drop`, e o relay de saída **responde uma por uma**, com o próximo quadro vindo do contato ou com outro `Drop`. Silêncio e conversa têm a mesma taxa nos dois sentidos, em todos os saltos (há testes que contam 50 células/s nos dois casos). O circuito da **tela** usa o mesmo mecanismo com outro ritmo (uma célula a cada 2 ms, ou nenhum cover quando a pessoa abre mão da privacidade); veja [COMUNICACAO.md](COMUNICACAO.md#transmissão-de-tela).

- **Ligar/desligar:** `ClientOptions::circuit_cover` (padrão `Some(DEFAULT_CIRCUIT_COVER)` = 20 ms; `None` desliga). No gerente sans-IO: `ManagerConfig::cover_ms` ou `CircuitManager::set_cover`. Os relays não precisam de configuração: o cover liga no relay de saída ao receber o primeiro `Drop`.
- **Relógio do cliente:** o cliente dita a cadência dos dois sentidos, então o relay de saída não precisa de timer. Mídia que chega do contato espera no relay (fila de 64, a mais antiga cai) até a próxima célula do cliente.
- **Ping:** só ocupa o lugar de um `Drop`. Falando sem parar, o RTT medido pode ficar desatualizado, mas o circuito segue vivo, porque cada `Drop` ou `Data` recebido conta como sinal de vida e o failover continua abaixo de 500 ms (há teste).
- **Custo:** 50 células de 256 B por sentido, por circuito ativo: ≈ 100 kbps por sentido. O reserva também mantém cover (`ManagerConfig::standby_cover`, padrão `true`): outras 50 células por sentido, então o gasto total do cliente é o dobro. Com `standby_cover: false` o reserva volta a mandar só um ping por segundo. Esse tráfego já está dentro dos 800 kB/s por circuito do relay.
- **Latência:** o quadro de mídia espera até 20 ms pela próxima célula do cliente, mais até 20 ms no sentido de volta pela mesma razão (o exit só responde quando o cliente manda).
- **Limites:** com `standby_cover` ligado o reserva tem o mesmo ritmo do ativo, então o failover não muda o padrão; com ele desligado, o reserva se distingue (um ping por segundo). Quem vê o primeiro relay ainda nota o tráfego de mídia passando de um circuito ao outro. O cover não protege contra um relay de saída malicioso que mede a fila ou contra adversário que vê o contato.

### Perfis de custo do cover (`clam::cover`)

Os dois covers custam banda constante. Um **perfil** agrupa os intervalos em quatro níveis:

| Perfil | Link | Circuito | Banda do cliente (por sentido) | Atraso aproximado por link |
|---|---|---|---|---|
| `Off` | desligado | desligado | 0 | 0 |
| `Economy` | 20 ms | 20 ms | ≈ 112 kbps | até 20 ms (média 10) |
| `Standard` (padrão) | 10 ms | 20 ms | ≈ 223 kbps | média 5 ms |
| `Strong` | 5 ms | 20 ms | ≈ 446 kbps | média 2,5 ms |

O cover de circuito vai dentro dos quadros do link e não soma; o circuito nunca passa de 20 ms (cadência da voz) e o link nunca fica mais lento que o circuito (há teste).

```rust
use clam::cover::CoverProfile;
use clam::transport::{CircuitClient, ClientOptions};

// Escolher na partida (substitui `cover` e `circuit_cover`):
let c = CircuitClient::spawn_with(mgr, timeout, ClientOptions { profile: Some(CoverProfile::Economy), ..Default::default() });

// Adaptar à rede em uso, mesmo com a chamada em curso:
c.set_profile(CoverProfile::for_budget(150.0)).await;   // dados móveis: cabe só o Economy
c.set_profile(CoverProfile::Strong).await;              // Wi-Fi
```

- `CoverProfile::for_budget(kbps)` devolve o perfil mais forte que cabe no orçamento por sentido. Abaixo de 112 kbps devolve `Off`, porque um cover pela metade não esconde nada. A banda da voz (≈ 100 kbps) fica fora desse orçamento.
- `set_profile` vale também nos links já abertos (o escritor de cada link observa o intervalo), sem reconectar nem refazer circuito.
- **Quem decide é o aplicativo.** O clam não adivinha se a rede é paga ou se a bateria está baixa: ele só aplica o perfil. Também não há troca automática por congestionamento, porque cover mais lento não alivia fila nenhuma.
- A troca é visível: quem observa o link vê a cadência mudar. Trocar de perfil no meio da chamada revela a troca, não o conteúdo; prefira trocar na mudança de rede, quando o link já muda.
- **Perfil por relay.** O relay tem o seu próprio perfil, independente do cliente: `ServerConfig { profile: Some(CoverProfile::Economy), .. }` na partida (substitui `ServerConfig::cover`, padrão 10 ms) e `RelayServer::set_profile(..)` para trocar com o relay no ar, inclusive nos links abertos. Vale para o que o relay escreve, nos links que ele abre e nos de entrada cujo par usa padding. Um relay em banda cara usa `Economy`; os outros ficam em `Standard`. O cliente continua ditando o ritmo do circuito (1 célula a cada 20 ms), então o perfil do relay só muda o padding de link e o atraso por salto. O teto `max_cover_links` segue valendo.
- No `measure`: `probe ... --profile economy`.

### Fuzzing guiado por cobertura (`fuzz/`, `cargo-fuzz`)

Além dos testes determinísticos de `tests/fuzz.rs`, o diretório `fuzz/` tem alvos libFuzzer que usam cobertura de código para chegar a caminhos que o gerador aleatório não alcança. É um crate à parte (`[workspace]` próprio), então não afeta `cargo build` nem `cargo test`.

| Alvo | O que exercita | Propriedade |
|---|---|---|
| `cell` | `Cell::from_bytes`, `RelayCell::decode` | sem pânico; célula aceita reproduz a entrada |
| `descriptor` | `RelayDescriptor::from_bytes`, `verify` | sem pânico; descritor aceito reproduz a entrada |
| `reputation` | `Reputation::import` | sem pânico; o que entra sai importável de novo |
| `presence` | `presence::open` | sem pânico com registro arbitrário |
| `relay` | `RelayNode` com um circuito real aberto e células arbitrárias em 4 links | sem pânico; circuitos ≤ `max_circuits` |
| `manager` | `CircuitManager` com cover ligado, células arbitrárias e relógio irregular | sem pânico |
| `link` | Link Noise `NK` num socket de loopback: o alvo faz o iniciador anônimo (handshake de verdade e quadro cifrado com texto claro arbitrário), manda bytes crus e faz o relay hostil | sem pânico nem trava; `read_cell` aceita exatamente o que `Cell::from_bytes` aceita; handshake forjado e quadro cru são recusados |
| `server_op` | `Op::decode`, `Op::verify` | sem pânico; operação aceita reproduz a entrada |
| `server_msg` | `wire::decode`, `wire::open`, `wire::seal` | sem pânico; mensagem aceita sobrevive a codificar de novo e a ir num quadro selado |
| `invite` | `Invite::open` com texto qualquer, com cabeçalho certo e bytes arbitrários, e com convite selado por quem convida com campos fora do formato | sem pânico; convite dentro do formato abre igual; o que abre sela de novo e abre igual |

```sh
cargo install cargo-fuzz
cargo fuzz run relay                       # nightly (rustup default nightly), com AddressSanitizer
cargo fuzz run relay -- -max_total_time=300

# Sem nightly, só com o Rust estável (sem sanitizer):
RUSTC_BOOTSTRAP=1 cargo fuzz run -s none relay -- -max_total_time=300
```

O crate `fuzz/` compila o `clam` com a feature `net` (o alvo `link` precisa do transporte), então a primeira compilação é mais demorada. Um crash fica em `fuzz/artifacts/<alvo>/`; reproduza com `cargo fuzz run <alvo> fuzz/artifacts/<alvo>/crash-...`. O corpus (`fuzz/corpus`) e os artefatos não vão para o git.

Resultado (Rust 1.93 estável, `-s none`, sem AddressSanitizer): nenhum crash nem violação de propriedade em nenhuma rodada.

| Alvo | Rodada curta (40 s) | Campanha (20 min, 6 alvos em paralelo) | Cobertura (arestas / corpus) |
|---|---|---|---|
| `cell` | sem crash | 899 M execuções | 37 / 8 |
| `descriptor` | sem crash | 63 M | 163 / 35 |
| `reputation` | sem crash | 184 M | 141 / 155 |
| `presence` | sem crash | 2,2 M | 107 / 1 |
| `relay` | sem crash | 0,6 M | 346 / 78 |
| `manager` | sem crash | 3,1 M | 395 / 135 |

Os alvos `link`, `server_op`, `server_msg` e `invite` rodaram 3 min cada, em paralelo, com AddressSanitizer (nightly), sem crash nem violação de propriedade: 60 mil, 2,9 M, 253 mil e 150 mil execuções (o `link` é o mais lento, 330 por segundo, porque cada execução faz um handshake Noise de verdade por TCP de loopback).

A cobertura de `cell`, `descriptor` e `presence` parou cedo, o que é esperado: o parser é pequeno, e `presence` rejeita quase tudo porque a entrada precisa ser um registro cifrado válido. Os alvos `relay` e `manager` são os que mais ganhariam com mais tempo (são lentos por execução e ainda achavam caminhos novos). Uma campanha de verdade ainda deve rodar horas por alvo e com AddressSanitizer. O alvo `link` só exercita o nosso código em volta do `snow` (tamanho do quadro, prazo, célula dentro do quadro): a criptografia em si é da biblioteca, e o fuzzer não forja uma autenticação válida.

### O que ainda falta na Fase 3

- Coletar as medições entre máquinas reais (roteiro e scripts prontos, nenhuma medição coletada).
- Campanha longa de fuzzing (horas por alvo, com AddressSanitizer em nightly) e um alvo novo para o gerente de descobertas.
