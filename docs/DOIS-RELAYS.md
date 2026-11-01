# Opção "2 relays": nenhum lado sabe o IP do outro (desenho)

**Estado: etapas 1 a 4 implementadas e a etapa 5 em andamento (voz, tela, bate-papo, oferta e pedaços de arquivo, sondagem de presença, apresentação, caixa de correio e código de contato sem endereço); o resto é desenho.** A opção existe em Configurações (`two_relays`, desligada de início) e decide o caminho de **todo** fluxo (chamada, tela, bate-papo, arquivos, convites). Decisões já em código, que valem acima do texto abaixo quando divergem:

- **Qualquer contato pode ser relay.** Não existe marca de contato de confiança (`Dialer::set_trusted`, `PathRequest` e `PathError` foram removidos). O pool é `Dialer::known` sem o par, sem a saída e sem o seu relay (`Dialer::third_party_relays`).
- **Não existe tela "sem privacidade".** A tela usa o mesmo circuito dos outros fluxos, com cover.
- **Desligada = conexão direta, sem relay falso.** `ClientOptions::direct`: um salto ao relay do contato, sem o relay local em loopback que antes fazia as vezes de primeiro salto. O contato e quem observa a rede veem o seu IP, e a interface e `SEGURANCA.md` dizem isso.
- **Ligada, etapa 1: o primeiro salto é o relay de outro contato.** O destino vê o IP desse relay, não o seu. Sem relay de terceiro a ligação falha com `DialError::NoRelays`, nunca cai para o caminho direto. **O IP de quem recebe continua conhecido de quem liga**, porque a saída do circuito ainda é o relay do próprio contato.
- **Etapa 2: os serviços `intro:9` e `rv:9` existem em todo relay** (`src/hidden.rs`, [formato abaixo](#os-serviços-introduçãomd-formato-da-etapa-2)), e a etapa 3 os usa.
- **Etapa 3: introdução e encontro ponta a ponta** (`src/meet.rs`, [formato abaixo](#introdução-e-encontro-formato-da-etapa-3)). `meet` (quem liga) e `HiddenService` (quem atende) já fazem a chamada completa por `intro:9` e `rv:9`, testada em loopback (`tests/meet.rs`). **Ainda não está ligada ao `Node` nem à interface**: os pontos de introdução de D são passados por quem chama, porque a presença v2 que os publica (etapa 4) ainda não é chamada pelo `Node`. A opção "2 relays" da GUI segue só com a etapa 1.
- **Etapa 4: registro de presença v2** (`presence::seal_v2`/`open_v2`, `service::publish_v2`/`lookup_v2`, [formato abaixo](#registro-de-presença-versão-2-formato-da-etapa-4)). Os pontos de introdução de D viajam no registro, que continua com 668 B; teste ponta a ponta em `tests/meet.rs`: C acha os pontos na presença e liga. O código de contato só com a chave já era aceito pelo parser (`split_direct`). **O aplicativo não publica nem busca presença v2**: o `Node`/`Dialer` não está ligado, e a GUI e o código de contato exibido não mudaram. Com a opção desligada nada muda.
- **Etapa 5, parte 1: chamada de voz sem IP no `Node`** (`src/node/blind.rs`). Com a opção ligada e uma DHT ou âncora: o nó sobe um `HiddenService` em relays de terceiros (pontos refeitos a cada 30 min), grava a presença v2 para cada contato (alvo e assinante derivados do segredo do par: `PresenceKeys::dht_target` e `owner_signing_key`; mesma DHT, âncoras e gateway do reencontro) e `Dialer::dial` chega ao contato **só** por introdução e encontro. Sem pontos dele a chamada falha com `DialError::NoHiddenPoints`; sem relays de terceiros, `NoRelays`; nunca se usa o relay guardado. Teste ponta a ponta (`tests/node.rs`): A liga a B sem conhecer o endereço dele e nenhum dos dois guarda o relay do outro. O teste achou um vazamento que ficou fechado: o convite da chamada levava o relay de quem liga, então com a opção ligada o convite não leva relay (`Reachability::hide_relay`). Ligada, o nó também **deixa de entregar e gravar o seu endereço** (fila de saída, DHT de endereços e servidores), senão os contatos o aprenderiam por aí.
- **Etapa 5, parte 2: tela, bate-papo, arquivos e sondagem por encontro.** `Dialer::reachable_client` (o ponto único que os fluxos usam) passa a decidir o caminho: com a opção ligada ele chama `hidden_client` e nunca olha o relay guardado; desligada, tudo como antes. Entram no mesmo ponto `dial`, `share_screen_keys`, `open_file_pipe`, `deliver_confirmed` (bate-papo, quadros de arquivo, apresentação, remoção), `deliver_update`, `deliver_batch` e `is_online`. O circuito de encontro é o que volta ao chamador, então ele leva as opções do fluxo (`meet_with`): a tela e o arquivo montam o encontro já com o cover no ritmo dos slots (`screen_client_options`); a conversa com o ponto de introdução usa sempre as opções padrão. Quem recebe liga o encontro ao telefone local como o relay de saída faria, então bate-papo, arquivo e tela chegam sem mudança no telefone. O relay que funcionou deixa de existir nesse caminho (`Option<RelayInfo>`): nada é guardado do contato. O teste achou **um erro da parte 1**: o alvo da presença na DHT vinha só do segredo do par, então os dois lados gravavam no mesmo item e um apagava o outro (a chamada só funcionava de um lado). Agora `PresenceKeys::dht_target(epoch, owner)` entra com quem publica e cada lado grava num alvo seu. Testes (`tests/node.rs`): voz, bate-papo e oferta de arquivo nos dois sentidos e tela de A para B, sem que ninguém aprenda o relay do outro; tela e arquivo sem pontos não usam o relay guardado.
- **Etapa 5, parte 3: apresentação, caixa de correio e código de contato sem endereço.** O cartão de apresentação (`Intro`) passou a ter `reach: Option<Reach>`: sem relay ele vai com tamanho de endereço 0 e `onion` e `link` zerados (o quadro continua com 200 B; `onion` ou `link` não zerados com endereço 0 é lixo e não abre). Com a opção ligada, `Introducer` manda sempre o cartão sem endereço, direto (se o contato já tem pontos publicados) ou pela caixa de correio, que só precisa da chave do destinatário; o `own` deixa de sair daqui. `Node::relay_info()` devolve `None`, então o código de contato que a interface mostra leva só a chave; ao receber um cartão sem endereço a GUI adiciona o contato só pela chave (`Introduced.reach` é `Option`). O `ServerHub` recebe um `own` vazio: nenhum endereço vai em quadro de servidor. Testes (`tests/node.rs`): cartão direto e por caixa de correio (B offline, volta depois) sem relay; `update.rs` e `conf.rs` cobrem o formato e a linha de contato; o alvo de fuzz `media_update` cobre o cartão sem endereço (60 s).
- **Etapa 5, parte 4: servidores sem IP.** Com a opção ligada o motor (`Servers::set_hidden`, chamado em `Node::start`) cria e convida sem relay: `Hint.reach` e `Msg::Join.reach` viraram `Option<Reach>`, codificados com tamanho de endereço 0 e `onion`/`link` zerados (convite e `Join` com endereço 0 e bytes não zerados são lixo), o convite não leva dicas de outros membros, o `Join` sai sem esperar o relay, `set_reach` é ignorado e a busca do membro pela DHT de endereços (`find_member`) não roda. O `Join` e os quadros chegam pelo mesmo `reachable_client`, ou seja, por introdução e encontro. O estado salvo aceita `joining` sem endereço (`chave:::`). Teste (`tests/server.rs`): dois nós sem nenhum endereço formam o servidor e nenhum `Event::Address` circula; o alvo de fuzz `invite` cobre o convite sem endereço (60 s). **Limite:** só se alcançam membros que são contatos entre si, porque a presença v2 existe só para contatos (segredo do par); um membro que não é contato de outro fica sem falar com ele enquanto a opção estiver ligada.
- **Etapa 5, parte 6: a interface passa a dizer que o IP fica escondido.** Todo fluxo (voz, tela, bate-papo, arquivos, apresentação, caixa de correio, servidores) já corre sem o relay de quem usa a opção, então o hover e o "Como funciona" de Configurações dizem: ligada nos dois lados, nenhum aprende o IP do outro; contato com a opção desligada não é alcançado. Limites na garantia de [SEGURANCA.md](SEGURANCA.md).
- **Etapa 5, parte 5: medição do encontro em loopback.** O teste `measure_the_meeting_setup_and_voice_latency` (`tests/meet.rs`, `#[ignore]`, imprime) mede montagem (≈ 230 ms o encontro, ≈ 330 ms até a chamada), primeiro quadro (≈ 100 ms), perda (nenhuma em 150) e jitter (p95 ≈ 1 ms); a tabela, a estimativa de rede real e o custo de banda (dois circuitos: o dobro no total, o mesmo por link) estão em [CIRCUITO.md](CIRCUITO.md#custo-do-encontro-às-cegas-2-relays) e [COMUNICACAO.md](COMUNICACAO.md). A medição entre máquinas distintas continua pendente (o `measure` não tem modo de encontro).
- **Ainda não feito** (etapa 5, o resto): a fila de saída, a DHT de endereços e os publicadores seguem desligados com a opção ligada; bate-papo não tem caixa de correio, então um contato offline não o recebe; **relay antigo** descarta cartão, convite e `Join` sem endereço (para ele `addr_len == 0` é inválido), então quem usa a opção não consegue se apresentar a quem ainda não atualizou; latência e banda do encontro só foram medidas em loopback.

[← Voltar ao README](../README.md) · veja também [COMUNICACAO.md](COMUNICACAO.md), [CIRCUITO.md](CIRCUITO.md), [ENCONTRO.md](ENCONTRO.md) e [SEGURANCA.md](SEGURANCA.md).

## O pedido

Uma opção **2 relays** em Configurações, **desligada** por padrão:

- **Desligada:** o comportamento de hoje. O conteúdo é cifrado de ponta a ponta e o caminho é de 2 saltos com cover.
- **Ligada:** a chamada, a tela, o bate-papo, os arquivos e os convites passam por **2 relays reais**, de terceiros, e **quem liga nunca fica sabendo o IP de quem atende, nem o contrário**, inclusive nos convites.

## O que a opção desligada cobre de verdade

O pedido diz que, desligada, o IP fica "coberto para agentes externos". Isso é verdade só em parte, e a interface não deve prometer mais do que isto:

- Cobre quem **não está no caminho** (nó da DHT, âncora, outro contato, quem só lê a rede de longe): o conteúdo vai cifrado e o ritmo é constante.
- **Não** cobre quem está no caminho. O 1º salto é o seu relay por loopback e o 2º é o relay do contato, então o provedor, o dono do Wi-Fi e os roteadores do trajeto veem os cabeçalhos IP: o seu IP falando com o IP do relay do contato.
- Como cada pessoa é o relay de si mesma, o IP do relay do contato é o IP do contato. A chave do contato dentro das camadas, portanto, esconde pouco de quem vê o fio: o destino se deduz do IP.
- O contato vê o seu IP (o 1º salto é o seu relay). Está em "Limites conhecidos" de [COMUNICACAO.md](COMUNICACAO.md).

## Onde o IP aparece hoje

| Lugar | Quem vê o IP | Observação |
|---|---|---|
| Código de contato direto | quem recebe o código | leva a chave e o relay (`RelayInfo`: `addr`, `onion`, `link`) |
| Registro de presença/endereço (`AddressRecord`, `src/media/update.rs`) | o contato que o abre; o nó da DHT vê o IP de quem grava e de quem lê | `addr` de até 60 B, cifrado para o par |
| Apresentação (tag 7) e aviso de endereço novo | o contato | cifrados para o par |
| Convite de servidor (`Reach`, `src/server/invite.rs`) | o convidado | cifrado para ele; guardiões da caixa veem só as chaves |
| Convite de chamada | quem atende | relay de quem liga, cifrado junto da identidade |
| Dicas e `known`/`history` (`~/.clam-gui.known`) | quem tem o arquivo | uma linha por endereço de relay de contato |
| Chamada, tela, arquivos | o contato e o relay dele | o 1º salto (você) liga ao relay do contato |

A ligação direta entre "contato" e "IP" é do desenho atual: o destino de saída `<8 B da chave em hex>:9` (`dest_for`, `src/node/mod.rs`) só existe no relay da própria pessoa.

Também hoje não há relay de terceiro no caminho: em `Dialer::client_with` a lista de candidatos de 1º salto passa vazia e o único 1º salto é o seu relay (`127.0.0.1:<porta>`, o "último recurso" do código). O gerente de circuitos já sabe escolher entre vários relays (`CircuitManager`), mas ninguém o alimenta com relays de terceiros.

## Metas e não-metas

Metas, com a opção ligada:

1. O destinatário não aprende o IP de quem liga, e quem liga não aprende o do destinatário. Vale para chamada, tela, bate-papo, arquivo, presença e convite.
2. Nenhum relay sozinho liga as duas pontas.
3. Sem queda silenciosa: se não houver relays suficientes, a chamada **falha com um motivo claro**. Nunca cai para o seu relay, para a conexão direta nem para a DHT em claro (mesmo princípio de `dht_circuit`).
4. O conteúdo continua cifrado de ponta a ponta com as chaves dos contatos, que nenhum relay vê.

Não-metas (limites que a opção **não** remove):

- Adversário que vê a rede toda e correlaciona entrada e saída. Continua sem promessa de anonimato global.
- Uma única pessoa que controle os dois relays do mesmo circuito (ou o relay de encontro e os dois de entrada).
- Esconder a **chave** do contato de quem já a tem. A opção esconde o endereço de rede, não a identidade criptográfica.
- Esconder o seu IP do **1º relay** do seu próprio circuito: ele precisa dele para responder.

## Desenho

O desenho em três camadas, da mais simples à mais cara. Cada uma é útil sozinha, e só a terceira cumpre a promessa de "nenhum lado sabe o IP do outro".

### Camada A: dois relays de terceiros no caminho

Com a opção ligada, o circuito deixa de usar o seu relay como 1º salto. Ele passa a ter dois relays reais, escolhidos entre os relays de **outros contatos** (nunca o seu, nem o do par com quem se fala):

```text
você ──> relay R1 (contato X) ──> relay R2 (contato Y) ──> destino
```

- R1 vê o seu IP, não o destino. R2 vê o destino (e o IP de R1), não o seu IP.
- **Quem pode ser relay de terceiro:** qualquer contato (decidido: sem marca de confiança). Quem serve passa a ser conhecido pelo IP de quem o usa; por isso o pool é de contatos, não aberto.
- **Seleção:** o gerente sorteia entre os de maior pontuação, em grupos diferentes (`intro.rs` e `reputation.rs` já têm a política de diversidade e de saúde), sem repetir o par, sem usar o próprio nó. Se faltarem relays, devolve erro: `PathMode` ganha um modo novo (nome provisório `Hidden`), e `PathRequest::resolve` não o rebaixa nunca.
- **O que A resolve:** o destinatário deixa de ver o IP de quem liga (ele vê o de R2). **O que A não resolve:** quem liga ainda conhece o IP do destinatário, porque o `Extend` do circuito precisa do endereço do relay do destino.

### Camada B: pontos de introdução e de encontro

Para quem liga não aprender o IP do destinatário, o destinatário não pode ter um endereço publicado. Segue o molde dos serviços ocultos, reaproveitando o que o relay já faz para a DHT e a âncora: **destinos de saída** (`dht:9`, `anchor:9`) atendidos por um serviço local do relay.

Dois destinos novos, `intro:9` e `rv:9`. Pedido e resposta no mesmo formato do gateway (`u16 BE tamanho ‖ corpo`, em pedaços por célula), para manter a célula de 256 B e o ritmo de 20 ms.

1. **Pontos de introdução.** O destinatário D mantém, por um circuito de 2 relays (camada A), uma conexão de espera com 3 relays `I1..I3`. Em cada um manda `ESTABLISH(chave pública, prova de posse)`. O `I` guarda "a chave de D se alcança por este circuito" e **não** sabe o IP de D (vê R2 de D).
2. **Registro de presença.** No lugar do `addr`, o registro cifrado para o par (e o item da DHT) leva os pontos de introdução `I1..I3` (endereço e chaves de cada um). O registro ganha uma versão nova; o tamanho fixo continua, porque ele é a regra contra análise de tamanho.
3. **Introdução.** Quem liga (C) escolhe um relay de encontro `M`, cria `ESTABLISH_RV(cookie)` em `M` por um circuito de 2 relays, e manda a `I` por outro circuito de 2 relays: `INTRODUCE(chave de D, M, cookie, quadro cifrado só para D)`. O quadro cifrado carrega a prova de autoria do convite (já existe) e o `cookie`. `I` repassa a D pela conexão de espera; não lê o quadro.
4. **Encontro.** D abre um circuito de 2 relays até `M` e manda `JOIN(cookie)`. `M` liga os dois fluxos e copia os quadros de um para o outro. C e D falam a chamada normal por cima (as mesmas chaves de ponta a ponta, o mesmo telefone), sem que nenhum veja o IP do outro.
5. **Cover.** Cada circuito mantém o ritmo constante de hoje (uma célula a cada 20 ms nos dois sentidos). `M` só repassa o que chega.

Cada lado usa os seus 2 relays, e os dois circuitos se encontram em `M`. Nenhum lado fala direto com o outro.

#### Os serviços `intro:9` e `rv:9` (formato da etapa 2)

Implementados em `src/hidden.rs`. Todo relay liga os dois em loopback ao subir (`Node::start`), independente da opção "2 relays" de quem o opera: sem isso, quem liga a opção não acharia pontos na rede. A política de saída continua uma lista fechada (`allow_list`); `intro:9` e `rv:9` só chegam a esses dois serviços, nunca a um endereço qualquer.

Enquadramento igual ao do gateway: o relay de saída põe `[u16 tamanho][pedaço de até 200 B]` no TCP local, e a mensagem é `u16 tamanho ‖ corpo`, com corpo de 1 a 400 B. Pedaço ou mensagem fora do limite fecha a conexão.

**`intro:9`** (cada conexão é um circuito):

| Mensagem | Corpo | Resposta |
|---|---|---|
| `Establish` | `0x10 ‖ auth_key[32]` | `Challenge`: `0x80 ‖ nonce[32]` |
| `Prove` | `0x11 ‖ assinatura[64]` | `Established` (`0x81`) ou `Refused` (`0x8f`) |
| `Introduce` | `0x12 ‖ auth_key[32] ‖ u16 tamanho ‖ blob` | `Delivered` (`0x82`) ou `Refused` |
| aviso a D | `0x90 ‖ u16 tamanho ‖ blob` | (empurrado no circuito de espera) |

- A `auth_key` é uma chave ed25519 **de serviço**, nova por período e sem relação com a chave de contato nem com o relay. D prova que a tem assinando `"clam-intro-establish-v1" ‖ nonce ‖ auth_key` (`verify_strict`). O desafio é novo por pedido e vale uma vez: errar a prova obriga a recomeçar; a assinatura não vale em outro circuito nem em outro ponto.
- Quem prova a posse substitui o registro anterior da mesma chave (D refez o circuito de espera). Quando o circuito de espera cai, o registro sai, mas só se ainda for o dele.
- O `blob` (1 a 365 B) é opaco: cifrado só para D, o ponto não o lê e o entrega intacto. O ponto **não** verifica contato; quem decide se uma introdução vale é D, que descarta a de quem não é contato.
- Limites: 256 serviços por ponto, 20 introduções por minuto por `auth_key`, 600 pedidos por minuto no ponto, 8 avisos na fila de um circuito de espera lento (cheia, a introdução é recusada, não segura quem introduz).

**`rv:9`** (uma conexão faz um único pedido):

| Mensagem | Corpo | Resposta |
|---|---|---|
| `Establish` | `0x20 ‖ cookie[32]` | `Waiting` (`0xa0`) ou `Refused` (`0xaf`) |
| `Join` | `0x21 ‖ cookie[32]` | `Joined` (`0xa1`) ou `Refused` |

- C guarda o `cookie` e espera. Quando D chega com o mesmo `cookie`, o ponto responde `Joined` a D e a C e passa a copiar os bytes de um circuito ao outro (`copy_bidirectional`), sem ler nada.
- O `cookie` é de **uso único** (sai da tabela no `Join`), vence em 60 s e um `Establish` repetido não toma o lugar do primeiro. Capacidade: 256 `cookie`s; 600 pedidos por minuto.
- Depois do `Joined`, o que passa é o tráfego cifrado de ponta a ponta de C e D; o ponto de encontro não tem as chaves.

Alvos de fuzz: `hidden_frames` (os quatro parsers e o acumulador de pedaços) e `hidden_point` (sequências de pedidos contra as duas tabelas, tempo variável). Testes: sans-IO e loopback em `src/hidden.rs`; por circuito real de 2 saltos em `tests/hidden.rs`.

#### Introdução e encontro (formato da etapa 3)

```text
  D ══ circuito de espera ══> I        Establish(auth_key) + Prove: D registra uma chave de serviço efêmera por ponto
  C ══ circuito ══> M                  Establish(cookie): C espera em M (relay de encontro que ele escolhe)
  C ══ circuito ══> I                  Introduce(auth_key, blob): I responde Delivered a C e Push(blob) a D
  D ══ circuito ══> M                  Join(cookie): M responde Joined aos dois e liga os circuitos
  C <════ chamada normal (convite, aceite, mídia) cifrada de ponta a ponta ════> D
```

- **Blob** (`meet::seal_meeting`, 274 B fixos, qualquer que seja o endereço de `M`): chave efêmera X25519 de C → chave de contato de D, ChaCha20-Poly1305 com HKDF. Dentro: chave de contato de C, carimbo (ms), `cookie`, relay de encontro (chaves e endereço de até 60 B, preenchido) e uma **prova de autoria** (HKDF do segredo estático C↔D sobre efêmera, `cookie`, carimbo e destino, comparada com `subtle`). `I` e `M` não leem o blob e não aprendem quem liga; quem não tem a chave privada de C não forja uma introdução em nome dele.
- **Quem decide é D.** Descarta, sem resposta, o blob que não abre, de carimbo a mais de 120 s do relógio, de autor fora da lista de contatos, de `cookie` já visto (lembra os últimos 256) ou quando já há 4 encontros em andamento. Quem ligou só percebe `MeetError::NoMeeting` depois de 30 s e não sabe o motivo.
- **Primeiro salto sempre de terceiro.** Todo circuito de `meet` e de `HiddenService` usa o pool de terceiros (`MeetError::NoPool` sem ele); não existe queda para o caminho que mostra o IP.
- **Circuito de espera.** Cai junto com a conexão ao ponto, e o registro morre com ela: `HiddenService` refaz o registro do zero (mesma `auth_key`, provada de novo), com espera de 1 s dobrando até 30 s.
- **Bridge no lado de D.** O circuito do encontro é ligado ao `Phone::local_addr()` com um quadro `[u16 tamanho][bytes]` por pedaço, como o relay de saída faz; o código do telefone não muda.

Alvo de fuzz: `meet_blob` (`open_meeting` com bytes arbitrários e com blob válido adulterado). Testes: unitários em `src/meet.rs`; chamada completa, estranho descartado, duas introduções seguidas e falta de relay de terceiro em `tests/meet.rs`.

#### Registro de presença versão 2 (formato da etapa 4)

O envelope é o da versão 1 (668 B: nonce, texto claro de 576 B cifrado com o endereço da época como dado associado, tag e assinatura ed25519), então o tamanho fixo e as garantias de endereço e assinatura valem igual. Muda o texto claro: o primeiro byte é a marca `0xA2` (maior que o máximo de 8 pontos da v1, então um leitor v1 recusa o registro como `Malformed`), o segundo é a quantidade de pontos (até 3) e cada ponto ocupa 157 B: tamanho do endereço (1 a 60), endereço do relay preenchido com zeros até 60, chave do circuito, chave do link e `auth_key` (32 B cada). A decodificação é estrita: preenchimento diferente de zero, endereço que não é UTF-8 ou tamanho fora de 1 a 60 são recusados. Nenhum campo é o IP de quem publica; o endereço é o do relay do ponto de introdução.

Compatibilidade: `open` (v1) não abre registro v2 e `open_v2` não abre v1; a busca de cada versão devolve `None` para a outra. Um contato com cliente antigo, portanto, não acha os pontos de D, e quem usa a opção desligada segue pelo caminho direto de sempre. Alvo de fuzz: `presence_v2` (decodificação crua e reencodificação idêntica).

### Camada C: endereçamento sem IP

Sem esta camada, o IP volta pelo texto. Ela toca todo lugar da tabela "Onde o IP aparece hoje":

- **Código de contato:** só a chave (e, opcional, os pontos de introdução). Sem `addr`. Quem adiciona alguém não aprende o IP.
- **Presença:** o registro versão 2 (camada B, passo 2).
- **Apresentação, aviso de endereço novo, `Reach` do convite de servidor, dicas e `known`:** passam a carregar pontos de introdução em vez de `addr`. O convite de chamada leva, no lugar do relay de quem liga, os seus pontos de introdução.
- **Convites na caixa de correio e na custódia:** o item da DHT segue igual; só muda o conteúdo cifrado.
- **O relay de cada pessoa:** quem usa a opção ligada deve ter um relay **separado do endereço dela**. Um relay que serve a terceiros (camada A) revela o IP de quem o opera aos contatos que o usam; para que isso não vire "o IP da pessoa X", o relay anunciado não pode carregar a chave de contato nem o destino `<chave>:9`.
- **Bônus:** quem recebe só abre conexões de saída (para os pontos de introdução e para `M`). Não precisa de porta aberta nem de UPnP para ser alcançado; hoje a falta de porta alcançável impede receber chamadas.

## Fluxo de uma chamada, com a opção ligada

1. C lê o registro de D (item da DHT ou aviso direto) e obtém `I1..I3`. Nenhum IP de D.
2. C monta 2 circuitos de 2 relays: um até `M` e outro até `I1`.
3. C manda `ESTABLISH_RV(cookie)` a `M` e `INTRODUCE(...)` a `I1`.
4. `I1` repassa a D, que confere a prova de autoria e que C é contato (como hoje: chave fora da lista não recebe resposta).
5. D monta um circuito de 2 relays até `M` e manda `JOIN(cookie)`.
6. `M` junta os dois fluxos. A chamada (convite, resposta, mídia) corre por cima, com as chaves de ponta a ponta.
7. O failover do gerente de circuitos vale para cada lado de forma independente: um circuito reserva por lado, os mesmos 350 ms.

## O que cada parte vê, com a opção ligada

| Quem | Vê | Não vê |
|---|---|---|
| Quem liga (C) | a chave de D, que há uma chamada | o IP de D, os relays de D |
| Quem atende (D) | a chave de C, que há uma chamada | o IP de C, os relays de C |
| R1 de C | o IP de C, que há um circuito | o destino, o conteúdo |
| R2 de C, `M` | o IP de R1, o `cookie` (em `M`) | o IP de C ou de D, o conteúdo |
| `I` (ponto de introdução) | a chave de D e que alguém pediu para falar com ela | o IP de D, o IP de C, o conteúdo |
| Provedor de C ou de D | tráfego constante entre a pessoa e o R1 dela | com quem fala, quando fala |
| Nó da DHT | o IP de quem grava e de quem lê o item (via circuito, só o IP da saída) | os pontos de introdução, o IP |
| Quem vê a rede toda | pode correlacionar entrada e saída | — (**sem** promessa de anonimato global) |

## Custos

- **Latência:** o caminho passa de 2 para cerca de 5 relays efetivos (R1 e R2 de C, `M`, R2 e R1 de D). Soma-se a introdução: vários segundos para estabelecer a chamada. O alvo de voz (200 ms em 2 saltos) tende a ficar acima, e deve ser medido com `examples/measure` antes de ser prometido.
- **Banda:** cada circuito consome ≈ 100 kbps por sentido (célula de 256 B a cada 20 ms). Uma chamada tem 2 circuitos, mais os 3 de espera de D, ≈ 300 kbps contínuos enquanto D está online.
- **Relays de terceiros:** quem serve gasta banda e abre-se a abuso. Os limites de `RelayConfig` (256 circuitos, 800 kB/s por circuito, 2 MB/s no relay, 20 criações/s) valem, e o destino de saída é restrito a `dht:9`, `anchor:9`, `intro:9` e `rv:9`: nunca vira proxy aberto.
- **Disponibilidade:** com poucos contatos de confiança online, não há relays suficientes. O erro é explícito.

## Ameaças e limites

- **Mesmo adversário em dois pontos.** Quem controla R1 e R2 do mesmo circuito liga as pontas; quem controla `M` e os dois R2 liga C a D. Como o pool vem de contatos de confiança, isso exige que a mesma pessoa controle vários deles. Documentado como limite.
- **Correlação de tempo.** O cover constante reduz a correlação por volume, mas não impede um adversário que vê as duas pontas.
- **Ponto de introdução hostil.** Pode negar o serviço (não repassa) ou contar quantas vezes D é procurado. Por isso são 3, em grupos diferentes.
- **Relay de encontro hostil.** Vê o `cookie` e os dois R2. Um `cookie` repetido por quem o observa deve ser recusado (uso único, com prazo curto).
- **DoS de introdução.** `I` limita pedidos por D e por origem, como o gateway da DHT, que já tem limite de pedidos por minuto.
- **Convites de servidor.** Quem convida e quem é convidado não aprendem o IP um do outro; o `Join` ao servidor passa a ir por introdução + encontro, não mais direto ao relay de quem convidou.
- **Relay antigo e contato antigo.** Um relay de versão anterior não entende `intro:9` nem `rv:9` e recusa o destino; o contato com versão antiga não abre o registro versão 2. Com a opção desligada nada muda; com ela ligada, esses contatos ficam inalcançáveis e o app avisa em vez de cair para outro caminho.
- **Tela sem privacidade.** Removida: a tela segue a opção como os outros fluxos.

## Interface proposta

- Em Configurações, uma caixa **2 relays**, desligada, com a dica (hover) explicando em uma frase o efeito e o custo, e o aviso de que exige contatos de confiança online.
- Ligar a opção mostra o que muda: latência maior, tempo de estabelecer a chamada, e que contatos com versão antiga não serão alcançados.
- Enquanto a opção está ligada, a interface mostra um estado discreto (ícone) quando há relays suficientes, e o motivo exato quando não há. A opção **nunca** muda o caminho sozinha.
- O código de contato gerado com a opção ligada não leva endereço; o texto avisa que o contato só se alcança por introdução.

## Plano de etapas

Cada etapa é um commit com testes e docs; a frase "nenhum lado sabe o IP do outro" só entra na interface depois da etapa 5.

1. **Relays de terceiros no caminho (camada A).** `PathMode::Hidden`, pool de contatos de confiança, seleção sem o próprio nó e sem o par, falha sem queda silenciosa. Testes: gerente com relays simulados (sans-IO); integração em loopback com 4 nós (você, 2 terceiros, destino) verificando que o destino não vê o IP de quem liga. A opção em Configurações nasce aqui, mas a dica diz só o que esta etapa cumpre.
2. **Destinos `intro:9` e `rv:9` no relay** (formato, limites, cookie de uso único). **Feito** (`src/hidden.rs`, alvos `hidden_frames` e `hidden_point`, testes sans-IO, de loopback e por circuito real). A exigência de contato não fica no ponto: ele não conhece a lista de contatos, então D descarta introdução de quem não é contato (etapa 3).
3. **Introdução e encontro ponta a ponta.** O destinatário mantém os pontos de espera; quem liga encontra D por introdução e encontro. **Feito** na biblioteca (`src/meet.rs`, `tests/meet.rs`, alvo `meet_blob`): chamada completa em loopback. Limites: em loopback todos são 127.0.0.1, então "nenhum lado tem o IP do outro" é provado só pela estrutura (C só recebe `IntroTarget` e a chave de D; D só recebe o blob; todo primeiro salto é de terceiro), não por medição em rede real; falta ligar ao `Node`/`Dialer` e publicar os pontos, o que depende da etapa 4.
4. **Registro de presença versão 2 e código de contato sem endereço.** Tamanho fixo preservado. Compatibilidade com a opção desligada. **Feito** na biblioteca (`src/presence.rs`, `src/service.rs`, `tests/meet.rs`, alvo `presence_v2`). Limites: o `Node`/`Dialer` não publica nem busca o registro, então hoje quem liga a um contato com código só de chave ainda não acha os pontos; a GUI não mostra nem gera esse código; ligar tudo é a etapa 5, que também precisa decidir como a presença v2 é guardada (a `NetStore`/DHT libp2p ainda não é usada pelo aplicativo).
5. **Convites, apresentação, caixa de correio, servidores, bate-papo, arquivos e tela sem IP.** Cada fluxo que hoje usa `reachable_client` passa pelo mesmo ponto de decisão; sem queda silenciosa. Medir latência e banda reais e atualizar este doc e os orçamentos de [COMUNICACAO.md](COMUNICACAO.md) e [CIRCUITO.md](CIRCUITO.md). **Partes 1 a 5 feitas**: voz, tela, bate-papo, arquivos, sondagem, apresentação, caixa de correio e código de contato (veja o estado no topo); servidores (parte 4) também. Limites: contato com a opção desligada ou com cliente antigo não acha este nó pelo endereço (fica inalcançável, como previsto em "Ameaças e limites"), e quem liga precisa de pelo menos 2 relays de outros contatos guardados; latência e banda só medidas em loopback.

## Perguntas em aberto

- **Quem serve de relay?** Decidido: qualquer contato.
- **O relay de quem usa a opção ligada.** Separar a chave do relay da de contato (proposto acima) exige que o `known`/`history` e os avisos de endereço passem a guardar duas identidades.
- **Pontos de introdução de longa duração.** Três circuitos contínuos custam ≈ 300 kbps. Dá para manter um só em ritmo reduzido, aceitando que o ritmo passa a identificar "este é um ponto de espera"?
- **Servidores grandes.** A malha da sala conecta todos com todos; com encontro, cada par precisa de um `M`. O custo de uma sala de muita gente precisa ser medido antes de a opção valer para salas.
