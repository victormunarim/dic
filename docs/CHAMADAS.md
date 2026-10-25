# Chamadas de voz, diretório e salas

Camada de mídia, telefone, diretório de primeiro contato, salas persistentes e a interface gráfica mínima.

[← Voltar ao README](../README.md)

## Camada de mídia: chamada de voz (`clam::media`)

Só voz 1:1. Vídeo, grupo (MLS) e Opus ficam de fora (veja "Limites"). A camada é *sans-IO*: não abre socket nem tem relógio; você entrega bytes e o tempo em ms.

```text
microfone ─> Codec::encode ─> VoiceCall::send ─> [200 B] ─> CircuitClient::send
alto-falante <─ Codec::decode <─ VoiceCall::playout <─ jitter <─ VoiceCall::receive <─ Event::Data
```

### Uso

```rust
use clam::media::{VoiceCall, codec::Mulaw, keys::{CallKeys, Ephemeral}};

// Cada lado gera uma efêmera e a manda ao outro dentro do convite/resposta da chamada.
let eph = Ephemeral::generate();
// Chamador: my_static = sua chave do pareamento, peer_static = a do contato.
let keys = CallKeys::initiator(&my_static, &eph, &peer_static, &peer_eph)?;
// Chamado: CallKeys::responder(...) com os mesmos argumentos espelhados.
let mut call = VoiceCall::new(&keys, Mulaw::new());

// A cada 20 ms, com 160 amostras i16 (8 kHz) do microfone:
let pacote = call.send(&pcm)?;           // 200 bytes -> CircuitClient::send(&pacote)
// Ao chegar Event::Data(bytes):
call.receive(agora_ms, &bytes)?;         // erro = pacote descartado (forjado, repetido, tamanho errado)
// A cada 20 ms, também:
for quadro in call.playout(agora_ms) { /* despeje no alto-falante */ }
```

`cargo run --example voice -- --loss 5 --jitter-ms 60 --seconds 10` simula uma chamada com perda e jitter e imprime as estatísticas.

### O que cada parte faz

- **Chaves (`keys`)**: handshake de 3 DH no estilo Noise `KK` (efêmera×efêmera, estática×efêmera nos dois sentidos) + HKDF-SHA256 com a transcrição. Sigilo adiante por chamada, autenticação mútua pelas chaves do pareamento, uma chave por direção. Não há mensagem de confirmação: o primeiro pacote válido confirma. Pontos de ordem baixa são recusados (`MediaError::BadKey`).
- **Pacote (`packet`)**: `seq (4, autenticado) ‖ ChaCha20-Poly1305(len ‖ quadro ‖ zeros)`, **sempre 200 bytes**, cabe nos 203 de uma célula de 2 saltos. Quadro de fala, de silêncio ou de tamanho diferente: mesmo tamanho. Janela anti-replay de 128 pacotes; o `seq` só é marcado como visto depois de autenticar.
- **Jitter (`jitter`)**: reordena, marca perda e adapta o atraso. Começa em 2 quadros (40 ms); sobe 1 quadro a cada quadro que chega tarde demais, desce 1 depois de 250 quadros em dia; limite 200 ms. Salto de 50 quadros para frente reinicia o buffer.
- **Codec (`codec`)**: trait `Codec` (`encode`, `decode(None)` = perda com PLC). Vêm o G.722 (16 kHz, 320 amostras ↔ 160 B por quadro de 20 ms, 64 kbps, banda larga; usado pela GUI) e o G.711 µ-law (8 kHz, 160 B, banda estreita). Os dois lados precisam usar o mesmo codec. PLC: continua a onda repetindo o último período de pitch do quadro anterior, com fade linear até o silêncio em 3 quadros (60 ms); quando o áudio volta, o começo do primeiro quadro bom é misturado com a continuação para não haver degrau.
- **Sinalização (`signal`)**: `Caller::invite` → `Incoming::parse` → `accept`/`reject` → `Caller::answer`, em quadros de 200 B. O convite leva a identidade de quem liga **cifrada** (chave de `DH(e_i, s_r)`) e a hora de quem liga (só para telefones de versão anterior): o relay de saída vê que há um convite, mas não quem liga. Quem finge ser um contato não deriva as chaves da chamada. O convite não depende do relógio de quem recebe: o telefone lembra as efêmeras dos últimos 1024 convites e não toca duas vezes pelo mesmo. Replay de um convite gravado depois de o telefone reiniciar só o faz tocar; não dá acesso à chamada. Um segundo bloco cifrado do convite leva a *dica de relay* de quem liga (`RelayHint`, aleatório quando não há), para o chamado ligar de volta sem diretório; como a identidade, não é autenticada, então só é usada depois do endereço guardado e do diretório e só é promovida depois de uma chamada autenticada.
- **Desligar**: pacote autenticado do mesmo tamanho dos de voz (campo `tipo` dentro da cifra); ninguém de fora derruba a chamada.
- **Sem DTX**: o cover do circuito já manda 1 célula a cada 20 ms, então calar não economiza banda e só revelaria quando você fala.

Banda total por sentido com o circuito: 1 célula de 256 B por 20 ms (~100 kbps), a mesma com ou sem fala.

### Limites

- **Opus não está incluído**: exige `libopus` (C) e `cmake` neste ambiente. Implemente `Codec` com a crate de sua escolha; o quadro codificado deve ter até 167 bytes (`MAX_PAYLOAD`), sobra para Opus a 32–64 kbps em 20 ms.
- Sem controle de congestionamento nem retransmissão (voz tolera perda), sem relógio de áudio (o app amostra o dispositivo e lida com deriva).
- `tests/media.rs` cobre rede limpa, perda de 5% com jitter e duplicatas, pacotes forjados e áudio bit a bit idêntico ao do codec. Não há teste sobre o circuito real.

### Telefone: chamar e atender pelo circuito (`clam::phone`, feature `net`)

`media` é sans-IO; `phone` liga tudo a tokio, ao circuito e a um destino TCP.

**Quem atende** (papel de destino). `Phone` escuta num endereço TCP; é o endereço que a política de saída dos relays mapeia (`contato:9`, veja `allow_list`). Cada circuito chega como uma conexão com quadros `[u16 BE tamanho][dados]`.

```rust
use clam::phone::Phone;
use clam::media::codec::Mulaw;

// contatos: chaves públicas estáticas (32 bytes) dos pareamentos que podem ligar.
let mut phone = Phone::bind("0.0.0.0:7000".parse()?, minha_chave, contatos).await?;
while let Some(convite) = phone.next_incoming().await {
    println!("liga: {:x?}", convite.caller().as_bytes());
    let mut chamada = convite.accept(Mulaw::new()).await?;   // ou convite.reject().await
    // microfone -> chamada.send(pcm).await ; alto-falante <- chamada.next_pcm().await
}
```

**Quem liga**:

```rust
use clam::phone::call;

let cliente = CircuitClient::spawn(CircuitManager::new(cfg, "contato:9", relays), Duration::from_secs(3));
let mut chamada = call(cliente, &minha_chave, &chave_do_contato, Mulaw::new(), Duration::from_secs(10)).await?;
// Erros: CallError::{NoCircuit, NoAnswer, Rejected, Closed, Media}
```

- `ActiveCall::send(pcm)`: um quadro do microfone (160 amostras com `Mulaw`), a cada 20 ms. `next_pcm()`: o próximo quadro para o alto-falante, no ritmo do codec; vem quadro escondido (PLC) quando falta pacote, então o fluxo não para até a chamada acabar (`None`). `stats()`, `hangup()` (ou largar o valor) desliga.
- **Contatos desconhecidos** não tocam e **não recebem resposta**: o telefone não revela que existe. `contacts` vazio = ninguém liga.
- **Failover**: o circuito reserva abre outra conexão com o `Phone`; se o ativo cai, a voz passa a chegar por ela sem convite. O `Phone` experimenta o pacote nas chamadas em andamento e a que o autenticar passa a responder por aquela conexão. Pacote repetido nunca muda a rota.
- **Desligar** sai em 3 cópias (20 ms entre elas) porque a voz tolera perda, mas o desligar não deveria depender de uma célula só.
- `tests/phone.rs` cobre: chamada por relays reais com áudio nos dois sentidos e desligar; recusa; desconhecido sem resposta; troca de conexão (failover) sem convite; e queda do primeiro relay do circuito ativo no meio da chamada (6 relays, voz nos dois sentidos): a voz volta sozinha com lacuna de ~400 ms em 127.0.0.1 (limite do teste 800 ms), sem novo convite.
- Limites: o `Phone` aceita TCP de qualquer origem (não só dos relays), então protege-se só pela autenticação dos pacotes: quem não é contato é ignorado, mas pode gastar CPU com lixo. Uma chamada por contato e por vez não é imposta. Não há toque/ocupado, e a chamada em grupo não existe.

### Transmissão de tela (`clam::media::screen`, `clam::phone`)

A tela anda num **circuito só dela**, aberto por quem transmite até o telefone de quem assiste, com cover de 2 ms nos links e no circuito (500 pacotes/s, ≈ 610 kbps de vídeo; veja `phone::SCREEN_SLOT_MS`). O ritmo é constante: parar a tela, mexer o mouse ou trocar de janela não muda nada no fio. O codec deve usar bitrate constante dentro de `ScreenStream::capacity_bps`.

- Chaves: `CallKeys::sub` deriva, das chaves da chamada, um par por sentido de quem transmite (`screen/caller` ou `screen/callee`), então os dois lados podem transmitir ao mesmo tempo sem reusar nonce.
- Pacotes: os mesmos 200 B cifrados da voz, com cabeçalho de 5 bytes (flags, id do quadro, índice, total). Quadro de até 255 fragmentos; a remontagem aceita fora de ordem.
- Perda: sem FEC (a taxa é fixa). Perdeu fragmento, os quadros seguintes não são entregues até chegar um quadro-chave, que quem assiste pede (`ScreenStream::needs_key`, no máximo 10 pedidos por segundo) e quem transmite lê em `ScreenShare::keyframe_wanted`. Fila de saída acima de 400 fragmentos descarta quadros velhos e exige quadro-chave.
- Sinalização: `phone::share_screen` manda um pacote de abertura que prende o circuito à chamada do outro lado (só quem tem as chaves da chamada o autentica; outros pacotes nunca viram convite). `ScreenShare::stop` avisa quem assiste (`ScreenEvent::Stopped`).
- Quem assiste lê `ActiveCall::next_screen`. Para a chamada que *ligou* receber a tela, ela precisa estar registrada no telefone da própria máquina (`Phone::link`, `call_linked`); `Node::dial` já faz isso.
- Codec (feature `video`, módulo `clam::video`): `H264Encoder` (openh264, compilado da fonte, sem `cmake` nem `nasm`) em taxa constante, 90% da capacidade do circuito (`H264Encoder::default_bitrate`), 15 quadros/s, quadro-chave a cada 30 s e sob demanda; duas camadas temporais (os quadros ímpares não servem de referência: com a fila do circuito acima de 40 fragmentos `pump_all` os larga e a taxa de quadros cai sem perda nem quadro-chave, e acima de 120 a resolução cai à metade até a fila baixar por 2 s); `H264Decoder` devolve `RawFrame` RGBA. `pump(fonte, codificador, share)` faz um passo captura → codifica → `ScreenShare::push` e atende `keyframe_wanted`. Dimensões ímpares são cortadas para pares; mudar de resolução reabre o codificador com quadro-chave. Quadro acima de `MAX_FRAME` é descartado e o próximo sai como quadro-chave.
- Captura X11 (feature `capture`, `clam::capture`): `X11Screen` lê a janela raiz por `GetImage`. Sob Wayland o XWayland recusa a leitura (`BadMatch`) e `X11Screen::open` devolve `Unreadable`.
- Captura Windows (feature `capture`, `clam::capture::GdiScreen`): `BitBlt` da tela principal para um bitmap, via `windows-sys`; o processo se declara ciente de DPI para pegar a resolução real. `X11Screen`, `PortalScreen` e suas dependências (`x11rb`, `ashpd`, `rustix`) só existem no Unix; no Windows a GUI usa só o GDI. O código GDI foi verificado só por compilação cruzada do módulo isolado (o `openh264` precisa de mingw para compilar o crate inteiro), sem execução em Windows.
- Captura Wayland (feature `portal`, Unix, `clam::portal`): `PortalScreen::open().await` pede ao portal `ScreenCast` (diálogo do compositor: monitor ou janela, a cada transmissão) e lê o PipeWire por um `gst-launch-1.0` filho que entrega RGBA a 15 quadros/s, reduzido a no máximo 1920 de largura. Não há dependência de compilação do PipeWire; em tempo de execução precisa de `gstreamer1.0-tools`, `gstreamer1.0-plugins-base` e `gstreamer1.0-pipewire`. Teste manual: `cargo test --features portal --lib -- --ignored real_portal`. Outra fonte entra implementando `ScreenSource`.
- GUI: no cartão "Em chamada", "Compartilhar minha tela" abre o circuito de tela e transmite a 15 quadros/s; a tela de quem transmite para você aparece no mesmo cartão. A feature `gui` inclui `capture` e `portal`: sob Wayland usa o portal (a pessoa escolhe a tela no diálogo do sistema), nos outros casos o X11, e uma serve de reserva da outra. Na sala, o botão aparece no cartão "Salas": quem transmite abre um circuito de tela para cada membro conectado (N-1 circuitos, cerca de 770 kbps de cobertura cada, `video::pump_all` codifica uma vez e empurra o mesmo quadro a todos); quem entra depois ganha o circuito e um quadro-chave, e a tela de cada membro aparece com o nome dele. Ao cair a ligação a transmissão para e é preciso ligar de novo.

### Conexão simples: diretório só para o primeiro contato

Só o **primeiro contato** usa servidor. Um diretório (`src/directory/`) guarda, por 120 s, "chave pública → relay" de quem está online; cada `Node` (`src/node.rs`) republica a sua entrada a cada 40 s. Para ligar, você consulta a chave do contato uma vez, monta o circuito de 2 saltos até o relay dele e o resto (convite, voz) segue P2P pelos relays, sem passar pelo diretório.

- Quem publica prova a posse da chave sem assinatura (DH com a chave do servidor + HMAC do pedido); ninguém publica por outra chave.
- O diretório vê IPs e chaves de quem publica e consulta, nunca o conteúdo das chamadas.
- Cada pessoa é relay de si mesma. Com só duas pessoas, o primeiro salto é o relay do chamador e o chamado vê o IP dele; com mais gente online, o primeiro salto é sorteado do diretório.
- **Relógio:** o diretório manda a hora dele no primeiro contato e todos usam essa hora nos convites (`src/clock.rs`). Hora errada numa máquina não impede a chamada nem com o diretório fora do ar: o convite não é conferido contra o relógio de quem recebe.
- **Atrás de roteador:** com o diretório na internet, o nó pede ao roteador para encaminhar a porta do relay por UPnP (`src/upnp.rs`, renovado a cada 40 s) e publica o IP externo. Sem UPnP, ou com CGNAT, a janela avisa o motivo; então abra a porta 4001 ou informe `public_addr`. Se o diretório está na LAN (IP privado), o UPnP não é usado.
- **Furo de NAT por UDP** (`src/punch.rs`, `NodeConfig.punch`, padrão ligado): o nó usa um socket UDP que fala com o diretório (mesma porta do TCP; ele responde de onde o pacote veio, como um STUN) e mantém o mapeamento do roteador aberto. Quem aceita chamadas mantém um controle com o diretório (`REQ_PUNCH_LISTEN`); quem chama pede o furo (`REQ_PUNCH_PREP`/`REQ_PUNCH_GO`), o diretório avisa o ouvinte do endereço UDP do chamador e devolve o do ouvinte, e os dois sondam um ao outro até abrir. O convite e a voz (já cifrados ponta a ponta) seguem direto por UDP, sem circuito e sem relay. Se o alvo não aceita furos ou o furo não abre em 6 s, `Node::dial` cai no circuito. **Limite:** NAT simétrico nos dois lados não fura. O diretório vê os dois IPs no momento do furo. Cobertura: `tests/node.rs` (loopback; não prova um NAT real).
- **Teste de alcançabilidade** (`directory::reachable`, `REQ_REACH`): depois de publicar, o nó pede ao diretório que tente abrir TCP no relay publicado (só com o diretório na internet). Se não abrir, `Node::network_note` avisa que ninguém consegue ligar pelo circuito (ou que só o furo de NAT funciona). O pedido exige a prova de posse e o servidor espera 20 s entre testes da mesma chave, para não servir de varredor de portas. Cobertura: `tests/node.rs`.

Servidor (uma máquina alcançável pelos dois lados):

```text
cargo run --release --features net --example directory -- 0.0.0.0:7777
```

### Salas persistentes (`clam::room`, feature `net`)

Como uma sala do Discord: a sala é um **segredo de 32 bytes** (64 hex) que fica salvo na máquina de quem já entrou. Quem o tem entra de novo quando quiser; para convidar, copie o código da sala.

- O diretório identifica a sala por uma chave Ed25519 derivada do segredo (HKDF). Entrar, sair e listar são pedidos assinados por ela (com a `E` da conexão, sem repetição): sem o segredo não se entra nem se vê quem está lá.
- A lista é de quem está presente (renovada a cada sincronização, vence em `TTL`). O **primeiro membro vivo é o anfitrião**; se ele sai, o próximo assume. Até `MAX_ROOM_MEMBERS` (8) por sala.
- **Malha completa**: o menor `chave pública` de cada par liga para o outro; a voz segue de ponta a ponta entre cada par. `room::Mesh` envia o microfone a todos e mistura o que chega com um limitador (acima de pico 30000 o ganho cai na hora, sem cortar a onda, e volta em ~200 ms). `room::connect_missing` disca em segundo plano e em paralelo (um membro ausente não trava ninguém), com espera crescente de 2 a 16 s após falhas; a GUI repete a cada 2 s, então uma chamada que cai volta sozinha.
- Áudio: a troca de taxa do dispositivo usa filtro sinc com janela de Blackman (corte em 45% da menor taxa, avaliado na posição fracionária exata), em vez de média de caixa e interpolação linear.
- Os membros listados são liberados no telefone (`Node::allow`), então ninguém precisa estar na lista de Contatos.
- Na GUI: "Criar / entrar" (código vazio cria uma sala nova), "Copiar código", "Entrar" nas salas salvas, "Sair da sala". As salas ficam em `~/.clam-gui.conf` (campo `rooms`, contém segredos).
- Limite: o diretório vê quem entra em cada sala (chaves e IPs), não a voz.

### Interface gráfica mínima

Guias passo a passo por sistema: [`docs/LINUX.md`](LINUX.md) e [`docs/WINDOWS.md`](WINDOWS.md).

```text
cargo run --release --features gui --example gui
```

1. Na primeira abertura a identidade já é criada sozinha (fica em *Configurações avançadas*, oculta). O **código de contato** (`chave@host:porta`, ou, sem diretório, o código direto `chave#ip:porta#circuito#link` que leva o seu relay; veja `docs/GUI.md`, que também conta como os contatos recebem o endereço novo quando o seu muda) aparece em *Meu código de contato* com *Copiar*; passe-o a quem vai te chamar.
2. Informe o **servidor (diretório)** `host:porta` e clique **Ficar online**. O indicador mostra Offline / Conectando / Online.
3. **Contatos**: cada um tem *Ligar* e *Remover*; para adicionar, nome e código de contato. Quem não estiver na lista (nem numa sala sua) não recebe resposta; contatos são atendidos automaticamente. *Ligar para um código* aceita um código ou nome sem salvar. **Desligar** encerra.
4. Os campos são gravados em `~/.clam-gui.conf` (`%USERPROFILE%` no Windows), **inclusive a chave secreta**; no Unix o arquivo fica `0600`.
5. **Áudio real** (feature `audio`): `--features gui,audio` e marque *Microfone e alto-falante*. Usa `cpal`, 16 kHz mono (G.722), filas de 400 ms. O microfone passa pelo `sonora` (WebRTC em Rust puro): filtro passa-altas, cancelamento de eco, supressão de ruído e ganho automático (cada estágio se liga e desliga com `Audio::set_processing(Processing)`, e `Audio::echo_stats()` diz quantos dB de eco saem; a referência do eco é o que saiu pelo alto-falante, contínua (com o silêncio), em pedaços inteiros de 10 ms entregues assim que chegam, sem zeros de preenchimento; `echo_stats().erle_db` é medido na energia do microfone antes e depois; a GUI desliga os que ela mesma faz, ver `docs/GUI.md`). Fones ainda são o mais seguro. Sem a opção, cai para tom (440 Hz quem liga, 660 Hz quem atende). No Linux a compilação exige `pkg-config` e `libasound2-dev`; com PulseAudio/PipeWire rodando o `cpal` usa o servidor (a lista de dispositivos mostra os nomes do sistema), senão o ALSA.
6. `eframe` fica atrás da feature `gui`, fixado em 0.32 (a 0.37 conflita com o libp2p por `js-sys`).

O fluxo antigo, com relays manuais, continua em `examples/call.rs` e `examples/measure.rs`.
