# Chamadas de voz

Camada de mídia, telefone, tela, conexão entre contatos sem servidor e a interface gráfica. Para a visão de ponta a ponta de como tudo se encaixa, comece por [COMUNICACAO.md](COMUNICACAO.md).

[← Voltar ao README](../README.md)

## Camada de mídia: chamada de voz (`clam::media`)

Voz 1:1 (a tela tem a sua seção, mais abaixo; o grupo, com MLS, não existe). A camada é *sans-IO*: não abre socket nem tem relógio; você entrega bytes e o tempo em ms.

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
- **Codec (`codec`)**: trait `Codec` (`encode`, `decode(None)` = perda com PLC). Vêm o G.722 (16 kHz, 320 amostras ↔ 160 B por quadro de 20 ms, 64 kbps, banda larga; o que vale quando o convite não diz), o Opus (o padrão da GUI; 16 kHz, 20 ms, 32 kbps com taxa variável, até `MAX_PAYLOAD` B por quadro, Rust puro pela crate `opus-pure`; sem DTX e sem FEC, a perda é escondida pelo PLC do próprio Opus) e o G.711 µ-law (8 kHz, 160 B, banda estreita). **Quem liga escolhe o codec** (`CodecId`, `Caller::invite_codec`) e o convite o diz: nos 12 bytes finais, sorteio de 4 bytes ‖ 8 bytes de PRF da chave do convite, que só o destinatário confere (`Incoming::codec`, `IncomingCall::codec`); G.722 não leva etiqueta, então o convite dele é igual ao de um telefone antigo, que também ignora a etiqueta e atende em G.722. Ligar em Opus exige os dois lados desta versão. Quem atende deve usar `incoming.codec().build()` (`AnyCodec`). PLC: continua a onda repetindo o último período de pitch do quadro anterior, com fade linear até o silêncio em 3 quadros (60 ms); quando o áudio volta, o começo do primeiro quadro bom é misturado com a continuação para não haver degrau.
- **Sinalização (`signal`)**: `Caller::invite` → `Incoming::parse` → `accept`/`reject` → `Caller::answer`, em quadros de 200 B. O convite leva a identidade de quem liga **cifrada** (chave de `DH(e_i, s_r)`) e a hora de quem liga (só para telefones de versão anterior): o relay de saída vê que há um convite, mas não quem liga. Quem finge ser um contato não deriva as chaves da chamada. O convite não depende do relógio de quem recebe: o telefone lembra as efêmeras dos últimos 1024 convites e não toca duas vezes pelo mesmo. Replay de um convite gravado depois de o telefone reiniciar só o faz tocar; não dá acesso à chamada. Um segundo bloco cifrado do convite leva a *dica de relay* de quem liga (`RelayHint`, aleatório quando não há), para o chamado ligar de volta; como a identidade, não é autenticada, então só é usada depois do endereço guardado e só é promovida depois de uma chamada autenticada.
- **Desligar**: pacote autenticado do mesmo tamanho dos de voz (campo `tipo` dentro da cifra); ninguém de fora derruba a chamada.
- **Sem DTX**: o cover do circuito já manda 1 célula a cada 20 ms, então calar não economiza banda e só revelaria quando você fala.

Banda total por sentido com o circuito: 1 célula de 256 B por 20 ms (~100 kbps), a mesma com ou sem fala.

### Limites

- **Opus vem em Rust puro** (crate `opus-pure`, sem `libopus` nem `cmake`). Um codec seu entra implementando `Codec`; o quadro codificado deve ter até 178 bytes (`MAX_PAYLOAD`), o que sobra para Opus a 32–64 kbps em 20 ms.
- A voz não tem controle de congestionamento nem retransmissão (tolera perda; a tela tem as duas coisas, veja abaixo), e não há relógio de áudio (o app amostra o dispositivo e lida com deriva).
- `tests/media.rs` cobre rede limpa, perda de 5% com jitter e duplicatas, pacotes forjados e áudio bit a bit idêntico ao do codec. O circuito real é coberto por `tests/phone.rs`, mais abaixo.

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

A tela anda num **circuito só dela**, aberto por quem transmite até o telefone de quem assiste, com cover de 2 ms nos links e no circuito (500 pacotes/s, capacidade de 684 kbps de vídeo e orçamento do codec de ≈ 615 kbps; veja `phone::SCREEN_SLOT`). O ritmo é constante: parar a tela, mexer o mouse ou trocar de janela não muda nada no fio. O codec fica dentro de `ScreenStream::capacity_bps`.

- **Privacidade opcional.** O ritmo constante e o cover são a privacidade da tela, e custam banda parada. `Dialer::share_screen_keys_with(.., private = false)` abre o circuito sem cover (`phone::open_screen_client_options`) e com um pacote a cada `phone::OPEN_SCREEN_SLOT` = 0,5 ms, ou seja, quatro vezes o ritmo (2000 pacotes/s, capacidade de 2,7 Mbps e orçamento do codec de ≈ 2,5 Mbps, `H264Encoder::bitrate_for`; 512 kB/s no fio) e 30 quadros/s em vez de 15 (`video::fps_for`). Os relays precisam deixar passar ≈ 512 kB/s por circuito: o padrão de `RelayConfig` é 800 kB/s por circuito e 2 MB/s no relay; relays antigos, com 400 kB/s, seguram a imagem. Só sai pacote quando há vídeo, então quem observa a rede vê quando a tela se mexe; o conteúdo e as chaves seguem cifrados e o caminho é o mesmo. Na GUI é a caixa "Privacidade na transmissão da tela" da aba Vídeo (ligada de início; desmarcar grava `no_screen_privacy=1` no `.clam-gui.conf`), que vale a partir da próxima transmissão.
- **Fluidez ou nitidez.** A taxa de bits é uma só; a escolha é como repartir: `video::Priority::Smoothness` (padrão) manda `fps_for(taxa)` quadros por segundo (30 sem privacidade, 15 com) e `Priority::Sharpness` manda a metade, com o dobro de bits em cada quadro (`H264Encoder::with_priority`). Fluidez serve a jogo e vídeo; nitidez, a texto, código e slides, onde pouca coisa muda por instante e cada quadro fica bem mais fino. Na GUI são os botões "Priorizar: Fluidez / Nitidez" ao lado da privacidade, na aba Vídeo (fluidez de início; nitidez grava `screen_sharp=1` no `.clam-gui.conf`), que valem a partir da próxima transmissão.
- Chaves: `CallKeys::sub` deriva, das chaves da chamada, um par por sentido de quem transmite (`screen/caller` ou `screen/callee`), então os dois lados podem transmitir ao mesmo tempo sem reusar nonce.
- Pacotes: os mesmos 200 B cifrados da voz, com cabeçalho de 7 bytes (flags, id do quadro, índice, total, 2 B cada), o que deixa 171 B de vídeo por pacote. Quadro de até 1000 fragmentos; a remontagem aceita fora de ordem.
- **Perda: reenvio do que faltou, sem FEC.** Quem assiste vê as lacunas (fragmento que falta dentro de um quadro, ou quadro inteiro que não apareceu) e as pede de volta (`NACK`, até 40 pares quadro/índice por pacote de preenchimento, no mesmo pacote do relatório de perda; cada quadro só é pedido de novo depois de 250 ms). Quem transmite guarda os últimos 660 fragmentos que saíram (`media::screen::HISTORY`) e reenvia o pedido à frente da fila, em pacote novo (sequência e cifra novas; no fio é um pacote como os outros, e o ritmo do slot não muda). Os quadros esperam o reparo e saem em ordem, então um fragmento perdido custa uma ida e volta no circuito e não um quadro-chave. Não se reenvia o mesmo fragmento em menos de 120 ms nem mais de 128 de uma vez. Quadro-chave novo largou o histórico (o que era anterior não serve mais). FEC ficou de fora: num fluxo de taxa fixa ele gastaria banda toda hora, e o reenvio só gasta onde há perda.
- Reparo que não veio em 800 ms desde o último fragmento do quadro (`media::screen::PATIENCE`): o quadro é abandonado (`frames_lost`), os seguintes não são entregues até chegar um quadro-chave, que quem assiste pede (`ScreenStream::needs_key`, no máximo 10 pedidos por segundo) e quem transmite lê em `ScreenShare::keyframe_wanted`. Fila de saída acima de 660 fragmentos descarta quadros velhos e exige quadro-chave. Contra um emissor de versão anterior (que ignora o `NACK`), a imagem espera 800 ms antes de pedir o quadro-chave, o que antes era imediato. Até 48 quadros ficam esperando reparo.
- Sinalização: `phone::share_screen` manda um pacote de abertura que prende o circuito à chamada do outro lado (só quem tem as chaves da chamada o autentica; outros pacotes nunca viram convite). `ScreenShare::stop` avisa quem assiste (`ScreenEvent::Stopped`).
- Quem assiste lê `ActiveCall::next_screen`. Para a chamada que *ligou* receber a tela, ela precisa estar registrada no telefone da própria máquina (`Phone::link`, `call_linked`); `Node::dial` já faz isso.
- **Taxa adaptativa.** Quem assiste conta as lacunas no número de sequência dos pacotes e, a cada 0,5 s (com ao menos 16 pacotes na janela), devolve a fração perdida num pacote de preenchimento (`ScreenStream::report_packet`). `pump`/`pump_all` leem o relatório (`ScreenShare::take_report`) e `H264Encoder::report_loss` aplica um AIMD (`video::RateControl`): perda acima de 2% corta a taxa em metade da perda (20% de perda tira 10%) e memoriza um teto 10% abaixo; sem perda ela sobe 8% por relatório até esse teto, que sobe 10% a cada 5 s limpos para testar se a banda voltou. O piso é um oitavo da capacidade do circuito (mínimo 150 kbps). Se os relatórios somem por 4 s com ao menos 300 pacotes de vídeo saindo, a taxa cai 30% (a cada 2 s). Com vários espectadores vale o pior. Quem assiste numa versão sem relatórios não derruba a taxa: o corte por silêncio só vale depois do primeiro relatório. A GUI mostra a taxa atual no diagnóstico ("taxa N kbps"). Com privacidade o ritmo no fio não muda (o cover preenche); só sobe o tráfego de volta, um pacote a cada meio segundo.
- Codec (feature `video`, módulo `clam::video`): `H264Encoder` (openh264, compilado da fonte, sem `cmake` nem `nasm`), 15 quadros/s (30 quando a taxa passa de 1,5 Mbps, `video::fps_for`), orçamento de 90% da capacidade do circuito (`H264Encoder::default_bitrate`), quadro-chave a cada 30 s e sob demanda; `H264Decoder` devolve `RawFrame` RGBA. `pump(fonte, codificador, share)` faz um passo captura → codifica → `ScreenShare::push` e atende `keyframe_wanted`. Dimensões ímpares são cortadas para pares; mudar de resolução reabre o codificador com quadro-chave.
- **Fatias.** O quadro sai em fatias de até `SLICE_LEN` = 1500 B (`max_slice_len`), o que deixa o openh264 repartir o trabalho entre os núcleos: um quadro de 1080p com texto leva ≈ 20 ms em vez de ≈ 26 ms (medido; `num_threads` e a complexidade não mudavam nada sem fatias). Custa algo como 20% a mais de bits no mesmo QP, que o controle de taxa absorve subindo o QP. A medida importa porque a 30 quadros/s cada quadro tem 33 ms para capturar e codificar. Teste: `sliced_full_hd_frames_decode_close_to_the_source`.
- **Controle de taxa próprio.** O controle de taxa do openh264 não respeita o orçamento por quadro (medido com texto rolando a 1080p: de 3 a 10 vezes mais, até 600 kB na troca de página), então ele fica desligado e o QP de cada quadro é nosso:
  - O QP vem do tamanho dos quadros anteriores (acima do orçamento sobe até 12 de uma vez, abaixo desce 2 por quadro, faixa 28 a 51) e salta até 14 quando a imagem muda de repente (fração de pixels amostrados que mudaram, comparada à do quadro anterior). Assim o primeiro quadro depois de uma rolagem ou troca de tela já sai grosseiro, em vez de gastar o orçamento de segundos.
  - **`pump_all` adia o quadro** enquanto a fila do circuito tem mais de 20 fragmentos (≈ 40 ms). A fila nunca acumula quadros velhos, o que mantém o atraso baixo e evita o estouro de `MAX_QUEUE`, que descartava a fila inteira e pedia quadro-chave. Sob movimento brusco a taxa de quadros cai (o circuito é a taxa, não o codificador), mas o que chega é sempre a tela atual.
  - Imagem parada: o quadro refina a nitidez um QP por quadro. Um pico de refinamento acima de 8 orçamentos fixa o piso de QP até a imagem se mexer de novo, e quadro acima de `FRAME_LIMIT` (o que o circuito aceita, menor que `MAX_FRAME`) é descartado e o próximo sai como quadro-chave em QP 51, com piso acima do QP que estourou.
  - Movimento que nem o QP 51 segura (3 quadros seguidos acima de 2 orçamentos) reduz a resolução em mais um degrau (até 1/3); 1 s de imagem parada restaura. O openh264 manda quadro-chave por conta própria quando a cena muda muito (a detecção de cena não se desliga em conteúdo de tela).
  - Duas camadas temporais: os quadros ímpares não servem de referência e, com a fila acima de 40 fragmentos, `pump_all` os larga (cai a taxa de quadros, sem perda nem quadro-chave). Simulação com página de texto 960×540 (`brusque_movement_never_piles_up_frames_in_the_circuit_queue`): nenhum quadro recusado pelo circuito e fila de no máximo ≈ 0,66 s.
- **Codificação por hardware (experimental, só Linux).** `clam::hwenc::HwEncoder`, ligado por `H264Encoder::set_hardware(Some(elemento))` (a GUI: "Codificar a tela pela placa de vídeo (experimental)" na aba Vídeo, desligado de início; a chave salva é `screen_hardware=1`). `hwenc::detect()` procura, com `gst-inspect-1.0 --exists`, o primeiro entre `vah264lpenc`, `vah264enc`, `nvh264enc`, `qsvh264enc` e `vaapih264enc`. O codificador é um processo filho `gst-launch-1.0` (`fdsrc ! rawvideoparse ! videoconvert ! NV12 ! <codificador> ! h264parse ! gdppay ! fdsink`): quadros RGBA entram pelo stdin, e a saída vem em quadros GDP, de onde se tira cada unidade de acesso e a marca de quadro dependente (`DELTA_UNIT`). Limites, todos por o filho não aceitar comandos:
  - Sem controle de QP por quadro nem camadas temporais (`droppable` é sempre falso); o controle de taxa é o do codificador (CBR no bitrate que o `RateControl` dá).
  - Pedido de quadro-chave, mudança de resolução, corte de taxa acima de 25% e aumento acima de 33% (só depois de 10 s) **reiniciam o filho**, que abre com IDR. O aumento de taxa é raro de propósito.
  - A saída chega com atraso de alguns quadros; `encode` devolve o mais antigo já pronto.
  - Quadro acima de `FRAME_LIMIT` baixa a resolução (`min_k + 1`) e pede quadro-chave. Filho que não produz nada em 8 s, ou morre 3 vezes seguidas, faz o `H264Encoder` voltar ao openh264 (`hw_note` diz o motivo, e a GUI o mostra na linha de diagnóstico).
  - Precisa de `gstreamer1.0-tools`, `gstreamer1.0-plugins-bad` (e `-good`/`-base` para `videoconvert`) e o driver da GPU (`intel-media-va-driver`, `mesa-va-drivers`, NVENC da NVIDIA...). No Windows `detect()` devolve `None`.
  - **Verificado só com `x264enc` no lugar do codificador de GPU** (a máquina de desenvolvimento não tem GPU com codificação H.264 utilizável): a tubulação, o parser GDP, os reinícios e a queda para o openh264 têm teste; as propriedades de cada codificador de GPU (`ELEMENTS`) vêm da documentação do GStreamer e **não foram executadas**.
- Captura X11 (feature `capture`, `clam::capture`): `X11Screen` lê a janela raiz por `GetImage`. Sob Wayland o XWayland recusa a leitura (`BadMatch`) e `X11Screen::open` devolve `Unreadable`.
- Captura Windows (feature `capture`, `clam::capture::GdiScreen`): `BitBlt` da tela principal para um bitmap, via `windows-sys`; o processo se declara ciente de DPI para pegar a resolução real. `X11Screen`, `PortalScreen` e suas dependências (`x11rb`, `ashpd`, `rustix`) só existem no Unix; no Windows a GUI usa só o GDI. O código GDI foi verificado só por compilação cruzada do módulo isolado (o `openh264` precisa de mingw para compilar o crate inteiro), sem execução em Windows.
- Captura Wayland (feature `portal`, Unix, `clam::portal`): `PortalScreen::open().await` pede ao portal `ScreenCast` (diálogo do compositor: monitor ou janela, a cada transmissão) e lê o PipeWire por um `gst-launch-1.0` filho que entrega RGBA a 15 quadros/s (ou a taxa pedida em `PortalScreen::open_with`), reduzido a no máximo 1920 de largura. Não há dependência de compilação do PipeWire; em tempo de execução precisa de `gstreamer1.0-tools`, `gstreamer1.0-plugins-base` e `gstreamer1.0-pipewire`. Teste manual: `cargo test --features portal --lib -- --ignored real_portal`. Outra fonte entra implementando `ScreenSource`.
- GUI: na aba Chamada, o cartão "Compartilhar a minha tela" abre o circuito de tela e transmite a 15 quadros/s (30 sem privacidade); a tela de quem transmite para você aparece num cartão "Tela de ..." da mesma aba. A linha de diagnóstico mostra taxa (kbps), pacotes/s, QP, resolução (imagem 1/N) e quadros adiados. A feature `gui` inclui `capture` e `portal`: sob Wayland usa o portal (a pessoa escolhe a tela no diálogo do sistema), nos outros casos o X11, e uma serve de reserva da outra. Ao cair a ligação a transmissão para e é preciso ligar de novo.

### Conexão sem servidor

Não há servidor nem diretório: cada pessoa é relay de si mesma e os contatos se acham entre eles. Para ligar, o `Dialer` (`src/node.rs`) monta o circuito de 2 saltos até o relay do contato e o resto (convite, voz) segue P2P pelos relays. O endereço do contato vem, nesta ordem, do que já se tem dele (o último endereço guardado de uma chamada, o do código direto `chave#ip:porta#circuito#link`, o histórico), do aviso que ele mandou ao mudar de endereço e, se ligados (a GUI liga a DHT e o serviço de âncora de início), da DHT e das âncoras (`docs/ENCONTRO.md`). O passo a passo completo está em [COMUNICACAO.md](COMUNICACAO.md#fluxo-de-uma-chamada). Sem nenhum desses, a ligação falha com `DialError::NoAddress` e diz se um endereço guardado foi tentado.

- O primeiro salto é sempre o relay do chamador (por loopback) e o segundo, o do contato; o chamado vê o IP dele. Os relays de outros contatos não entram como primeiro salto; é um limite conhecido. Só a consulta da DHT pelo circuito (`dht_circuit`) usa relays de terceiros.
- **Relógio:** os convites não são conferidos contra o relógio de quem recebe, então hora errada numa máquina não impede a chamada. Só a DHT e as âncoras usam a hora, e toleram um período para cada lado.
- **Atrás de roteador:** sem `public_addr`, o nó pede ao roteador para encaminhar a porta do relay por UPnP (`src/upnp.rs`, renovado a cada 40 s) e usa o IP externo. Sem UPnP, ou com CGNAT, `Node::network_note` e a janela avisam o motivo; então abra a porta 4001 ou informe `public_addr` (um `ip:porta` ou um nome). Sem endereço alcançável o código direto leva o IP da rede local, que só serve dentro dela.
- **Não há furo de NAT:** quem não tem porta alcançável (CGNAT, roteador sem UPnP) não recebe chamadas.

### Interface gráfica

Guias passo a passo por sistema: [`docs/LINUX.md`](LINUX.md) e [`docs/WINDOWS.md`](WINDOWS.md).

```text
cargo run --release --features gui --example gui
```

0. A janela segue o estilo do Discord: barra lateral só de ícones, com uma aba por assunto e o nome ao passar o mouse (**Início**, **Contatos**, **Chamada**, **Vídeo**, **Áudio**, **Configurações**, **Atividade**) e, no pé dela, o seu avatar com o estado (o texto aparece ao passar o mouse), o botão de mudo e o de desligar. Os servidores não têm aba na barra: ficam só como ícones no trilho à esquerda dela, e clicar num abre a página do servidor. Em janela larga as páginas se dividem em duas colunas. A aba **Chamada** abre sozinha quando uma chamada 1:1 começa (entrar numa sala não muda de aba) e guarda o compartilhamento de tela e as telas que chegam; as preferências de transmissão (privacidade, fluidez ou nitidez, placa de vídeo) ficam na aba **Vídeo**.
1. Na primeira abertura a identidade já é criada sozinha (fica na aba *Configurações*, oculta). O **código de contato** (`chave#ip:porta#circuito#link`, que leva o seu relay; veja `docs/GUI.md`, que também conta como os contatos recebem o endereço novo quando o seu muda) aparece em *Meu código de contato*, na aba *Início*, com *Copiar*; passe-o a quem vai te chamar.
2. O programa já tenta ficar online ao abrir (a caixa *Tentar ficar online ao abrir o programa*, na aba Início, vem ligada); se você a desligou, clique **Ficar online**. O indicador mostra Offline / Conectando / Online.
3. **Contatos**: cada um tem *Ligar* e *Remover*; para adicionar, nome e código de contato. Quem não estiver na lista não recebe resposta; contatos são atendidos automaticamente. *Ligar para um código* aceita um código ou nome sem salvar. **Desligar** encerra.
4. Os campos são gravados em `~/.clam-gui.conf` (`%USERPROFILE%` no Windows), **inclusive a chave secreta**; no Unix o arquivo fica `0600`.
5. **Áudio real** (feature `audio`): `--features gui,audio` e marque *Usar microfone e alto-falante* na aba *Áudio* (com o nó offline); o microfone e o alto-falante se escolhem ali também. Usa `cpal`, 16 kHz mono (G.722 ou Opus, ambos a 16 kHz), filas de 400 ms. A troca de taxa do dispositivo usa filtro sinc com janela de Blackman (corte em 45% da menor taxa, avaliado na posição fracionária exata), em vez de média de caixa e interpolação linear. O microfone passa pelo `sonora` (WebRTC em Rust puro): filtro passa-altas, cancelamento de eco, supressão de ruído e ganho automático (cada estágio se liga e desliga com `Audio::set_processing(Processing)`, e `Audio::echo_stats()` diz quantos dB de eco saem; a referência do eco é o que saiu pelo alto-falante, contínua (com o silêncio), em pedaços inteiros de 10 ms entregues assim que chegam, sem zeros de preenchimento; `echo_stats().erle_db` é medido na energia do microfone antes e depois; a GUI desliga os que ela mesma faz, ver `docs/GUI.md`). Fones ainda são o mais seguro. Sem a opção, cai para tom (440 Hz quem liga, 660 Hz quem atende). No Linux a compilação exige `pkg-config` e `libasound2-dev`; com PulseAudio/PipeWire rodando o `cpal` usa o servidor (a lista de dispositivos mostra os nomes do sistema), senão o ALSA.
6. `eframe` fica atrás da feature `gui`, fixado em 0.32 (a 0.37 conflita com o libp2p por `js-sys`). O visual (paleta escura, avatar, botões grandes) está em `examples/gui/theme.rs`; as abas, em `examples/gui/ui.rs`.

O fluxo antigo, com relays manuais, continua em `examples/call.rs` e `examples/measure.rs`.
