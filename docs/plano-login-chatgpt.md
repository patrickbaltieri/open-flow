# Plano — "Continue with ChatGPT" (Sign in with ChatGPT) no Open Flow

Status: **planejado, não implementado.** Escrito em 07/10/2026.

Objetivo: usar a assinatura ChatGPT Plus do usuário para pagar a etapa de
reescrita do ditado, sem chave de API e sem consumir crédito avulso.

---

## 1. O que a OpenAI realmente oferece

"Sign in with ChatGPT" (SIWC) foi expandido no DevDay de 29/09/2026. Além do
login de identidade, existe a capacidade opcional **ChatGPT plan usage**: o app
pede permissão para gastar a cota do plano do usuário em chamadas à Responses
API.

Dois caminhos, com portas de entrada diferentes:

| Caminho | Quem pode usar | Como se cadastra |
|---|---|---|
| App comercial ou hospedado remotamente | parceiros selecionados | formulário de interesse, lista de espera |
| **App open-source ou que roda localmente** | **qualquer um** | **registro dinâmico, sem aprovação prévia** |

O Open Flow é fork de um repositório público e roda na máquina do usuário.
Cai no segundo caso: `client_id=dynamic_agent_client` na primeira autorização,
e a própria OpenAI emite um `client_id` definitivo no retorno. Não há client
secret, não há chave de parceiro, não há fila.

Documentação: <https://developers.openai.com/siwc/token-sharing-open-source>

---

## 2. O limite que decide o escopo

> "Audio/video input, the Files upload API, and the transcription API are not
> supported by this flow."
> — [Preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)

**Transcrição pela assinatura é impossível.** Não é questão de modelo
disponível nem de cota: o fluxo inteiro recusa áudio e recusa o endpoint de
transcrição. O `whisper-large-v3-turbo` da Groq continua sendo quem transcreve,
e isso não muda com este plano.

Portanto o escopo real é: **reescrita apenas**.

---

## 3. O que muda na chamada

A rota autorizada é `POST https://api.openai.com/v1/responses` — não
`/chat/completions`. Isso importa porque o `rewrite_openai()` atual monta um
corpo de `chat/completions` e não serve aqui.

Diferenças obrigatórias em relação ao que o app faz hoje:

| Hoje (`/chat/completions`) | SIWC (`/responses`) |
|---|---|
| `stream: false` | `stream: true` — obrigatório |
| — | `store: false` — obrigatório |
| `temperature: 0.2` | **proibido**, junto com `top_p`, `max_output_tokens`, `metadata`, `user`, `truncation` e outros |
| `messages: [{role: user, ...}]` | `input: [...]`; item explícito `{type: "message", role: "system"}` é **rejeitado** — a instrução vai em `instructions` |
| resposta JSON única | eventos SSE; só vale como sucesso quando chega `response.completed` |

Consequência de projeto: é um **terceiro caminho de reescrita**, ao lado de
`rewrite_openai()` e `rewrite_gemini()`. Não dá para reaproveitar o corpo
existente.

O catálogo de modelos também é próprio: `GET https://api.openai.com/v1/models`
com o mesmo token devolve `{"models":[...]}` com `slug`, `display_name` e
`visibility`. Formato diferente do `{"data":[...]}` que o `ModelPicker` lê hoje.

---

## 4. Fluxo de autorização, passo a passo

Tudo com PKCE, cliente público, sem segredo.

### 4.1 Antes da primeira entrada

Gerar e **persistir** um `ext_agent_host_id` — identificador opaco da máquina.
Formato aceito: `urn:uuid:<uuid-v4>` (mais simples) ou um JWK thumbprint
`urn:ietf:params:oauth:jwk-thumbprint:…` (recomendado pela OpenAI, mas hoje
apenas identificador: a posse da chave privada não é verificada).

Não pode ser e-mail, nome de usuário ou qualquer coisa que identifique a
pessoa. Reinstalar o app não deve gerar host novo; o Windows e o MacBook são
hosts distintos sob o mesmo `client_id`.

### 4.2 Autorização

Abrir o navegador do sistema em `https://auth.openai.com/api/accounts/authorize`
com:

```
client_id              = dynamic_agent_client     (só na primeira vez)
agent_name_hint        = Open Flow                (só na primeira vez)
ext_agent_host_id      = urn:uuid:...
response_type          = code
redirect_uri           = http://127.0.0.1:<porta>/auth/callback
scope                  = openid profile email offline_access resource.invoke chatgpt.tokens.use.direct
resource               = https://api.openai.com/v1
state                  = aleatório por tentativa
nonce                  = aleatório por tentativa
code_challenge_method  = S256
code_challenge         = base64url(sha256(verifier)), sem padding
```

Detalhes que quebram se forem ignorados:

- **`127.0.0.1`, nunca `localhost`.** A OpenAI trata como URIs diferentes.
- O caminho `/auth/callback` tem de bater exatamente — `/callback` não serve.
- Só a **porta** pode mudar entre entradas; esquema, host e caminho ficam fixos.
- O listener tem de estar de pé **antes** de abrir o navegador.
- A mesma `redirect_uri`, com a mesma porta, vai na autorização e na troca do
  código.

### 4.3 Retorno e troca

O retorno traz `code`, `state`, `scope` e — só no primeiro registro — o
`client_id` emitido (`oaiapp_...`). Guardar esse valor; `dynamic_agent_client`
**não** é o ID a salvar.

Trocar em `POST https://auth.openai.com/api/accounts/oauth/token`, formulário
`application/x-www-form-urlencoded`, grant `authorization_code`, com o
`client_id` emitido, `code`, `code_verifier`, a mesma `redirect_uri` e o mesmo
`resource`.

### 4.4 Validação

Validar o `id_token` contra o JWKS publicado da OpenAI: assinatura, emissor,
audiência (igual ao `client_id` emitido), expiração e o `nonce` da tentativa.

Depois, conferir se os escopos concedidos contêm `chatgpt.tokens.use.direct`.
ID token válido **não** significa permissão de gastar a cota — são coisas
separadas, e o usuário pode ter recusado só essa parte.

### 4.5 Guarda e renovação

Credenciais por conta, em arquivo próprio. O access token vale **1 hora**; o
refresh token vale **30 dias** e é rotativo — cada renovação devolve um novo,
com novos 30 dias.

No Windows, proteger com DPAPI, igual às chaves de API de hoje (prefixo `enc:`).
No macOS ficaria em texto, como as demais chaves depois que o autor removeu o
Keychain na v0.1.7 — é regressão conhecida do upstream, não deste plano.

Renovar quando faltar pouco para expirar, nunca no meio de um ditado: um ditado
que esbarra na renovação vira latência visível.

---

## 5. Encaixe no código

### 5.1 `Provider` ganha um tipo

Hoje todo provedor é "compatível com a API da OpenAI, com chave estática".
Falta distinguir. Proposta: campo novo, com padrão que preserva o
comportamento atual.

```rust
#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
enum ProviderKind {
    #[default]
    ApiKey,        // Groq, OpenRouter, OmniRoute, Gemini — o que existe hoje
    ChatgptOauth,  // Sign in with ChatGPT
}
```

`#[serde(default)]` já está no `Provider`, então arquivo de configuração antigo
continua lendo sem migração.

### 5.2 Despacho

`rewrite()` (lib.rs:880) passa a olhar o tipo antes de chamar:

```rust
Some(p) => return match p.kind {
    ProviderKind::ChatgptOauth => rewrite_responses(raw, s, profile, p, &s.rewrite_model),
    ProviderKind::ApiKey       => rewrite_openai(raw, s, profile, p, &s.rewrite_model),
},
```

### 5.3 Funções novas

| Função | O que faz |
|---|---|
| `siwc_authorize()` | comando Tauri: gera PKCE/state/nonce, sobe o listener, abre o navegador |
| `siwc_callback_listener()` | `TcpListener` da std numa porta efêmera; lê a primeira linha do GET, devolve HTML de "pode fechar" |
| `siwc_exchange()` | troca o código por tokens |
| `siwc_validate_id_token()` | assinatura via JWKS, emissor, audiência, expiração, nonce |
| `siwc_refresh()` | renovação com rotação do refresh token |
| `siwc_models()` | `GET /v1/models`, lê `models[]` em vez de `data[]`, filtra `visibility == "list"` |
| `rewrite_responses()` | monta o corpo da Responses API, consome o SSE até `response.completed` |

### 5.4 Dependências novas

| Para quê | Opção enxuta | Opção pronta |
|---|---|---|
| SHA-256 do PKCE | `sha2` | — |
| base64url sem padding | `base64` | — |
| Listener de retorno | `std::net::TcpListener`, ~60 linhas | `tauri-plugin-oauth` |
| Validar JWT contra JWKS | — | `jsonwebtoken` |
| UUID do host | `uuid` com feature `v4` | — |

`jsonwebtoken` arrasta criptografia RSA e é o maior peso do conjunto. Vale
medir o tamanho do binário antes e depois: hoje são ~14,3 MB.

### 5.5 Erros a tratar

| Código | HTTP | Conduta |
|---|---|---|
| `subscription_sharing_usage_limit_exceeded` | 429 | cota do plano acabou; avisar no balão e apontar as configurações de uso do ChatGPT. **Não** deduzir horário de volta |
| `subscription_sharing_user_not_eligible` | 403 | plano não permite; explicar e parar — não repetir, não refazer OAuth em laço |
| `subscription_sharing_usage_unavailable` | — | indisponível no momento |
| admissão | 401 / 403 / 503 | o corpo pode vir como `{"detail":"..."}`, sem o objeto `error` padrão. Tratar como texto de diagnóstico, não como código estável |

Erro de cota pode chegar **depois** de o SSE já ter começado, como evento
`response.failed`. O código tem de ler o fluxo até o fim em vez de assumir
sucesso no primeiro byte.

O `friendly_error()` (lib.rs:1296) recebe esses casos e o balão de aviso da
seção 7 do documento do fork já mostra o motivo — infraestrutura pronta.

---

## 6. Interface

Na aba Provedores, um botão **"Continue with ChatGPT"** — rótulo e marca
definidos pelas [UI/UX guidelines](https://developers.openai.com/siwc/ui-ux-guidelines)
da OpenAI, que também pedem indicar quando o plano está em uso e o que oferecer
quando a cota acaba.

Depois de conectado, o cartão mostra o e-mail da conta e o seletor de modelo
passa a listar os modelos da assinatura. O seletor de **transcrição** não muda:
o provedor OAuth não aparece ali, porque o fluxo não transcreve.

---

## 7. Testes

Os dois primeiros não tocam a rede e protegem contra regressão de merge:

1. `pkce_gera_desafio_s256_sem_padding` — vetor conhecido do RFC 7636.
2. `corpo_da_responses_nao_leva_campo_proibido` — garante ausência de
   `temperature`, `top_p`, `max_output_tokens`, `store: true` e de item com
   `role: "system"`.
3. `catalogo_do_chatgpt_le_models_nao_data` — o formato é diferente do resto.
4. `limite_de_cota_vira_aviso_e_nao_trava_a_sessao` — 429 de cota não deve
   rebaixar a sessão inteira, pela mesma lógica do `erro_e_definitivo` do
   upstream (item 5 do documento do fork).

---

## 8. Medição — vale a pena, com uma condição

Feita em 07/10/2026, antes de escrever qualquer linha de implementação. O Codex
CLI já estava instalado e autenticado nesta máquina (`auth_mode: chatgpt`), o
que permitiu medir os modelos da assinatura sem construir o OAuth.

### Método

Entrada: ditado real nº 292 do `history.jsonl` — 2.872 caracteres, o maior caso
clínico de verdade do histórico. Prompt montado como `build_prompt()` monta:
2.872 de transcrição + 5.795 de estilo do perfil "Anotção clínica" = 9.154
caracteres.

**A maior entrada do histórico não serve para este teste.** São 3.400 caracteres,
mas é o roteiro de um post de carrossel, não uma anotação clínica — e o perfil
clínico aplicado a ela produz comparação sem sentido. Foi o primeiro erro deste
teste, corrigido antes de concluir qualquer coisa.

Critério objetivo: o perfil define **8 seções** — Exame clínico, Exame por
imagem, Plano de tratamento, Anestesia, Conduta, Materiais, Prescrição,
Orientações. Conta-se quantas saíram, quantas foram inventadas, e confere-se um
trecho de fidelidade: o original diz *"remoção completa da resina antiga"*.

### Resultado

| Modelo | ms | Seções | Problemas |
|---|---|---|---|
| **Codex / `gpt-5.6-terra` medium** | 33.297 | **8/8** | **nenhum** |
| Codex / `gpt-5.6-terra` low | 30.827 | 7/8 | faltou Materiais |
| Codex / `gpt-6.1-sol` medium | 25.367 | 7/8 | faltou Plano de tratamento; inventou "Relato do paciente" |
| Codex / `gpt-6.1-sol` low | 19.689 | 6/8 | faltou Plano e Materiais; inventou "Relato do paciente" |
| Groq / `openai/gpt-oss-120b` | **8.441** | **8/8** | erro de conteúdo + tipografia |

O erro da Groq não é estilístico. Ela escreveu **"Desobturção completa da
restauração antiga"** onde o original diz "remoção completa da resina antiga":
desobturação é de canal radicular, não de restauração — e a palavra saiu sem o
"a". Além disso, injeta hífen não separável (U+2011) em "guta‑percha" e
"disto‑mesiolingual", caractere invisível que viaja para o prontuário.

O `gpt-5.6-terra` com esforço médio acertou as 8 seções, escreveu "Remoção
completa da resina antiga", formatou os anestésicos como `1:100.000` e
`1:200.000`, e não usou U+2011.

O `sol` é mais fiel ao conteúdo falado que a Groq, mas **inventa seção fora do
perfil** — exatamente o defeito já rejeitado quando o perfil produziu uma seção
"PENDÊNCIAS" por conta própria. Entre os dois da assinatura, `terra` obedece
melhor.

### Descontando o custo do CLI

Overhead de processo medido com um prompt trivial ("responda OK"):
`gpt-5.6-terra` 10.031 ms, `gpt-6.1-sol` 5.999 ms. Então o `terra` medium gasta
cerca de **23 s de modelo**; a chamada HTTP direta da Responses API pagaria isso,
não os 33 s. Continua **2,7× mais lento** que a Groq.

### Conclusão

Vale implementar, **desde que a escolha de provedor de reescrita passe a ser por
perfil**. Hoje é global, e isso força a escolha errada nos dois sentidos:

- "Anotção clínica" vai para o prontuário. Esperar 23 s por um texto sem erro
  clínico é troca boa.
- Ditado corrido, resposta de mensagem, texto solto: 23 s é inaceitável, e a
  Groq em 8 s resolve.

O app já tem atalho por perfil (item 2 do documento do fork). Acrescentar
`Profile.rewrite_provider_name` e `Profile.rewrite_model`, com vazio
significando "usa o global", é mudança pequena e de migração trivial — e é
pré-requisito para esta integração valer a pena.

Dados brutos em `open-flow-instalar/comparar-reescrita.txt`; script em
`comparar-reescrita.py`, reprodutível por tarefa agendada.

### O que continua contra

- Cada reescrita consome cota do Plus, que o usuário também usa para outras coisas.
- O custo de construção é o maior de tudo que já se fez neste fork: OAuth com
  PKCE, servidor de retorno, validação de JWT, renovação rotativa, um formato de
  API novo e consumo de SSE.

---

## 9. O que fica de fora

- **Transcrição pela assinatura** — recusada pelo fluxo, como consta na seção 2.
- **OAuth do Google/Gemini** — fora deste plano; o Gemini continua por chave.
- **Conversas do ChatGPT** — a permissão não dá acesso a elas, e o app não quer.
- **Endpoints `backend-api` do ChatGPT** — a documentação proíbe apontar para
  lá. Só `https://api.openai.com/v1`.
