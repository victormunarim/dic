# Como a comunicação funciona

Visão ponta a ponta, do clique em *Ligar* ao áudio e à tela do outro lado: quem fala com quem, em que formato, o que cada parte enxerga e por que escolhemos assim. Os outros documentos entram em cada peça com mais detalhe e testes; este amarra todas.

[← Voltar ao README](../README.md)

## Princípios

- **Sem servidor, sem diretório.** Cada pessoa roda um relay de si mesma (TCP, porta 4001 por padrão). Quem liga usa os relays das próprias pessoas envolvidas; o projeto não opera nada.
- **Identidade = uma chave X25519.** A chave secreta fica no seu computador (no chaveiro do sistema, se houver). A pública é o que você passa a um contato. Não existe conta, nome único nem cadastro.
- **Tudo de ponta a ponta.** Os relays no meio carregam bytes cifrados que não sabem ler. Voz, tela e avisos de endereço são cifrados com chaves que só o par tem.
- **Quem não é contato não recebe nem resposta.** O telefone ignora em silêncio o que não consegue autenticar, para não revelar que existe.
- **O fio parece o mesmo falando ou calado.** Células de tamanho fixo e cadência constante (cover traffic) escondem quando há voz ou tela.
- **Sem furo de NAT.** Quem recebe precisa de uma porta alcançável (UPnP, redirecionamento ou endereço público). Sem isso ninguém liga para essa pessoa (CGNAT, por exemplo).

## As camadas, de baixo para cima

```text
 voz / tela / avisos          pacote de 200 B, ChaCha20-Poly1305 (chaves do par)        src/media/
 ───────────────────────────────────────────────────────────────────────────────────────────────
 telefone (Phone)             quadros [u16 BE tamanho][dados] sobre um TCP local          src/phone.rs
 ───────────────────────────────────────────────────────────────────────────────────────────────
 circuito de 2 saltos         célula de 256 B, camadas AEAD por salto (handshake ntor)     src/circuit/
 ───────────────────────────────────────────────────────────────────────────────────────────────
 link entre dois relays       TCP + Noise NK, quadro de 279 B a cada 10 ms (cover)         src/link.rs, src/transport/
```

### 1. Link

Dois relays (ou o cliente e o relay) conversam por TCP com **Noise NK**: quem disca conhece a chave de **link** de quem atende (`link_key`) e a usa para autenticá-lo. Depois do handshake todo quadro tem **279 bytes**: célula real, ou uma célula `Padding` se não houver nada a mandar. O link emite exatamente um quadro a cada **10 ms** (`DEFAULT_COVER`), então quem olha o cabo não distingue conversa de silêncio. O relay só inicia padding nos links que ele mesmo abre e **responde** nos de entrada cujo par mandou padding (limite de 64 links com cover, `max_cover_links`).

Cada relay tem **duas chaves diferentes**: a de circuito (`onion_key`, usada na criação de circuitos) e a de link (`link_key`, usada no Noise). As duas vão juntas no código de contato e nos avisos de endereço.

### 2. Circuito

O circuito tem **2 saltos** e células de `CELL_LEN = 256` bytes, com **203 bytes** de dados por célula. Cada salto faz o seu handshake X25519 autenticado pela chave de circuito do relay (estilo ntor), com sigilo adiante. Cada camada tem contador explícito e janela anti-replay; o preenchimento é aleatório a cada salto, então o tamanho no fio não diz em que salto a célula está.

O desenho ao ligar para alguém:

```text
você ── loopback ──> seu relay ──(link Noise)──> relay do contato ──TCP──> telefone do contato
      (1º salto)                 (2º salto, "saída")                     destino contato:9
```

- O **primeiro salto é o seu próprio relay** (por loopback). Os relays de outros contatos não entram como primeiro salto; é um limite conhecido (veja "O que cada parte vê").
- O **segundo salto é o relay do contato**, que faz o papel de saída. A **política de saída** do relay só deixa passar destinos conhecidos: `<hex da chave>:9` leva ao telefone local daquela pessoa, `dht:9` ao gateway da DHT e `anchor:9` à âncora. O nome real do destino (a chave do contato) só aparece dentro da camada do segundo salto.
- **Cover de circuito:** o cliente manda **uma célula a cada 20 ms**, seja voz, ping ou `Drop`; o relay de saída responde uma a uma, com o que vem do contato ou com outro `Drop`. Silêncio e conversa têm a mesma taxa nos dois sentidos, em todos os saltos. Custo: ≈ 100 kbps por sentido.
- **Failover:** o gerente mantém um circuito **ativo** e um **reserva** (com o mesmo cover). O ativo recebe ping a cada 100 ms e é dado como morto após 350 ms sem sinal; a voz passa ao reserva em menos de 500 ms, sem novo convite. O telefone aceita o pacote em qualquer conexão e responde por aquela que o autenticou.
- Os limites do relay (`RelayConfig`: 256 circuitos, 800 kB/s por circuito, 2 MB/s no relay, 20 criações/s) protegem quem presta o serviço.

### 3. Telefone

O `Phone` escuta num TCP local; é o que a saída do relay alcança com `contato:9`. Cada circuito chega como uma conexão com quadros `[u16 BE tamanho][dados]`. Convites de contatos conhecidos viram `IncomingCall`; quadros de servidor (tag 8) de membros de um servidor em comum vão ao motor de servidores, e convites de chamada de quem está na mesma sala ([SERVIDORES.md](SERVIDORES.md)) também são atendidos; qualquer outra coisa é ignorada sem resposta.

### 4. Pacote de mídia (200 bytes)

Tudo o que passa pelo telefone é um pacote de **200 bytes**: `seq (4, autenticado) ‖ ChaCha20-Poly1305(tipo ‖ tamanho ‖ conteúdo ‖ zeros)`. Cabe numa célula de 203 bytes. Voz, silêncio, desligar, convite, aceite, aviso de endereço e apresentação têm o mesmo tamanho. Há janela anti-replay de 128 pacotes, e o `seq` só vale depois de autenticar.

## Fluxo de uma chamada

1. **Achar o relay do contato** (`Dialer::reachable_client`). Cada endereço é testado por 5 s até o circuito ficar pronto; um endereço velho não trava a ligação. Em ordem:
   1. o endereço **guardado** (o do último contato bem-sucedido ou do último aviso), a **dica** (o que o convite dele trouxe, ou o código de contato) e o **histórico** (os últimos 4 endereços);
   2. se nenhum responde, o registro que o contato deixou na **DHT** e nas **âncoras**, consultados ao mesmo tempo, por até 45 s, com o dia anterior, o atual e o seguinte. O de maior `seq` vence e vira o endereço guardado;
   3. sem nenhum endereço, `DialError::NoAddress`, que diz se algum foi tentado.
2. **Montar o circuito** até esse relay (acima) e esperar `Ready`.
3. **Convite.** O chamador gera uma chave efêmera e manda o convite: a identidade dele **cifrada** com `DH(efêmera, chave do destinatário)`, a efêmera, a dica do próprio relay (para o outro ligar de volta) e o **codec** escolhido, numa etiqueta de 12 bytes que só o destinatário confere. O relay de saída vê que passa um convite, mas não de quem. O telefone lembra as efêmeras dos últimos 1024 convites e não toca duas vezes pelo mesmo.
4. **Aceite.** O chamado responde com a sua efêmera. Dos dois lados sai o handshake de 3 DH estilo Noise `KK` (efêmera × efêmera e estática × efêmera nos dois sentidos) com HKDF: sigilo adiante por chamada, autenticação mútua pelas chaves dos contatos e uma chave por sentido. O primeiro pacote válido confirma.
5. **Voz.** A cada 20 ms o microfone vira um quadro do codec e um pacote de 200 bytes. Codecs: **G.722** (16 kHz, 64 kbps, o que vale se o convite não disser nada), **Opus** (padrão na GUI, 16 kHz, 32 kbps, desmarcável) e **G.711 µ-law**. Quem recebe passa pelo buffer de jitter (começa em 40 ms, adapta até 200 ms) e o PLC esconde a perda.
6. **Desligar** é um pacote autenticado, mandado 3 vezes (20 ms entre elas). Ninguém de fora derruba a chamada.
7. Se nada chega por 8 s a GUI dá a chamada como perdida; quem ligou refaz sozinho, até 5 vezes.

Depois de uma chamada completa, o relay usado passa a ser o endereço guardado do contato. A dica do convite, que não é autenticada, nunca sobrescreve o guardado antes disso.

## Transmissão de tela

A tela anda num **circuito só dela**, aberto por quem transmite até o telefone de quem assiste, com chaves derivadas das da chamada (`CallKeys::sub`, um par por sentido). Os pacotes são os mesmos de 200 bytes, com cabeçalho de 7 bytes (flags, id do quadro, índice, total) e **171 bytes de vídeo** por pacote.

| | Com privacidade (padrão) | Sem privacidade |
|---|---|---|
| Caminho | 2 saltos: o seu relay e o do contato | direto ao relay do contato, 1 salto |
| Cover | link e circuito no ritmo do slot | nenhum, só sai o que há para mandar |
| Slot (`phone::SCREEN_SLOT` / `OPEN_SCREEN_SLOT`) | 2 ms (500 pacotes/s, capacidade 684 kbps) | 0,5 ms (2000 pacotes/s, 2,7 Mbps, 512 kB/s no fio) |
| Orçamento do codec (90 %) | ≈ 615 kbps | ≈ 2,5 Mbps |
| Quadros por segundo (fluidez) | 15 | 30 |
| O que um observador vê | taxa constante, a tela parada ou mexendo | quando a tela se mexe e o seu IP falando com o relay do contato (o conteúdo segue cifrado) |

- **Fluidez ou nitidez** (`video::Priority`): a taxa de bits é uma só; fluidez a reparte em mais quadros por segundo, nitidez em metade dos quadros com o dobro de bits.
- **Perda:** quem assiste pede de volta o que faltou (`NACK`, no pacote de preenchimento) e o emissor reenvia os últimos 660 fragmentos que guardou, à frente da fila, em pacote novo. Sem FEC. Reparo que não vem em 800 ms abandona o quadro e pede um quadro-chave.
- **Taxa adaptativa:** a cada 0,5 s quem assiste devolve a fração perdida; o emissor aplica AIMD (`video::RateControl`).
- **Codec:** H.264 por openh264 com controle de QP próprio, ou, opcional e só no Linux, pela placa de vídeo (`src/hwenc.rs`, via `gst-launch-1.0`).
- **Captura:** X11, Wayland (portal `ScreenCast` + PipeWire) ou GDI no Windows.
- **Relays:** para o modo sem privacidade o relay precisa deixar passar ≈ 520 kB/s por circuito; o padrão (800 kB/s) deixa.

Detalhes e testes em [CHAMADAS.md](CHAMADAS.md#transmissão-de-tela-clammediascreen-clamphone).

## Como os contatos se reencontram

O endereço de um relay muda (IP novo, outra porta). Para o código antigo não virar lixo, o programa tem camadas que quem liga tenta em ordem, parando na primeira que completa o circuito. Veja [ENCONTRO.md](ENCONTRO.md) para o desenho e [GUI.md](GUI.md#como-os-contatos-se-acham) para o uso.

| # | Camada | Quando serve |
|---|---|---|
| 0 | Endereço guardado, dica e histórico | o IP não mudou ou mudou e ele avisou |
| 1 | **Aviso de endereço novo** (quadro de tag 4, confirmação tag 5), por fila de saída | só um lado mudou |
| 2 | **DHT** Mainline (BEP 44), um item por sentido, chave do dia | os dois mudaram |
| 3 | **Âncoras** (`anchor:9`), o mesmo item em depósitos que a pessoa escolhe | a DHT está bloqueada |

- Todas carregam o mesmo **registro de endereço** (endereço, chave de circuito e de link, `seq`), cifrado para o contato. Ninguém de fora o abre nem o forja.
- A **fila de saída** tenta com espera que dobra (30 s, 1 min, ... 2 h) e desiste em 3 dias ou quando o contato sai da lista. O nó confere o IP externo a cada 40 s (UPnP) e avisa sozinho quando muda.
- A DHT guarda o item por cerca de 2 h; o dono o renova a cada hora, então ele só dura enquanto o dono está online. O alvo muda por dia, e o leitor consulta os três dias de uma vez.
- Quem opera nós da DHT vê o IP de quem escreve e de quem lê. Com a opção "Falar com a DHT pelos relays dos contatos", a gravação e a leitura vão por um circuito de dois relays de contatos (`dht:9`) e a DHT só vê o IP da saída. Sem dois relays utilizáveis ela fica parada: **nunca cai para o acesso direto**.
- **Contato de duas vias.** Ao adicionar alguém, o programa manda uma **apresentação** (quadro de tag 7: nome, chave e relay, cifrados para o par) e insiste até a confirmação. Do outro lado, a pessoa passa a aparecer na lista sozinha. Quem não quer isso desliga "Aceitar quem me adiciona como contato".

## O que cada parte vê

| Quem | Vê | Não vê |
|---|---|---|
| Seu provedor / quem olha seu cabo | tráfego constante entre você e seu relay e o relay do contato | quando você fala, o conteúdo, com quem fala (o destino está dentro das camadas) |
| Seu relay (1º salto) | que há um circuito e a cadência dele | o destino, o conteúdo |
| Relay do contato (saída) | o seu IP, que há um convite, o destino `contato:9` | quem liga (a identidade vai cifrada), o conteúdo |
| O contato | seu IP (o 1º salto é o seu relay) e quem você é | — |
| Nó da DHT | IP de quem grava e de quem lê, um alvo que muda por dia | o conteúdo, a quem o item pertence |
| Âncora | o IP de quem a usa quando o 1º salto é o próprio usuário; quantos itens existem | o conteúdo, a quem pertencem |
| Quem vê a rede toda | pode correlacionar entrada e saída | — (**não** há promessa de anonimato contra adversário global) |

## Limites conhecidos

- O primeiro salto é o seu relay (menos na tela sem privacidade, que vai direto ao relay do contato): o contato vê o seu IP. O circuito esconde o conteúdo e o destino de quem observa, mas não o seu IP do contato.
- Sem CGNAT/porta alcançável não se recebe chamada. Não há furo de NAT.
- Quem controla o primeiro salto e a saída do mesmo circuito liga as pontas. Como os relays são os dos próprios contatos, isso exige que ambos sejam o mesmo adversário.
- A tela sem privacidade revela quando a imagem se mexe e vai direto ao relay do contato, sem relay no meio. Um contato com versão antiga não abre essa tela.
- A tela em hardware (`hwenc`) só foi testada com `x264enc` no lugar do codificador de GPU.

## Onde está cada coisa

| Assunto | Código | Documento |
|---|---|---|
| Circuito, relay, gerente | `src/circuit/` | [CIRCUITO.md](CIRCUITO.md) |
| Link, cover, política de saída | `src/link.rs`, `src/transport/`, `src/cover.rs` | [CIRCUITO.md](CIRCUITO.md#cover-traffic-padding-de-link) |
| Pacote, jitter, codecs, convite | `src/media/` | [CHAMADAS.md](CHAMADAS.md) |
| Telefone e discagem | `src/phone.rs`, `src/node.rs` | [CHAMADAS.md](CHAMADAS.md#telefone-chamar-e-atender-pelo-circuito-clamphone-feature-net) |
| Servidores e salas | `src/server/`, `examples/gui/engine/room.rs` | [SERVIDORES.md](SERVIDORES.md) |
| Tela | `src/media/screen.rs`, `src/video.rs`, `src/hwenc.rs`, `src/capture/`, `src/portal.rs` | [CHAMADAS.md](CHAMADAS.md#transmissão-de-tela-clammediascreen-clamphone) |
| DHT, âncoras, gateway | `src/rendezvous.rs`, `src/media/update.rs` | [ENCONTRO.md](ENCONTRO.md) |
| UPnP e endereço público | `src/upnp.rs`, `src/node.rs` | [GUI.md](GUI.md#como-os-contatos-se-acham) |
