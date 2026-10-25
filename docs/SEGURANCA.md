# Segurança, limites e estado em disco

Garantias e limites, limitações conhecidas e chaveiro.

[← Voltar ao README](../README.md)

## Garantias e limites de segurança

- Contra contatos e observadores comuns: ninguém fora do par descobre que você está online, nem liga seus endereços entre períodos.
- **Não** promete anonimato diante de quem monitora a rede inteira (adversário global). Diga isso aos usuários.
- O modo direto expõe o seu IP ao contato; o código exige confiança e aviso confirmado, mas o aviso na interface é responsabilidade do app.
- O relógio é passado como argumento (`agora_unix`); use um relógio confiável. Defasagem maior que uma época faz a busca falhar.
- Não há proteção contra replay de um registro antigo dentro da janela de três épocas.

## Limitações

Ainda **não** existem: TLS/QUIC e travessia de NAT (não há furo de NAT) no adaptador (só TCP + Noise), validação de registros na DHT (um nó armazenador pode sobrescrever um registro com lixo; `open` rejeita lixo, mas ele esconde o registro bom até a próxima publicação), failover de circuitos, descoberta e reputação de relays, integração com MLS e vídeo.
`PathMode` só modela e valida o modo; ele não move mídia.

## Chaveiro

A GUI guarda a chave de identidade no chaveiro do sistema (`secret=@keyring` no `~/.clam-gui.conf`); sem chaveiro disponível, continua no arquivo (0600).
