# Segurança, limites e estado em disco

Garantias e limites, limitações conhecidas, limite de taxa, troca de segredo da sala e estado do diretório.

[← Voltar ao README](../README.md)

## Garantias e limites de segurança

- Contra contatos e observadores comuns: ninguém fora do par descobre que você está online, nem liga seus endereços entre períodos.
- **Não** promete anonimato diante de quem monitora a rede inteira (adversário global). Diga isso aos usuários.
- O modo direto expõe o seu IP ao contato; o código exige confiança e aviso confirmado, mas o aviso na interface é responsabilidade do app.
- O relógio é passado como argumento (`agora_unix`); use um relógio confiável. Defasagem maior que uma época faz a busca falhar.
- Não há proteção contra replay de um registro antigo dentro da janela de três épocas.

## Limitações

Ainda **não** existem: TLS/QUIC e travessia de NAT no adaptador (só TCP + Noise), validação de registros na DHT (um nó armazenador pode sobrescrever um registro com lixo; `open` rejeita lixo, mas ele esconde o registro bom até a próxima publicação), failover de circuitos, descoberta e reputação de relays, integração com MLS e vídeo.
`PathMode` só modela e valida o modo; ele não move mídia.

## Segurança: limite de taxa, troca de segredo da sala e chaveiro

- **Limite por IP no diretório.** Cada IP (IPv6 por /64) tem um balde de fichas (`RateLimit`, padrão 50/s, rajada 200). Pedido simples custa 1; operações de sala, avisos e punch custam 5; teste de alcançabilidade custa 20. Conexões novas também gastam ficha. `DirectoryServer::spawn_limited` troca os valores.
- **Troca de segredo da sala.** O dono (chave de identidade que criou a sala) chama `room::rotate(server, owner, old_secret, keep)`: gera um segredo novo e deixa, no diretório, um aviso por membro mantido, cifrado com o DH entre as identidades do dono e do membro. Quem foi removido não recebe aviso e não abre o dos outros. Cada membro chama `room::follow(server, me, owner, old_secret)` ao entrar e adota o segredo novo. Avisos expiram em 3 dias; o diretório guarda no máximo 8192.
- **Chaveiro.** A GUI guarda a chave de identidade no chaveiro do sistema (`secret=@keyring` no `~/.clam-gui.conf`); sem chaveiro disponível, continua no arquivo (0600).

## Estado do diretório em disco

`directory [escuta] [estado]` grava as publicações e as salas em `estado` a cada 5 s (arquivo temporário + renomear) e relê ao subir, descartando o que passou de 120 s. Um reinício não esvazia as salas nem muda o anfitrião. Sem o argumento, o estado fica só em memória.
