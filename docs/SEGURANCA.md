# Segurança, limites e estado em disco

Garantias e limites, limitações conhecidas e chaveiro.

[← Voltar ao README](../README.md)

## Garantias e limites de segurança

- Contra contatos e observadores comuns: ninguém fora do par descobre que você está online, nem liga seus endereços entre períodos.
- **Não** promete anonimato diante de quem monitora a rede inteira (adversário global). Diga isso aos usuários.
- O modo direto expõe o seu IP ao contato; o código exige confiança e aviso confirmado, mas o aviso na interface é responsabilidade do app.
- O relógio é passado como argumento (`agora_unix`); use um relógio confiável. Defasagem maior que uma época faz a busca falhar.
- Não há proteção contra replay de um registro antigo dentro da janela de três épocas. (No aplicativo o registro de endereço leva um `seq` que só cresce, e o contato descarta o que não for mais novo.)
- Servidores: o relógio `lamport` das operações não pode saltar (uma operação que passa de um acima do maior já visto é ignorada até as que faltam chegarem), então um membro não congela o servidor assinando um valor enorme. Limite que sobra: quem foi removido pode ter operações já assinadas com `lamport` antigo que valem na ordem do registro, por serem indistinguíveis de operações feitas ao mesmo tempo que a remoção. Quem já soube da remoção descarta o que o removido manda; a janela é a da propagação da remoção, ou um membro cúmplice que repasse as operações. O dono não pode ser removido por elas. Um membro também pode encher o registro até `MAX_OPS`.
- Relay: o `Extend` de um estranho só alcança endereços públicos (`is_public`: nada de loopback, rede privada, link-local ou reservado). O nome é resolvido pelo relay e a conexão vai ao IP já checado, o que evita *DNS rebinding*. Só o link local (loopback, do próprio programa) disca endereço privado, para o primeiro salto do seu circuito. Assim ninguém usa o relay de outra pessoa para varrer a rede interna dela.
- Vídeo: o decodificador recusa imagem maior que `MAX_DECODED_PIXELS` (1920x2160), pois as dimensões vêm do quadro-chave de quem transmite e decidiriam quanta memória alocamos. O openh264 ainda aloca os próprios buffers internos pelo tamanho que o quadro-chave declara; esse teto é o dele.
- A tela sem a opção de privacidade deixa de ter cover: quem observa a rede vê quando a imagem se mexe. O conteúdo segue cifrado de ponta a ponta.

## Limitações

Ainda **não** existem: TLS/QUIC e travessia de NAT (não há furo de NAT: quem não tem porta alcançável não recebe chamadas), chamadas em grupo com cifra de grupo (MLS; as salas de voz são uma malha de chamadas 1:1, [SERVIDORES.md](SERVIDORES.md)) e relays que não sejam os dos próprios contatos (o aplicativo não usa a descoberta por GossipSub nem a reputação; elas são camada de biblioteca).
A DHT é pública: um nó armazenador pode sobrescrever um registro com lixo (`open` rejeita lixo, mas ele esconde o registro bom até a próxima publicação; o dono republica a cada hora), e quem opera nós vê o IP de quem lê e de quem grava, a menos que a pessoa use a opção de falar com a DHT pelos relays dos contatos.
`PathMode` só modela e valida o modo; a chamada do aplicativo é sempre o circuito de 2 saltos (o primeiro salto é o relay de quem liga, então o contato vê o IP dele).

## Estado em disco (GUI)

Tudo fica na pasta pessoal (`~` no Unix, `%USERPROFILE%` no Windows), gravado por arquivo temporário e renomeado, e no Unix já com modo `0600`.

| Arquivo | Conteúdo |
|---|---|
| `.clam-gui.conf` | Contatos e configurações. A chave secreta fica aqui em hex só se não houver chaveiro (`secret=@keyring` quando há). Opções: `no_dht`, `dht_circuit`, `no_anchor`, `anchors`, `intros`, `name`, `public_addr`, `no_screen_privacy`, `screen_sharp`, `screen_hardware`, `no_opus`, `no_auto_online`, e as do áudio |
| `.clam-gui.known` | Uma linha por endereço de relay de contato: `chave addr onion link`, com `dica` (veio do convite, sem confirmação) ou `antigo` (histórico) no fim |
| `.clam-gui.relay` | As duas chaves secretas do relay (`onion` e `link`), para o endereço que os contatos guardaram continuar valendo depois de reabrir |
| `.clam-gui.intro` | Contatos a quem a apresentação ainda não foi confirmada (uma chave por linha) |
| `.clam-gui.servers` | Os servidores de que você faz parte: **o segredo de cada um**, o registro de operações e os membros. Quem lê o arquivo entra nos servidores como você; ele fica em modo `0600` e não vai para o chaveiro ([SERVIDORES.md](SERVIDORES.md)) |

Quem lê estes arquivos conhece com quem você fala e o seu relay; trate-os como dados pessoais.

## Chaveiro

A GUI guarda a chave de identidade no chaveiro do sistema (`secret=@keyring` no `~/.clam-gui.conf`); sem chaveiro disponível, continua no arquivo (0600).
