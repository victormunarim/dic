# Opção "2 relays": nenhum lado sabe o IP do outro (desenho)

**Estado: etapas 1 e 2 implementadas; o resto é desenho.** A opção existe em Configurações (`two_relays`, desligada de início) e decide o caminho de **todo** fluxo (chamada, tela, bate-papo, arquivos, convites). Decisões já em código, que valem acima do texto abaixo quando divergem:

- **Qualquer contato pode ser relay.** Não existe marca de contato de confiança (`Dialer::set_trusted`, `PathRequest` e `PathError` foram removidos). O pool é `Dialer::known` sem o par, sem a saída e sem o seu relay (`Dialer::third_party_relays`).
- **Não existe tela "sem privacidade".** A tela usa o mesmo circuito dos outros fluxos, com cover.
- **Desligada = conexão direta, sem relay falso.** `ClientOptions::direct`: um salto ao relay do contato, sem o relay local em loopback que antes fazia as vezes de primeiro salto. O contato e quem observa a rede veem o seu IP, e a interface e `SEGURANCA.md` dizem isso.
- **Ligada, etapa 1: o primeiro salto é o relay de outro contato.** O destino vê o IP desse relay, não o seu. Sem relay de terceiro a ligação falha com `DialError::NoRelays`, nunca cai para o caminho direto. **O IP de quem recebe continua conhecido de quem liga**, porque a saída do circuito ainda é o relay do próprio contato.
- **Etapa 2: os serviços `intro:9` e `rv:9` existem em todo relay** (`src/hidden.rs`, [formato abaixo](#os-serviços-introduçãomd-formato-da-etapa-2)), mas **nada os usa ainda**: o cliente da introdução e do encontro é a etapa 3.
- **Ainda não feito** (camadas B e C, etapas 3 a 5 abaixo): introdução e encontro ponta a ponta, registro de presença v2, código de contato sem IP e convites com pontos de introdução. Nenhuma tela ou doc deve dizer que o IP fica escondido dos dois lados até elas estarem prontas e testadas.

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
3. **Introdução e encontro ponta a ponta.** O destinatário mantém os pontos de espera; quem liga encontra D por introdução e encontro. Testes de integração: a chamada completa sem que nenhum lado tenha o IP do outro em memória, em arquivo ou em log.
4. **Registro de presença versão 2 e código de contato sem endereço.** Tamanho fixo preservado. Compatibilidade com a opção desligada.
5. **Convites, apresentação, caixa de correio, servidores, bate-papo, arquivos e tela sem IP.** Cada fluxo que hoje usa `reachable_client` passa pelo mesmo ponto de decisão; sem queda silenciosa. Medir latência e banda reais e atualizar este doc e os orçamentos de [COMUNICACAO.md](COMUNICACAO.md) e [CIRCUITO.md](CIRCUITO.md).

## Perguntas em aberto

- **Quem serve de relay?** Decidido: qualquer contato.
- **O relay de quem usa a opção ligada.** Separar a chave do relay da de contato (proposto acima) exige que o `known`/`history` e os avisos de endereço passem a guardar duas identidades.
- **Pontos de introdução de longa duração.** Três circuitos contínuos custam ≈ 300 kbps. Dá para manter um só em ritmo reduzido, aceitando que o ritmo passa a identificar "este é um ponto de espera"?
- **Servidores grandes.** A malha da sala conecta todos com todos; com encontro, cada par precisa de um `M`. O custo de uma sala de muita gente precisa ser medido antes de a opção valer para salas.
