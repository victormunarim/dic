# Interface gráfica: sala, membros e modo sem diretório

Uso da GUI (`examples/gui`): quem fala, volume, sensibilidade do microfone, apelidos, dispositivos, remover membro e o que funciona sem diretório.

[← Voltar ao README](../README.md)

## Interface da sala: quem fala, volume, apelidos e dispositivos

- **Quem fala.** Cada membro da sala tem uma bolinha: verde e maior enquanto fala (energia do áudio recebido, com 400 ms de retenção), cinza quando conectado e quieto, laranja enquanto a ligação não completou. Na API: `Mesh::is_speaking`.
- **Volume por pessoa.** Um controle de 0 (mudo) a 200 % por membro; vale só para o que você ouve (`Mesh::set_volume`, antes do limitador da mixagem).
- **Apelidos.** O campo ao lado de cada membro grava um apelido salvo no `~/.clam-gui.conf` (`nicks=`, só neste computador). Contatos continuam com o nome da lista; o apelido nunca torna alguém "contato" (atendimento automático segue só a lista de contatos).
- **Controles de áudio.** Com o áudio ligado e o nó online, o cartão "Áudio" tem mudo (envia silêncio), volume do alto-falante (0 a 200 %), ganho do microfone (0 a 300 %), um medidor do microfone (vale mesmo com mudo, para confirmar que ele capta) e "Testar o microfone", que devolve a sua voz ao alto-falante (use fones). Durante o teste a linha fica ocupada.
- **Sensibilidade e tratamento da voz.** O medidor traz uma linha laranja: o limiar de sensibilidade (-60 a -20 dBFS, padrão -45). Quando a voz passa da linha o medidor diz "Microfone: voz". Quanto mais à esquerda a linha, mais sensível. Em volta dela:
  - **Remover ruído (rede neural)** (ligado por padrão): RNNoise, pelo crate `nnnoiseless` (Rust puro). Tira teclado, ventilador, estática e vozes ao fundo, o que filtro por volume não separa. O áudio sobe de 16 para 48 kHz, passa pela rede em quadros de 10 ms e volta; custa ~10 ms de atraso e pouca CPU. A rede também diz, a cada 10 ms, a probabilidade de ser voz: com ela ligada, a porta e o indicador "voz" só contam trechos acima do limiar **e** com probabilidade ≥ 0,5. Nos primeiros ~100 ms de uma chamada a rede ainda se calibra e pode tomar ruído por voz.
  - **Silenciar o que não é voz** (porta, desligada por padrão): abaixo do limiar (ou fora do que a rede reconhece como voz) o som enviado cai pela "Redução do ruído" (6 a 60 dB, padrão 40). A porta abre em 2 ms, segura 250 ms depois da última palavra e fecha em ~60 ms, para não cortar o fim das frases.
  - **Cortar graves** (ligado por padrão): filtro passa-altas de 12 dB por oitava em 100 Hz; tira ventilador, estrondo de mesa e sopro.
  - **Volume automático** (desligado por padrão): mira ~-22 dBFS (reforço até +24 dB, corte até -12 dB); desce rápido e sobe devagar, só aprende com voz e nunca reforça o ruído de fundo. Um limitador suave impede estouro.
  - **Restaurar padrões do áudio** volta tudo ao padrão.

  A ordem é filtro de graves → medidor (e volume por trecho de 5 ms) → RNNoise → porta → ganho do microfone × volume automático → limitador (`examples/gui/engine/voice.rs`). Os ajustes (e o volume e o ganho) ficam salvos no `~/.clam-gui.conf` (`volume`, `mic_gain`, `high_pass`, `denoise`, `gate`, `gate_db`, `gate_depth_db`, `agc`), gravados ao soltar o controle; valores fora da faixa ou ilegíveis voltam ao limite ou ao padrão.
- **Dispositivos de áudio.** Em Configurações avançadas, escolha microfone e alto-falante (`clam::audio::devices`, `Audio::open_with`); vazio usa o padrão do sistema. Se o dispositivo escolhido sumiu (fone desconectado), a abertura falha com aviso em vez de trocar de dispositivo em silêncio.

## Remover membro da sala (GUI)

- Quem cria a sala é o **dono**; o código de convite agora é `segredo:dono[@host:porta]` e a sala fica salva com o dono (`nome segredo:dono`). Códigos antigos (só o segredo) continuam entrando, mas sem dono ninguém remove.
- O dono vê **Remover** ao lado de cada membro, com confirmação ("Sim, remover" / "Cancelar"). Remover troca o segredo (`room::rotate`) e deixa o aviso cifrado só para quem fica; a chamada com o removido cai na hora.
- Os outros membros conferem o aviso a cada sincronização (10 s) e ao entrar (`room::follow`, até 8 trocas seguidas), passam para o segredo novo e a janela o salva no lugar do antigo. O removido fica no segredo velho, sem ver nem ser alcançado pelos que ficaram.
- Limite: um membro desconectado por mais de 3 dias perde o aviso (expira no diretório) e precisa de um convite novo.

## Sem diretório

O diretório só é indispensável no primeiro contato. Depois disso:

- **Endereço guardado.** O `Node` lembra o último relay de cada contato para quem ligou (`Node::known()`, devolvido em `NodeConfig::known`). Ao discar, se esse relay ainda aceita conexão, a chamada vai direto, sem consultar o diretório. Se não responde, cai na busca normal e atualiza o endereço. A GUI guarda a lista em `~/.clam-gui.known`. Para esse endereço valer também depois de reabrir ou recompilar o programa de quem recebe, o relay do nó mantém as mesmas chaves (circuito e link) entre execuções: `NodeConfig::relay_keys`, que a GUI guarda em `~/.clam-gui.relay` (0600). Sem isso cada início gerava chaves novas e o endereço que os contatos tinham guardado deixava de servir, obrigando a consultar o diretório na primeira ligação. A GUI regrava `~/.clam-gui.known` a cada mudança (verificação a cada 5 s) e ao sair, não só depois de uma ligação, e mostra no log qualquer erro de gravação (uma vez por erro), inclusive o do arquivo das chaves do relay.
- **Ligar de volta sem diretório.** O convite leva o relay de quem liga (endereço e chaves, cifrados junto com a identidade; telefones antigos ignoram). Quem atende guarda isso como *dica* (`Node::hints()`, devolvida em `NodeConfig::hints`; na GUI são as linhas com `dica` no fim de `~/.clam-gui.known`). Ao discar a ordem é: endereço guardado, dica, diretório. A dica não é autenticada (qualquer um que conheça a chave de um contato pode escrever o convite), por isso nunca sobrescreve o endereço guardado e só vira endereço guardado depois que uma chamada completa por ela. Sem diretório na partida, o nó informa o IP dele na rede local em vez de 127.0.0.1.
- **Subir com o diretório fora do ar.** `Node::start` não falha mais: registra o problema em `network_note()`/`directory_error()` e tenta publicar de novo a cada 40 s. Enquanto isso, recebe chamadas de quem já tem o seu endereço e liga para quem está em `known`.

Endereço guardado velho não trava a ligação: se o circuito não fica pronto em 5 s, a busca no diretório assume e corrige o cache. Quando o diretório volta, `published_addr()` e `network_note()` se atualizam sozinhos.

Limites: sem diretório não há furo de NAT nem sorteio de primeiro salto (usa o próprio relay por loopback), e o endereço publicado fica sendo o local até o diretório voltar. Quem muda de IP só é achado de novo pelo diretório.

Detalhes do modo sem diretório: o nó consulta o endereço de cada contato (até 32) ao subir e a cada 5 renovações, então dá para ligar a quem você nunca discou. Um furo de NAT que falha não é repetido para o mesmo contato por 5 minutos. Na GUI, discar roda em segundo plano (o botão "Desligar" cancela) e erros de áudio (fone desconectado, driver caiu) aparecem no log.

## Queda de rede no meio da chamada

Se nada chega por 8 s (a rede caiu ou mudou de Wi-Fi), a GUI dá a chamada como perdida. Quem discou tenta religar sozinho, até 5 vezes, com 3 s entre as tentativas, e a janela mostra "religando (n/5)". "Desligar" cancela. Se o outro lado desligou de propósito (`ActiveCall::peer_hung_up()`), ninguém religa; quem recebeu a chamada espera a nova ligação de quem discou.
