# Plano — gravação de consulta longa no Open Flow

**Status:** não implementado. Documento de projeto, escrito em 26/07/2026 para execução futura.
**Contexto:** uso clínico (consultório odontológico), gravar a conversa com o paciente e gerar
anamnese/prontuário a partir dela.

Nada aqui foi aplicado ao código. Os perfis `Anamnese (conversa)` e `Anotação clínica` já existem
no `settings.json` e funcionam **dentro do limite de 4m33s** descrito abaixo.

---

## 1. Fatos verificados (não são estimativas)

Levantados no código real e na documentação da Groq, em 26/07/2026.

### 1.1 Não existe separação de falantes

| Onde | Verificação |
|---|---|
| API Groq | A doc de speech-to-text não expõe parâmetro de diarização. A única menção a "speaker" diz que falantes simultâneos **pioram** a taxa de erro. |
| `lib.rs::transcribe_groq` | Envia `response_format: "json"` e lê apenas o campo `text`. Nem os timestamps são aproveitados. |
| `sidecar/stt_server.py` | `faster-whisper` com `beam_size=1, vad_filter=True`. Não faz diarização. |

Consequência: o texto volta como bloco único. Qualquer rótulo "Paciente:" / "Dentista:" seria
invenção do Gemini a partir do padrão pergunta-resposta — inaceitável em prontuário. Por isso o
perfil `Anamnese (conversa)` instrui explicitamente a **não** atribuir falas.

### 1.2 Limite de duração por gravação

Microfone atual (DELL SOUNDBAR / Dell Speakerphone SP3022), formato padrão lido do registro do
Windows: **48000 Hz, 2 canais, 16 bits**.

O app não reamostra: `Recorder::stop` reduz a mono pegando o primeiro canal e `wav_bytes` grava
16 bits na taxa nativa. Resultado: **96 KB/s**.

| Cenário | Taxa | Bytes/s | Teto de 25 MB (Groq free) |
|---|---|---|---|
| Hoje | 48 kHz mono 16 bits | 96.000 | **4 min 33 s** |
| Com reamostragem | 16 kHz mono 16 bits | 32.000 | 13 min 39 s |

Whisper trabalha internamente a 16 kHz — mandar 48 kHz é desperdício puro de banda e de teto.

Outros limites no caminho:

- `transcribe_groq`: timeout HTTP de **30 s** (cobre upload + transcrição).
- `transcribe` (local): timeout de **60 s**.
- `ensure_sidecar`: espera até 120 s pelo modelo aquecer na primeira vez.
- `rewrite` (Gemini): timeout de **30 s**.
- Falha na Groq cai automaticamente para o local — mas os caminhos `python`/`sidecar` no
  `settings.json` apontam para `C:\dev\open-flow`, que **não existe nesta máquina**. Hoje o
  fallback não tem para onde cair.

### 1.3 Memória durante a gravação

O buffer é `Vec<f32>` interleaved, na taxa e nos canais nativos: 48000 × 2 canais × 4 bytes =
**384 KB/s**.

| Duração | RAM do buffer | WAV 48 kHz mono | WAV 16 kHz mono |
|---|---|---|---|
| 5 min | 115 MB | 28,8 MB | 9,6 MB |
| 30 min | 691 MB | 172 MB | 57,6 MB |
| 60 min | 1,4 GB | 345 MB | 115 MB |

Tudo em RAM, sem nada em disco: um travamento do app no minuto 29 perde a consulta inteira.

### 1.4 Hardware disponível

NVIDIA GeForce RTX 3060, **12 GB de VRAM**. O sidecar já pede
`WhisperModel("large-v3-turbo", device="cuda", compute_type="float16")` — roda sem problema.

---

## 2. Decisões que precisam ser tomadas antes de codar

### 2.1 Privacidade (bloqueante)

Hoje o áudio vai para a Groq e o texto para o Google. O README do próprio projeto avisa que os
planos gratuitos podem usar os dados para treino. Aqui se trata de dado sensível de saúde, com
paciente identificável pela própria voz.

| Opção | Áudio | Texto | Custo |
|---|---|---|---|
| A — como está | Groq (nuvem) | Gemini free | R$ 0 |
| B — só STT local | fica na máquina | Gemini free | R$ 0 |
| C — local + Gemini pago | fica na máquina | Google, tier pago (sem treino) | por token |
| D — tudo local | fica na máquina | LLM local (não suportado hoje) | GPU |

**Recomendação: B no mínimo, C se o texto do prontuário for para a nuvem.** A opção D exigiria um
provedor de reescrita novo no código.

Fora do escopo técnico, mas parte da decisão: consentimento do paciente, onde os arquivos ficam,
por quanto tempo, e quem tem acesso.

### 2.2 Diarização: vale a pena?

Se for necessário separar as vozes, não dá para fazer no `faster-whisper` puro. Caminhos:

- **WhisperX** — alinhamento + `pyannote.audio`. Roda na 3060. Exige token do Hugging Face e
  aceitar a licença do modelo `pyannote/speaker-diarization`.
- **pyannote separado** — diariza primeiro, transcreve depois, cruza por timestamp.

Custo: reescrever o sidecar, mais uns 2 GB de modelos, e processamento bem mais lento que os
216x de fator do turbo. **Sugestão: não fazer na primeira versão.** Marcar a fala verbalmente
("paciente relata que...") resolve na prática, com zero código.

---

## 3. Implementação proposta

### Fase 1 — modo local (pré-requisito de tudo)

Sem isto, nada além de 4m33s funciona.

1. `python -m venv .venv` em `C:\PROJETOS\open-flow`
2. `.venv\Scripts\pip install faster-whisper nvidia-cublas-cu12 nvidia-cudnn-cu12`
3. Corrigir no `settings.json`:
   - `python`: `C:\PROJETOS\open-flow\.venv\Scripts\python.exe`
   - `sidecar`: `C:\PROJETOS\open-flow\sidecar\stt_server.py`
4. Trocar `stt_provider` para `local`
5. Primeira execução baixa ~1,6 GB de modelo

**Medir e anotar aqui:** tempo real de transcrição de 5, 15 e 30 minutos de áudio na 3060.
Todo o resto depende desse número.

### Fase 2 — reamostrar para 16 kHz

Ganho de 3x em tamanho e upload, sem perda de qualidade (Whisper reamostra internamente de
qualquer jeito).

- Onde: `Recorder::stop` ou `wav_bytes` em `lib.rs`
- Como: reamostragem linear simples de 48000 → 16000 já basta para voz; se houver serrilhado,
  usar a crate `rubato`
- Efeito colateral bom: o teto da Groq sobe de 4m33s para 13m39s, e o modo nuvem volta a ser
  utilizável para consultas curtas

### Fase 3 — gravação longa sem perder áudio

Dois problemas separados: não estourar RAM e não perder tudo num crash.

- Gravar direto em WAV no disco durante a captura, em vez de acumular `Vec<f32>`
  (`hound::WavWriter` já é dependência do projeto)
- Local sugerido: `%APPDATA%\OpenFlow\consultas\<AAAA-MM-DD_HH-MM>.wav`
- Ao encerrar, transcrever a partir do arquivo
- Decidir: apagar o WAV depois da transcrição ou manter? (impacta LGPD e disco)

### Fase 4 — transcrição em pedaços

Mesmo com 16 kHz, uma consulta de 40 minutos não cabe nos 25 MB da Groq, e a resposta de 30 min
de áudio pode estourar os timeouts.

- Fatiar em blocos de ~10 min com 5 s de sobreposição
- Transcrever em sequência e concatenar, removendo a duplicação da sobreposição
- Elevar os timeouts: `transcribe` de 60 s para algo compatível com o medido na Fase 1
- Mostrar progresso — 30 minutos de áudio sem nenhum retorno na tela parece travamento

### Fase 5 — saída em arquivo, não no cursor

`insert_text` faz clipboard + Ctrl+V. Para 30 minutos de transcrição isso não serve: destrói o
clipboard, pode estourar o campo de destino e não deixa registro.

- Marcar o perfil como "modo consulta" (campo novo em `Profile`)
- Nesse modo: salvar em `%APPDATA%\OpenFlow\consultas\<data>.md`, copiar para o clipboard e
  avisar no balão (a janela `toast` já existe, criada na implementação dos atalhos múltiplos)
- O `history.jsonl` já guarda `raw` e `final` — a busca da aba Histórico continua funcionando

### Fase 6 — regras fixas versus texto longo

A constante `RULES` em `lib.rs` é aplicada sempre e diz *"NÃO resuma, NÃO omita conteúdo"*. Isso
está certo para ditado curto e briga com a ideia de condensar 40 minutos de conversa em uma
anamnese enxuta.

- Tornar as regras substituíveis por perfil (campo `rules_override` em `Profile`), ou
- Criar uma variante de regras para o modo consulta

Sem isso, o perfil de anamnese fica preso entre a instrução de estilo e a regra fixa.

---

## 4. Arquivos que serão tocados

| Arquivo | Mudança |
|---|---|
| `app/src-tauri/src/lib.rs` | `Recorder` (disco + reamostragem), `Cmd`, pipeline, fatiamento, saída em arquivo, `RULES` por perfil, campo novo em `Profile` |
| `app/src-tauri/src/platform/windows.rs` | nada previsto |
| `sidecar/stt_server.py` | só se entrar diarização |
| `app/src/App.tsx` | opção "modo consulta" no perfil, indicador de progresso |
| `settings.json` | caminhos do Python/sidecar, `stt_provider` |

---

## 5. Riscos

- **Timeout local desconhecido.** Se 30 min de áudio levarem mais que o timeout, trava. Medir na
  Fase 1 antes de qualquer outra coisa.
- **Qualidade com dois falantes distantes do microfone.** A própria Groq documenta piora com
  falantes simultâneos. O Speakerphone SP3022 ajuda, mas precisa de teste real com paciente.
- **Alucinação em silêncio.** `is_hallucination` só protege trechos abaixo de 2 s. Em 30 minutos
  com pausas longas, o Whisper pode inventar frases. O `vad_filter=True` do sidecar reduz isso.
- **Perda de gravação.** Enquanto o áudio estiver só em RAM, qualquer crash perde a consulta.
- **Divergência do upstream.** Estas mudanças são grandes e provavelmente não entram no projeto
  original. Manter em branch separado do `feat/atalhos-multiplos`.

---

## 6. Critérios de aceite

1. Gravar 30 minutos sem estourar RAM nem perder áudio se o app for fechado à força
2. Transcrição completa, sem cortar o fim
3. Áudio não sai da máquina (provado por captura de rede ou pelo log do sidecar)
4. Saída em arquivo, com o clipboard preservado
5. Anamnese gerada sem atribuir falas a quem não se sabe que falou
6. Nenhuma regressão no ditado curto (Ctrl+Win continua colando no cursor em ~2 s)

---

## 7. Ordem sugerida

Fase 1 (medir) → Fase 2 (16 kHz) → Fase 3 (disco) → Fase 5 (saída) → Fase 4 (fatiar) → Fase 6
(regras). Diarização, só se depois do uso real ficar claro que faz falta.

A Fase 1 sozinha já entrega valor: tira o áudio da nuvem e sobe o limite de 4m33s para o que a
GPU aguentar em 60 s.
