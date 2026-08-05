use std::collections::HashSet;
use std::io::Cursor;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};

mod platform;

const RULES: &str = "Você é um editor de ditado. Receberá a transcrição literal de uma fala em pt-BR.\n\
Regras invioláveis:\n\
1. Quando o falante se corrige (\"não, melhor...\", \"deixa eu corrigir...\", \"quer dizer...\"), mantenha APENAS a versão final e descarte a descartada.\n\
2. Remova hesitações, muletas (\"é...\", \"tipo\", \"então...\") e repetições.\n\
3. Corrija concordância e pontuação. NÃO adicione fatos, NÃO resuma, NÃO omita conteúdo.\n";

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Profile {
    name: String,
    style: String,
    /// atalho próprio: dita já no estilo deste perfil, sem trocar o perfil ativo
    hotkey: String,
    /// "hold" (padrão) ou "toggle"
    hotkey_mode: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Snippet {
    trigger: String,
    text: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    /// atalho de ditado no modo "segurar e falar"
    hotkey: String,
    /// atalho de ditado no modo "apertar para iniciar, apertar para parar"
    hotkey_toggle: String,
    /// atalho que avança para o próximo perfil da lista
    hotkey_cycle_profile: String,
    /// tecla que aborta o ditado em andamento; vazio desliga
    cancel_key: String,
    /// legado: antes escolhia o modo do atalho único. Só é lido na migração.
    mode: String,
    mic: Option<String>,
    stt_provider: String,
    groq_api_key: Option<String>,
    groq_model: String,
    speech_lang: String,
    output_lang: String,
    gemini_model: String,
    gemini_api_key: Option<String>,
    /// quem reescreve o texto: "gemini" ou "groq" (reaproveita a chave da transcrição)
    rewrite_provider: String,
    groq_rewrite_model: String,
    wave_style: String,
    theme: String,
    profiles: Vec<Profile>,
    active_profile: String,
    dictionary: Vec<String>,
    snippets: Vec<Snippet>,
    history_enabled: bool,
    python: String,
    sidecar: String,
    stt_port: u16,
}

fn default_profiles() -> Vec<Profile> {
    let p = |name: &str, style: &str| Profile {
        name: name.into(),
        style: style.into(),
        hotkey: String::new(),
        hotkey_mode: "hold".into(),
    };
    vec![
        p("Bruto corrigido", "texto natural corrigido, mantendo o tom do falante."),
        p("Jurídico formal", "certidão ou comunicação de oficial de justiça, linguagem jurídica formal brasileira (ex.: \"Certifico e dou fé que...\", \"genitor\", \"Ante o exposto, devolvo o presente mandado\")."),
        p("E-mail profissional", "e-mail profissional claro e cordial, com saudação e fecho quando fizer sentido."),
        p("WhatsApp curto", "mensagem curta e informal de WhatsApp, direta, sem formalidade excessiva."),
        p("Roteiro de vídeo", "roteiro falado para vídeo do YouTube: frases curtas, ritmo de fala natural, tom de conversa."),
    ]
}

#[cfg(windows)]
fn default_hotkey() -> String {
    "Ctrl+Win".into()
}

// Ctrl+Option: a combinação mais "vazia" do macOS. Ctrl+Cmd foi descartado
// porque letra acidental durante o hold dispara combos do sistema
// (Ctrl+Cmd+Q trava a tela).
#[cfg(target_os = "macos")]
fn default_hotkey() -> String {
    "Ctrl+Option".into()
}

#[cfg(windows)]
fn default_sidecar_paths() -> (String, String) {
    (
        r"C:\dev\open-flow\.venv\Scripts\python.exe".into(),
        r"C:\dev\open-flow\sidecar\stt_server.py".into(),
    )
}

#[cfg(target_os = "macos")]
fn default_sidecar_paths() -> (String, String) {
    let home = dirs::home_dir().unwrap_or_default();
    (
        home.join("dev/open-flow/.venv/bin/python").to_string_lossy().into_owned(),
        home.join("dev/open-flow/sidecar/stt_server.py").to_string_lossy().into_owned(),
    )
}

impl Default for Settings {
    fn default() -> Self {
        let (python, sidecar) = default_sidecar_paths();
        Settings {
            hotkey: default_hotkey(),
            hotkey_toggle: String::new(),
            hotkey_cycle_profile: String::new(),
            // Esc e não Espaço: com o atalho segurado, Ctrl+Option+Espaço (mac) e
            // Win+Espaço (Windows) são atalhos do sistema que trocam a fonte de
            // entrada — o hook é listen-only, então o sistema recebe a combinação
            // junto e o layout do teclado muda no meio do ditado.
            cancel_key: "ESC".into(),
            mode: "hold".into(),
            mic: None,
            stt_provider: "groq".into(),
            groq_api_key: None,
            groq_model: "whisper-large-v3-turbo".into(),
            speech_lang: "pt".into(),
            output_lang: "same".into(),
            gemini_model: "gemini-3.1-flash-lite".into(),
            gemini_api_key: None,
            rewrite_provider: "gemini".into(),
            groq_rewrite_model: "llama-3.3-70b-versatile".into(),
            wave_style: "tech".into(),
            theme: "dark".into(),
            profiles: default_profiles(),
            active_profile: "Bruto corrigido".into(),
            dictionary: Vec::new(),
            snippets: Vec::new(),
            history_enabled: true,
            // ponytail: caminhos desta máquina; viram recurso empacotado na fase 3
            python,
            sidecar,
            stt_port: 17765,
        }
    }
}

struct Shared {
    settings: Mutex<Settings>,
    bindings: Mutex<Vec<Binding>>,
    cancel: Mutex<Option<u32>>,
    sidecar: Mutex<Option<std::process::Child>>,
    insert_lock: Mutex<()>,
    /// preenchido no setup(); o hook de teclado precisa dele para trocar perfil e avisar na tela
    app: OnceLock<AppHandle>,
    /// último texto inserido: a colagem pode falhar sem erro (janela sem foco,
    /// campo não editável) e o clipboard volta ao valor antigo 300ms depois —
    /// sem esta cópia o ditado some sem deixar rastro
    last_text: Mutex<String>,
    /// cada exibição do overlay ganha um número; quem termina tarde só mexe na
    /// janela se ainda for o dono dela (ditar de novo enquanto o anterior processa)
    overlay_gen: AtomicU64,
}

impl Settings {
    fn groq_ready(&self) -> bool {
        self.groq_api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
    }
}

fn config_dir() -> std::path::PathBuf {
    dirs::config_dir().unwrap().join("OpenFlow")
}

fn settings_path() -> std::path::PathBuf {
    config_dir().join("settings.json")
}

// --- Segredos: DPAPI (Windows) ⇄ Keychain (macOS); no disco fica "enc:..." ---
const ENC_PREFIX: &str = "enc:";

/// `name` identifica o item no cofre da plataforma (o Keychain precisa; o DPAPI ignora)
fn decrypt_key(name: &str, v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| match s.strip_prefix(ENC_PREFIX) {
            Some(stored) => platform::unprotect_secret(name, stored).unwrap_or_default(),
            None => s.clone(),
        })
        .filter(|s| !s.is_empty())
}

fn encrypt_key(name: &str, v: &Option<String>) -> Option<String> {
    v.as_ref().filter(|s| !s.is_empty()).map(|s| {
        if s.starts_with(ENC_PREFIX) {
            s.clone()
        } else {
            platform::protect_secret(name, s)
                .map(|b| format!("{ENC_PREFIX}{b}"))
                .unwrap_or_else(|| s.clone())
        }
    })
}

/// grava no disco com as chaves criptografadas (em memória ficam em texto)
fn persist_settings(s: &Settings) -> Result<(), String> {
    let mut disk = s.clone();
    disk.gemini_api_key = encrypt_key("gemini_api_key", &disk.gemini_api_key);
    disk.groq_api_key = encrypt_key("groq_api_key", &disk.groq_api_key);
    let path = settings_path();
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&path, serde_json::to_string_pretty(&disk).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn history_path() -> std::path::PathBuf {
    config_dir().join("history.jsonl")
}

/// retorna (settings, primeira_execucao)
fn load_settings() -> (Settings, bool) {
    let path = settings_path();
    if let Ok(txt) = std::fs::read_to_string(&path) {
        // tolera BOM que editores/PowerShell inserem
        match serde_json::from_str::<Settings>(txt.trim_start_matches('\u{feff}')) {
            Ok(mut s) => {
                if s.profiles.is_empty() {
                    s.profiles = default_profiles();
                }
                // versões antigas tinham um atalho só, com o modo num campo à parte:
                // quem estava em "toggle" mantém o comportamento no atalho equivalente
                if s.mode == "toggle" && s.hotkey_toggle.is_empty() && !s.hotkey.is_empty() {
                    s.hotkey_toggle = std::mem::take(&mut s.hotkey);
                }
                s.mode = "hold".into();
                if s.rewrite_provider.is_empty() {
                    s.rewrite_provider = "gemini".into();
                }
                if s.groq_rewrite_model.is_empty() {
                    s.groq_rewrite_model = "llama-3.3-70b-versatile".into();
                }
                for p in &mut s.profiles {
                    if p.hotkey_mode.is_empty() {
                        p.hotkey_mode = "hold".into();
                    }
                }
                s.gemini_api_key = decrypt_key("gemini_api_key", &s.gemini_api_key);
                s.groq_api_key = decrypt_key("groq_api_key", &s.groq_api_key);
                return (s, false);
            }
            Err(e) => {
                // nunca descartar o arquivo do usuário em silêncio
                eprintln!("[settings] arquivo ilegível ({e}); backup em settings.json.bak");
                let _ = std::fs::copy(&path, path.with_extension("json.bak"));
            }
        }
    }
    let s = Settings::default();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&s).unwrap());
    (s, true)
}

/// keycodes por plataforma (VK no Windows, CGKeyCode no mac) vêm de platform::
/// Cada elemento é um grupo: basta uma das teclas do grupo (ex.: Ctrl esquerdo ou direito).
/// `None` quando o texto está vazio ou cita uma tecla desconhecida — atalho inválido é
/// ignorado, nunca vira uma tecla arbitrária.
fn parse_hotkey(s: &str) -> Option<Vec<Vec<u32>>> {
    let mut groups = Vec::new();
    for token in s.split('+') {
        let name = token.trim().to_uppercase();
        if name.is_empty() {
            return None;
        }
        let group = platform::modifier_group(&name)
            .or_else(|| platform::key_from_name(&name).map(|k| vec![k]))?;
        groups.push(group);
    }
    if groups.is_empty() {
        None
    } else {
        Some(groups)
    }
}

/// O que um atalho faz quando disparado.
#[derive(Clone)]
enum Act {
    /// grava e dita; `profile` força o estilo daquele perfil sem mudar o perfil ativo
    Record { toggle: bool, profile: Option<String> },
    /// avança para o próximo perfil da lista
    CycleProfile,
}

#[derive(Clone)]
struct Binding {
    /// identidade estável do vínculo ("hold", "toggle", "cycle", "profile:<nome>")
    id: String,
    groups: Vec<Vec<u32>>,
    act: Act,
}

fn add_binding(v: &mut Vec<Binding>, id: String, keys: &str, act: Act) {
    if let Some(groups) = parse_hotkey(keys) {
        // um mesmo atalho em dois lugares: vale o primeiro cadastrado
        if v.iter().any(|b| b.groups == groups) {
            eprintln!("[hotkey] atalho duplicado ignorado em {id}: {keys}");
            return;
        }
        v.push(Binding { id, groups, act });
    }
}

fn build_bindings(s: &Settings) -> Vec<Binding> {
    let mut v = Vec::new();
    add_binding(&mut v, "hold".into(), &s.hotkey, Act::Record { toggle: false, profile: None });
    add_binding(&mut v, "toggle".into(), &s.hotkey_toggle, Act::Record { toggle: true, profile: None });
    add_binding(&mut v, "cycle".into(), &s.hotkey_cycle_profile, Act::CycleProfile);
    for p in &s.profiles {
        add_binding(
            &mut v,
            format!("profile:{}", p.name),
            &p.hotkey,
            Act::Record { toggle: p.hotkey_mode == "toggle", profile: Some(p.name.clone()) },
        );
    }
    // mais teclas = mais específico. Sem isto, Ctrl+Win dispararia junto com Ctrl+Win+J.
    v.sort_by(|a, b| b.groups.len().cmp(&a.groups.len()));
    v
}

/// vazio = cancelamento desligado
fn parse_cancel_key(s: &str) -> Option<u32> {
    let name = s.trim().to_uppercase();
    if name.is_empty() {
        return None;
    }
    // key_from_name devolve None para nome desconhecido: tecla inválida desliga o
    // cancelamento em vez de virar F9 por engano
    platform::key_from_name(&name)
}

type LevelFn = Box<dyn FnMut(f32) + Send>;

struct Recorder {
    stream: Option<cpal::Stream>,
    buf: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    channels: u16,
}

impl Recorder {
    fn new() -> Self {
        Recorder { stream: None, buf: Arc::new(Mutex::new(Vec::new())), rate: 16000, channels: 1 }
    }

    fn start(&mut self, mic: Option<&str>, mut on_level: LevelFn) -> Result<(), String> {
        let host = cpal::default_host();
        let device = match mic {
            Some(name) => host
                .input_devices()
                .map_err(|e| e.to_string())?
                .find(|d| d.name().map(|n| n.contains(name)).unwrap_or(false))
                .ok_or_else(|| format!("microfone '{name}' não encontrado"))?,
            None => host.default_input_device().ok_or("sem microfone padrão")?,
        };
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        self.rate = config.sample_rate().0;
        self.channels = config.channels();
        self.buf.lock().unwrap().clear();
        let buf = self.buf.clone();
        let err_fn = |e| eprintln!("[audio] erro no stream: {e}");
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_input_stream(
                    &config.into(),
                    move |data: &[f32], _: &_| {
                        let rms = (data.iter().map(|s| s * s).sum::<f32>()
                            / data.len().max(1) as f32)
                            .sqrt();
                        on_level(rms);
                        buf.lock().unwrap().extend_from_slice(data);
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| e.to_string())?,
            cpal::SampleFormat::I16 => device
                .build_input_stream(
                    &config.into(),
                    move |data: &[i16], _: &_| {
                        let mut b = buf.lock().unwrap();
                        let start = b.len();
                        b.extend(data.iter().map(|s| *s as f32 / 32768.0));
                        let chunk = &b[start..];
                        let rms = (chunk.iter().map(|s| s * s).sum::<f32>()
                            / chunk.len().max(1) as f32)
                            .sqrt();
                        drop(b);
                        on_level(rms);
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| e.to_string())?,
            f => return Err(format!("formato de sample não suportado: {f}")),
        };
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) -> (Vec<f32>, u32) {
        self.stream = None;
        let raw = std::mem::take(&mut *self.buf.lock().unwrap());
        let ch = self.channels as usize;
        let mono: Vec<f32> = if ch > 1 { raw.chunks(ch).map(|c| c[0]).collect() } else { raw };
        (mono, self.rate)
    }
}

/// maior RMS em janelas de 100ms — proxy de "houve fala?"
fn peak_rms(samples: &[f32], rate: u32) -> f32 {
    let win = (rate as usize / 10).max(1);
    samples
        .chunks(win)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .fold(0.0f32, f32::max)
}

/// alucinações típicas do Whisper em áudio sem fala
fn is_hallucination(text: &str, audio_secs: f32) -> bool {
    if audio_secs >= 2.0 {
        return false;
    }
    let norm: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    matches!(norm.trim(), "e aí" | "eaí" | "obrigado" | "obrigada" | "tchau" | "valeu" | "thank you")
}

fn wav_bytes(samples: &[f32], rate: u32) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    cursor.into_inner()
}

/// Cliente HTTP único para as APIs de nuvem. Criar um por chamada custava um
/// handshake TCP+TLS a cada ditado e, pior, sorteava um endereço novo toda vez:
/// em rede com IPv6 anunciado mas sem rota (VPN que bloqueia IPv6, por exemplo)
/// a conexão fica pendurada até o timeout de 30s. Amarrar a saída em IPv4 e
/// reaproveitar a conexão elimina os dois problemas.
fn cloud_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("cliente HTTP de nuvem")
    })
}

fn sidecar_healthy(port: u16) -> bool {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()
        .and_then(|c| c.get(format!("http://127.0.0.1:{port}/health")).send().ok())
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn ensure_sidecar(shared: &Shared, s: &Settings) -> Result<(), String> {
    if sidecar_healthy(s.stt_port) {
        return Ok(());
    }
    {
        let mut guard = shared.sidecar.lock().unwrap();
        if sidecar_healthy(s.stt_port) {
            return Ok(());
        }
        if let Some(c) = guard.as_mut() {
            let _ = c.kill();
        }
        *guard = platform::spawn_sidecar(s);
        if guard.is_none() {
            return Err("não foi possível iniciar a transcrição local".into());
        }
    }
    println!("[stt] aquecendo transcrição local (primeira vez leva ~30s)...");
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if sidecar_healthy(s.stt_port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err("transcrição local não ficou pronta em 120s".into())
}

fn transcribe_groq(wav: Vec<u8>, s: &Settings) -> Result<String, String> {
    let key = s.groq_api_key.clone().filter(|k| !k.is_empty()).ok_or("chave Groq ausente")?;
    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::blocking::multipart::Form::new()
        .part("file", part)
        .text("model", s.groq_model.clone())
        .text("response_format", "json");
    if s.speech_lang != "auto" {
        form = form.text("language", s.speech_lang.clone());
    }
    let resp = cloud_client()
        .post("https://api.groq.com/openai/v1/audio/transcriptions")
        .bearer_auth(key)
        .multipart(form)
        .send()
        .map_err(|e| format!("Groq: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Groq HTTP {}: {}", resp.status(), resp.text().unwrap_or_default()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    Ok(v["text"].as_str().unwrap_or("").trim().to_string())
}

fn run_stt(wav: Vec<u8>, shared: &Shared, s: &Settings) -> Result<String, String> {
    if s.stt_provider == "groq" && s.groq_ready() {
        match transcribe_groq(wav.clone(), s) {
            Ok(t) => {
                println!("[stt] transcrito via Groq (nuvem)");
                return Ok(t);
            }
            Err(e) => eprintln!("[stt] Groq falhou ({e}); caindo para o local"),
        }
    }
    match ensure_sidecar(shared, s) {
        Ok(()) => transcribe(wav, s.stt_port, &s.speech_lang),
        Err(e) if s.stt_provider == "local" && s.groq_ready() => {
            eprintln!("[stt] local indisponível ({e}); caindo para o Groq");
            transcribe_groq(wav, s)
        }
        Err(e) => Err(e),
    }
}

fn transcribe(wav: Vec<u8>, port: u16, lang: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = client
        .post(format!("http://127.0.0.1:{port}/stt?lang={lang}"))
        .body(wav)
        .send()
        .map_err(|e| format!("sidecar STT: {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    Ok(v["text"].as_str().unwrap_or("").trim().to_string())
}

fn build_prompt(s: &Settings, profile: &str) -> String {
    let style = s
        .profiles
        .iter()
        .find(|p| p.name == profile)
        .map(|p| p.style.clone())
        .unwrap_or_else(|| "texto natural corrigido, mantendo o tom do falante.".into());
    let mut p = RULES.to_string();
    let dict: Vec<&str> = s.dictionary.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
    if !dict.is_empty() {
        p.push_str(&format!("4. Grafias obrigatórias quando essas palavras aparecerem: {}.\n", dict.join(", ")));
    }
    p.push_str("Responda SOMENTE com o texto final, sem comentários.\n");
    if s.output_lang != "same" && s.output_lang != s.speech_lang {
        let idioma = match s.output_lang.as_str() {
            "pt" => "português do Brasil",
            "en" => "inglês",
            "es" => "espanhol",
            other => other,
        };
        p.push_str(&format!(
            "Escreva o texto final em {idioma}, mantendo URLs, e-mails e nomes próprios intactos.\n"
        ));
    }
    p.push_str(&format!("Estilo: {style}\n\nTranscrição:\n"));
    p
}

fn rewrite(raw: &str, s: &Settings, profile: &str) -> Result<String, String> {
    if s.rewrite_provider == "groq" {
        return rewrite_groq(raw, s, profile);
    }
    rewrite_gemini(raw, s, profile)
}

/// Reescrita pela Groq, com a mesma chave da transcrição. Latência bem mais
/// estável que o free tier do Gemini, que oscila de 0,5s a mais de 30s.
fn rewrite_groq(raw: &str, s: &Settings, profile: &str) -> Result<String, String> {
    let key = s.groq_api_key.clone().filter(|k| !k.is_empty()).ok_or("chave Groq ausente")?;
    let body = serde_json::json!({
        "model": s.groq_rewrite_model,
        "temperature": 0.2,
        "messages": [{"role": "user", "content": format!("{}{}", build_prompt(s, profile), raw)}]
    });
    let v: serde_json::Value = cloud_client()
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .map_err(|e| format!("Groq (reescrita): {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(format!("Groq (reescrita): {msg}"));
    }
    let text = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
    // modelos com cadeia de raciocínio devolvem <think>...</think> antes da resposta
    let text = match text.rsplit_once("</think>") {
        Some((_, depois)) => depois,
        None => text,
    };
    if text.trim().is_empty() {
        return Err(format!("resposta vazia da Groq: {v}"));
    }
    Ok(text.trim().to_string())
}

fn rewrite_gemini(raw: &str, s: &Settings, profile: &str) -> Result<String, String> {
    let key = s
        .gemini_api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| std::env::var("GEMINI_API_KEY").ok())
        .ok_or("chave do Gemini ausente (settings.json ou GEMINI_API_KEY)")?;
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        s.gemini_model, key
    );
    let body = serde_json::json!({
        "contents": [{"parts": [{"text": format!("{}{}", build_prompt(s, profile), raw)}]}],
        "generationConfig": {"temperature": 0.2}
    });
    let v: serde_json::Value = cloud_client()
        .post(url)
        .json(&body)
        .send()
        .map_err(|e| format!("Gemini: {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    let parts = &v["candidates"][0]["content"]["parts"];
    let text = parts
        .as_array()
        .map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""))
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Err(format!("resposta vazia do Gemini: {v}"));
    }
    Ok(text.trim().to_string())
}

fn apply_snippets(text: &str, snippets: &[Snippet]) -> String {
    let mut out = text.to_string();
    for sn in snippets {
        if sn.trigger.is_empty() {
            continue;
        }
        let lower_out = out.to_lowercase();
        let lower_trig = sn.trigger.to_lowercase();
        if let Some(pos) = lower_out.find(&lower_trig) {
            let mut end = pos + lower_trig.len();
            // consome pontuação final colada ao gatilho ("meu instagram." -> link)
            if out[end..].starts_with('.') || out[end..].starts_with(',') {
                end += 1;
            }
            out = format!("{}{}{}", &out[..pos], sn.text, &out[end..]);
        }
    }
    out
}

fn insert_text(text: &str) -> Result<(), String> {
    let mut clip = arboard::Clipboard::new().map_err(|e| {
        app_log("insert.log", &format!("clipboard indisponível: {e}"));
        e.to_string()
    })?;
    let backup = clip.get_text().ok();
    clip.set_text(text).map_err(|e| {
        app_log("insert.log", &format!("set_text falhou: {e}"));
        e.to_string()
    })?;
    std::thread::sleep(Duration::from_millis(60));
    // modificador físico ainda pressionado + V sintético formaria combos do sistema
    platform::wait_modifiers_released(Duration::from_secs(1));
    app_log(
        "insert.log",
        &format!(
            "colando {} chars | alvo={} | mods presos={} | backup={}",
            text.chars().count(),
            platform::foreground_window_desc(),
            platform::modifiers_down(),
            if backup.is_some() { "sim" } else { "não (clipboard vazio)" },
        ),
    );
    match platform::paste_shortcut() {
        Ok(()) => app_log("insert.log", "paste_shortcut retornou ok"),
        Err(e) => {
            app_log("insert.log", &format!("paste_shortcut FALHOU: {e}"));
            return Err(e);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    if let Some(b) = backup {
        let _ = clip.set_text(b);
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone)]
struct HistoryEntry {
    ts: u64,
    profile: String,
    raw: String,
    r#final: String,
    audio_secs: f32,
    stt_secs: f32,
    total_secs: f32,
}

fn append_history(entry: &HistoryEntry) {
    let _ = std::fs::create_dir_all(config_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(history_path()) {
        let _ = writeln!(f, "{}", serde_json::to_string(entry).unwrap());
    }
}

fn app_log(file: &str, msg: &str) {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = std::fs::create_dir_all(config_dir());
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(config_dir().join(file))
    {
        let _ = writeln!(f, "{ts} {msg}");
    }
}

fn overlay_log(msg: &str) {
    app_log("overlay.log", msg);
}

/// mostra o overlay e devolve o número desta exibição
fn overlay_show(app: &AppHandle) -> u64 {
    let shared = app.state::<Arc<Shared>>();
    let gen = shared.overlay_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let style = shared.settings.lock().unwrap().wave_style.clone();
    if let Some(o) = app.get_webview_window("overlay") {
        // monitor onde está o cursor (foco de digitação), não o primário
        let mon = o
            .cursor_position()
            .ok()
            .and_then(|p| o.monitor_from_point(p.x, p.y).ok().flatten())
            .or_else(|| o.primary_monitor().ok().flatten());
        if let (Some(mon), Ok(sz)) = (mon, o.outer_size()) {
            let m = mon.size();
            let x = mon.position().x + (m.width as i32 - sz.width as i32) / 2;
            let y = mon.position().y + m.height as i32 - sz.height as i32 - 72;
            if let Err(e) = o.set_position(PhysicalPosition::new(x, y)) {
                overlay_log(&format!("set_position falhou: {e}"));
            }
        }
        let _ = o.set_always_on_top(true);
        let _ = app.emit_to("overlay", "overlay_show", style);
    } else {
        overlay_log("janela overlay inexistente");
    }
    gen
}

/// só apaga se esta exibição ainda for a atual
///
/// A janela do overlay nasce visível e nunca é escondida: no Windows, esconder e
/// reexibir a janela deixa a webview sem pintar — a janela reaparecia no lugar
/// certo, no topo, do tamanho certo, e mesmo assim invisível, porque o conteúdo
/// nunca era apresentado. Quem "some" e "aparece" agora é só o conteúdo, do lado
/// do React; a janela em si fica parada, transparente e sem capturar cliques.
fn overlay_hide(app: &AppHandle, gen: u64) {
    if app.state::<Arc<Shared>>().overlay_gen.load(Ordering::SeqCst) != gen {
        return;
    }
    let _ = app.emit_to("overlay", "overlay_hide", ());
}

fn overlay_status(app: &AppHandle, state: &str, message: &str) {
    let _ = app.emit_to(
        "overlay",
        "overlay_status",
        serde_json::json!({ "state": state, "message": message }),
    );
}

/// mensagem técnica -> frase curta que cabe no overlay
fn friendly_error(e: &str) -> String {
    let low = e.to_lowercase();
    let msg = if low.contains("microfone") || low.contains("sem microfone") {
        "Microfone não encontrado"
    } else if low.contains("429") || low.contains("rate limit") || low.contains("quota") {
        "Limite da API atingido"
    } else if low.contains("401") || low.contains("403") || low.contains("chave") {
        "Chave de API inválida ou ausente"
    } else if low.contains("timed out")
        || low.contains("timeout")
        || low.contains("dns")
        || low.contains("connect")
    {
        "Sem conexão com o serviço"
    } else if low.contains("transcrição local") {
        "Transcrição local indisponível"
    } else if low.contains("alucinação") || low.contains("transcrição vazia") {
        "Nada foi dito"
    } else if low.contains("clipboard") || low.contains("área de transferência") {
        "Não consegui colar o texto"
    } else {
        return e.chars().take(70).collect();
    };
    msg.to_string()
}

/// mostra o motivo da falha por alguns segundos — sem isso o ditado some em silêncio
fn overlay_fail(app: &AppHandle, gen: u64, err: &str) {
    if app.state::<Arc<Shared>>().overlay_gen.load(Ordering::SeqCst) != gen {
        return;
    }
    overlay_status(app, "error", &friendly_error(err));
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(3200));
        overlay_hide(&app, gen);
    });
}

/// cada toast recebe um número; só o mais recente pode se apagar sozinho
static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// balão discreto acima do overlay, dizendo qual perfil está em uso
fn toast_show(app: &AppHandle, text: &str) {
    let Some(w) = app.get_webview_window("toast") else {
        overlay_log("janela toast inexistente");
        return;
    };
    let mon = w
        .cursor_position()
        .ok()
        .and_then(|p| w.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| w.primary_monitor().ok().flatten());
    if let (Some(mon), Ok(sz)) = (mon, w.outer_size()) {
        let m = mon.size();
        let x = mon.position().x + (m.width as i32 - sz.width as i32) / 2;
        // acima do overlay das ondas, para os dois poderem aparecer juntos
        let y = mon.position().y + m.height as i32 - sz.height as i32 - 128;
        let _ = w.set_position(PhysicalPosition::new(x, y));
    }
    let _ = w.set_always_on_top(true);
    let _ = w.show();
    let _ = app.emit_to("toast", "toast", text.to_string());

    let seq = TOAST_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1600));
        if TOAST_SEQ.load(Ordering::SeqCst) == seq {
            if let Some(w) = app.get_webview_window("toast") {
                let _ = w.hide();
            }
        }
    });
}

/// avança o perfil ativo e avisa na tela
fn cycle_profile(shared: &Arc<Shared>) {
    let s = {
        let mut g = shared.settings.lock().unwrap();
        if g.profiles.is_empty() {
            return;
        }
        let i = g.profiles.iter().position(|p| p.name == g.active_profile).unwrap_or(0);
        g.active_profile = g.profiles[(i + 1) % g.profiles.len()].name.clone();
        g.clone()
    };
    let _ = persist_settings(&s);
    println!("[hotkey] perfil ativo: {}", s.active_profile);
    if let Some(app) = shared.app.get() {
        rebuild_tray_menu(app, &s);
        let _ = app.emit("settings_changed", ());
        toast_show(app, &s.active_profile);
    }
}

enum Cmd {
    /// `Some(perfil)` dita naquele estilo sem mexer no perfil ativo
    Start(Option<String>),
    Stop,
    Cancel,
}

fn spawn_pipeline(shared: Arc<Shared>, app: AppHandle) -> Sender<Cmd> {
    let (tx, rx) = channel::<Cmd>();
    std::thread::spawn(move || {
        let mut rec = Recorder::new();
        // perfil pedido pelo atalho que iniciou a gravação em curso
        let mut pending_profile: Option<String> = None;
        let mut gen = 0u64;
        loop {
            match rx.recv() {
                Ok(Cmd::Start(profile)) => {
                    pending_profile = profile;
                    gen = overlay_show(&app);
                    let mic = shared.settings.lock().unwrap().mic.clone();
                    let app_lvl = app.clone();
                    let mut last = Instant::now() - Duration::from_secs(1);
                    let on_level: LevelFn = Box::new(move |rms| {
                        if last.elapsed() >= Duration::from_millis(40) {
                            last = Instant::now();
                            let _ = app_lvl.emit("level", rms);
                        }
                    });
                    if let Err(e) = rec.start(mic.as_deref(), on_level) {
                        eprintln!("[pipeline] gravação falhou: {e}");
                        overlay_fail(&app, gen, &e);
                    } else {
                        println!("[pipeline] gravando...");
                    }
                }
                Ok(Cmd::Cancel) => {
                    let _ = rec.stop();
                    overlay_hide(&app, gen);
                    println!("[pipeline] ditado cancelado");
                }
                Ok(Cmd::Stop) => {
                    // o overlay só sai depois da colagem: some antes = parece ter falhado
                    let forced_profile = pending_profile.take();
                    let (samples, rate) = rec.stop();
                    let secs = samples.len() as f32 / rate as f32;
                    if secs < 0.4 {
                        println!("[pipeline] áudio curto demais ({secs:.1}s), ignorado");
                        overlay_hide(&app, gen);
                        continue;
                    }
                    let peak = peak_rms(&samples, rate);
                    if peak < 0.006 {
                        println!("[pipeline] sem fala detectada (pico rms {peak:.4}), ignorado");
                        overlay_hide(&app, gen);
                        continue;
                    }
                    // overlay segue no ar em "processando": soltar o atalho e ver a
                    // janela sumir por 2-3s é indistinguível de ter falhado
                    overlay_status(&app, "processing", "");
                    // processamento em thread própria: a thread do atalho fica livre e o
                    // próximo Start é imediato (overlay e gravação sem atraso de fila)
                    let s = shared.settings.lock().unwrap().clone();
                    let profile = forced_profile.unwrap_or_else(|| s.active_profile.clone());
                    let shared_w = shared.clone();
                    let app_w = app.clone();
                    let gen_w = gen;
                    std::thread::spawn(move || {
                        let t0 = Instant::now();
                        let result = run_stt(wav_bytes(&samples, rate), &shared_w, &s)
                            .and_then(|raw| {
                                if raw.is_empty() {
                                    return Err("transcrição vazia".into());
                                }
                                if is_hallucination(&raw, secs) {
                                    return Err(format!("alucinação de silêncio descartada: {raw:?}"));
                                }
                                let t_stt = t0.elapsed().as_secs_f32();
                                let final_text = match rewrite(&raw, &s, &profile) {
                                    Ok(t) => t,
                                    Err(e) => {
                                        eprintln!("[pipeline] reescrita falhou ({e}); usando texto bruto");
                                        raw.clone()
                                    }
                                };
                                let final_text = apply_snippets(&final_text, &s.snippets);
                                let total = t0.elapsed().as_secs_f32();
                                println!("[pipeline] áudio {secs:.1}s | stt {t_stt:.2}s | total {total:.2}s");
                                if s.history_enabled {
                                    append_history(&HistoryEntry {
                                        ts: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                                        profile: profile.clone(),
                                        raw,
                                        r#final: final_text.clone(),
                                        audio_secs: secs,
                                        stt_secs: t_stt,
                                        total_secs: total,
                                    });
                                }
                                *shared_w.last_text.lock().unwrap() = final_text.clone();
                                let _serial = shared_w.insert_lock.lock().unwrap();
                                insert_text(&final_text)
                            });
                        match result {
                            Ok(()) => overlay_hide(&app_w, gen_w),
                            Err(e) => {
                                eprintln!("[pipeline] erro: {e}");
                                overlay_fail(&app_w, gen_w, &e);
                            }
                        }
                    });
                }
                Err(_) => break,
            }
        }
    });
    tx
}

// Estado compartilhado do rastreador de atalho: alimentado pelo hook LL no
// Windows e pelo CGEventTap no macOS (ambos em platform::).
struct HookState {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    pressed: HashSet<u32>,
    /// atalho fisicamente pressionado agora — enquanto existe, os outros são ignorados
    latched: Option<String>,
    /// atalho que iniciou a gravação em curso (só ele pode encerrá-la)
    recording: Option<String>,
}

fn binding_satisfied(b: &Binding, pressed: &HashSet<u32>) -> bool {
    !b.groups.is_empty() && b.groups.iter().all(|g| g.iter().any(|k| pressed.contains(k)))
}

fn handle_key(st: &mut HookState, vk: u32, down: bool) {
    if down {
        st.pressed.insert(vk);
    } else {
        st.pressed.remove(&vk);
    }

    // aborta o ditado em andamento sem inserir nada. No modo hold o atalho segue
    // pressionado, mas `latched` continua apontando para ele: ao soltar, o Release
    // não acha mais a gravação e não dispara Stop.
    if down && st.recording.is_some() && *st.shared.cancel.lock().unwrap() == Some(vk) {
        st.recording = None;
        let _ = st.tx.send(Cmd::Cancel);
        return;
    }

    enum Step {
        Nothing,
        /// o atalho pressionado foi solto; `hold` diz se isso encerra a gravação
        Release { hold: bool },
        Fire { id: String, act: Act },
    }

    // decide com o cadeado dos vínculos na mão e age depois de soltá-lo:
    // agir aqui dentro travaria junto com o cadeado das configurações
    let step = {
        let bindings = st.shared.bindings.lock().unwrap();
        match &st.latched {
            Some(id) => match bindings.iter().find(|b| &b.id == id) {
                Some(b) if binding_satisfied(b, &st.pressed) => Step::Nothing,
                Some(b) => Step::Release {
                    hold: matches!(b.act, Act::Record { toggle: false, .. }),
                },
                // o atalho sumiu (configuração mudou no meio): encerra o que estiver aberto
                None => Step::Release { hold: true },
            },
            // a lista vem do mais específico para o mais genérico
            None => match bindings.iter().find(|b| binding_satisfied(b, &st.pressed)) {
                Some(b) => Step::Fire { id: b.id.clone(), act: b.act.clone() },
                None => Step::Nothing,
            },
        }
    };

    match step {
        Step::Nothing => {}
        Step::Release { hold } => {
            let id = st.latched.take();
            if hold && st.recording == id {
                st.recording = None;
                let _ = st.tx.send(Cmd::Stop);
            }
        }
        Step::Fire { id, act } => {
            st.latched = Some(id.clone());
            match act {
                // fora da thread do hook: o WH_KEYBOARD_LL é removido pelo Windows se o
                // callback demorar, e isto grava arquivo e mexe em janela
                Act::CycleProfile => {
                    let shared = st.shared.clone();
                    std::thread::spawn(move || cycle_profile(&shared));
                }
                Act::Record { toggle, profile } => {
                    if st.recording.is_some() {
                        // gravando: só o mesmo atalho, em modo apertar/parar, encerra
                        if st.recording.as_deref() == Some(id.as_str()) && toggle {
                            st.recording = None;
                            let _ = st.tx.send(Cmd::Stop);
                        }
                        return;
                    }
                    println!("[hotkey] {id} ativado");
                    if let (Some(p), Some(app)) = (profile.clone(), st.shared.app.get()) {
                        let app = app.clone();
                        std::thread::spawn(move || toast_show(&app, &p));
                    }
                    st.recording = Some(id);
                    let _ = st.tx.send(Cmd::Start(profile));
                }
            }
        }
    }
}

#[tauri::command]
fn get_settings(state: tauri::State<Arc<Shared>>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let m = app.autolaunch();
    if enabled {
        m.enable().map_err(|e| e.to_string())
    } else {
        m.disable().map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: tauri::State<Arc<Shared>>,
    settings: Settings,
) -> Result<(), String> {
    persist_settings(&settings)?;
    *state.bindings.lock().unwrap() = build_bindings(&settings);
    *state.cancel.lock().unwrap() = parse_cancel_key(&settings.cancel_key);
    let warm_local = settings.stt_provider == "local";
    rebuild_tray_menu(&app, &settings);
    *state.settings.lock().unwrap() = settings;
    if warm_local {
        let shared = state.inner().clone();
        std::thread::spawn(move || {
            let s = shared.settings.lock().unwrap().clone();
            if let Err(e) = ensure_sidecar(&shared, &s) {
                eprintln!("[stt] falha ao aquecer local: {e}");
            }
        });
    }
    Ok(())
}

#[tauri::command]
fn list_mics() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

#[tauri::command]
fn get_history(query: Option<String>, limit: Option<usize>) -> Vec<HistoryEntry> {
    let q = query.unwrap_or_default().to_lowercase();
    let limit = limit.unwrap_or(50);
    let Ok(txt) = std::fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    txt.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .filter(|e| {
            q.is_empty()
                || e.r#final.to_lowercase().contains(&q)
                || e.raw.to_lowercase().contains(&q)
        })
        .take(limit)
        .collect()
}

#[tauri::command]
fn clear_history() -> Result<(), String> {
    match std::fs::remove_file(history_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Serialize)]
struct Diagnostics {
    stt_provider: String,
    groq_key_set: bool,
    sidecar_ok: bool,
    sidecar_configured: bool,
    sidecar_device: String,
    default_mic: String,
    gemini_key_set: bool,
    gemini_model: String,
    recent: Vec<HistoryEntry>,
}

#[tauri::command]
fn get_diagnostics(state: tauri::State<Arc<Shared>>) -> Diagnostics {
    let s = state.settings.lock().unwrap().clone();
    let (sidecar_ok, sidecar_device) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()
        .and_then(|c| c.get(format!("http://127.0.0.1:{}/health", s.stt_port)).send().ok())
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .map(|v| (true, v["device"].as_str().unwrap_or("?").to_string()))
        .unwrap_or((false, "offline".into()));
    let default_mic = cpal::default_host()
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_else(|| "nenhum".into());
    // sem os dois caminhos no disco o modo local nunca sobe — e "descarregado"
    // parece normal, escondendo que ele simplesmente não existe nesta máquina
    let sidecar_configured =
        std::path::Path::new(&s.python).exists() && std::path::Path::new(&s.sidecar).exists();
    Diagnostics {
        stt_provider: s.stt_provider.clone(),
        groq_key_set: s.groq_ready(),
        sidecar_ok,
        sidecar_configured,
        sidecar_device,
        default_mic,
        gemini_key_set: s.gemini_api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false)
            || std::env::var("GEMINI_API_KEY").is_ok(),
        gemini_model: s.gemini_model.clone(),
        recent: get_history(None, Some(10)),
    }
}

fn build_tray_menu(app: &AppHandle, s: &Settings) -> tauri::Result<Menu<tauri::Wry>> {
    let items: Vec<CheckMenuItem<tauri::Wry>> = s
        .profiles
        .iter()
        .map(|p| {
            CheckMenuItem::with_id(
                app,
                format!("profile:{}", p.name),
                &p.name,
                true,
                p.name == s.active_profile,
                None::<&str>,
            )
        })
        .collect::<Result<_, _>>()?;
    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        items.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    let perfil = Submenu::with_items(app, "Perfil", true, &refs)?;
    let copy_last =
        MenuItem::with_id(app, "copy_last", "Copiar último ditado", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Configurações", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Sair", true, None::<&str>)?;
    Menu::with_items(app, &[&perfil, &copy_last, &show, &quit])
}

fn rebuild_tray_menu(app: &AppHandle, s: &Settings) {
    if let (Some(tray), Ok(menu)) = (app.tray_by_id("tray"), build_tray_menu(app, s)) {
        let _ = tray.set_menu(Some(menu));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mode` deixou de ser um campo: agora o comportamento vem de qual atalho
    /// disparou. "hold" usa `hotkey`, "toggle" usa `hotkey_toggle`.
    fn hook(mode: &str, cancel: &str) -> (HookState, std::sync::mpsc::Receiver<Cmd>) {
        let mut s = Settings::default();
        if mode == "toggle" {
            s.hotkey_toggle = std::mem::take(&mut s.hotkey);
        }
        s.cancel_key = cancel.into();
        let shared = Arc::new(Shared {
            bindings: Mutex::new(build_bindings(&s)),
            cancel: Mutex::new(parse_cancel_key(cancel)),
            sidecar: Mutex::new(None),
            settings: Mutex::new(s),
            insert_lock: Mutex::new(()),
            app: OnceLock::new(),
            last_text: Mutex::new(String::new()),
            overlay_gen: AtomicU64::new(0),
        });
        let (tx, rx) = channel::<Cmd>();
        (HookState { shared, tx, pressed: HashSet::new(), latched: None, recording: None }, rx)
    }

    fn key(name: &str) -> u32 {
        platform::key_from_name(name).expect("tecla conhecida")
    }

    /// modificadores vêm de modifier_group (têm variante esquerda e direita)
    fn modkey(name: &str) -> u32 {
        platform::modifier_group(name).expect("modificador conhecido")[0]
    }

    /// primeiro grupo do atalho de ditado (Ctrl) e segundo (Win/Cmd)
    fn hotkey_codes(st: &HookState) -> (u32, u32) {
        let b = st.shared.bindings.lock().unwrap();
        let g = &b[0].groups;
        (g[0][0], g[1][0])
    }

    #[test]
    fn cancelar_no_hold_nao_dispara_stop_ao_soltar() {
        let (mut st, rx) = hook("hold", "SPACE");
        let (ctrl, win) = hotkey_codes(&st);
        let space = key("SPACE");

        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));

        handle_key(&mut st, space, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Cancel)));

        // soltar o atalho depois do cancelamento não pode mandar o áudio para o STT
        handle_key(&mut st, space, false);
        handle_key(&mut st, win, false);
        handle_key(&mut st, ctrl, false);
        assert!(rx.try_recv().is_err(), "Stop fantasma depois de cancelar");

        // e o próximo ditado continua funcionando
        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));
    }

    #[test]
    fn cancelar_no_toggle_permite_novo_ditado() {
        let (mut st, rx) = hook("toggle", "ESC");
        let (ctrl, win) = hotkey_codes(&st);
        let esc = key("ESC");

        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        handle_key(&mut st, win, false);
        handle_key(&mut st, ctrl, false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));

        handle_key(&mut st, esc, true);
        handle_key(&mut st, esc, false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Cancel)));

        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))), "toggle travado depois do cancelamento");
    }

    #[test]
    fn cancelamento_desativado_ignora_a_tecla() {
        let (mut st, rx) = hook("hold", "");
        let (ctrl, win) = hotkey_codes(&st);
        handle_key(&mut st, ctrl, true);
        handle_key(&mut st, win, true);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(_))));

        handle_key(&mut st, key("SPACE"), true);
        assert!(rx.try_recv().is_err());

        handle_key(&mut st, win, false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)));
    }

    /// monta um HookState com atalhos arbitrários, para os cenários de vários vínculos
    fn hook_com(ajusta: impl FnOnce(&mut Settings)) -> (HookState, std::sync::mpsc::Receiver<Cmd>) {
        let mut s = Settings::default();
        ajusta(&mut s);
        let shared = Arc::new(Shared {
            bindings: Mutex::new(build_bindings(&s)),
            cancel: Mutex::new(parse_cancel_key(&s.cancel_key)),
            sidecar: Mutex::new(None),
            settings: Mutex::new(s),
            insert_lock: Mutex::new(()),
            app: OnceLock::new(),
            last_text: Mutex::new(String::new()),
            overlay_gen: AtomicU64::new(0),
        });
        let (tx, rx) = channel::<Cmd>();
        (HookState { shared, tx, pressed: HashSet::new(), latched: None, recording: None }, rx)
    }

    #[test]
    fn atalho_de_perfil_dita_naquele_estilo_sem_trocar_o_ativo() {
        let (mut st, rx) = hook_com(|s| {
            s.hotkey = "Ctrl+Win".into();
            s.profiles[1].hotkey = "Ctrl+Alt+J".into();
        });
        let alvo = st.shared.settings.lock().unwrap().profiles[1].name.clone();

        handle_key(&mut st, modkey("CTRL"), true);
        handle_key(&mut st, modkey("ALT"), true);
        handle_key(&mut st, key("J"), true);
        match rx.try_recv() {
            Ok(Cmd::Start(Some(p))) => assert_eq!(p, alvo, "ditou no perfil errado"),
            Ok(Cmd::Start(None)) => panic!("Start sem perfil: o vínculo do perfil não foi usado"),
            _ => panic!("esperava Start com o perfil do atalho"),
        }
        // o perfil ativo não pode ter mudado
        assert_eq!(st.shared.settings.lock().unwrap().active_profile, "Bruto corrigido");

        handle_key(&mut st, key("J"), false);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Stop)));
    }

    #[test]
    fn atalho_mais_especifico_ganha_do_mais_curto() {
        // Ctrl+Win e Ctrl+Win+J juntos: completar a combinação maior não pode
        // acionar a menor
        let (mut st, rx) = hook_com(|s| {
            s.hotkey = "Ctrl+Win".into();
            s.profiles[1].hotkey = "Ctrl+Win+J".into();
        });
        let b = st.shared.bindings.lock().unwrap();
        assert!(b[0].groups.len() >= b[b.len() - 1].groups.len(), "lista não está do mais específico para o mais genérico");
        drop(b);

        handle_key(&mut st, modkey("CTRL"), true);
        handle_key(&mut st, modkey("WIN"), true);
        handle_key(&mut st, key("J"), true);
        // o primeiro a ser satisfeito foi Ctrl+Win, e ele fica travado até soltar
        assert!(matches!(rx.try_recv(), Ok(Cmd::Start(None))));
        assert!(rx.try_recv().is_err(), "dois Start para um gesto só");
    }

    #[test]
    fn atalho_de_ditado_e_de_troca_de_perfil_nao_se_misturam() {
        let (mut st, rx) = hook_com(|s| {
            s.hotkey = "Ctrl+Win".into();
            s.hotkey_cycle_profile = "Ctrl+Alt+P".into();
        });
        handle_key(&mut st, modkey("CTRL"), true);
        handle_key(&mut st, modkey("ALT"), true);
        handle_key(&mut st, key("P"), true);
        // trocar de perfil não grava áudio
        assert!(rx.try_recv().is_err(), "atalho de perfil disparou gravação");
    }

    #[test]
    fn atalho_invalido_e_descartado_em_vez_de_virar_outra_tecla() {
        assert!(parse_hotkey("Ctrl+Tecla_Que_Nao_Existe").is_none());
        assert!(parse_hotkey("").is_none());
        assert!(parse_hotkey("Ctrl+Win").is_some());
        // cancelamento com tecla desconhecida desliga, não vira F9
        assert!(parse_cancel_key("xyz").is_none());
        assert!(parse_cancel_key("").is_none());
        assert_eq!(parse_cancel_key("ESC"), platform::key_from_name("ESC"));
    }

    #[test]
    fn atalho_duplicado_nao_cria_dois_vinculos() {
        let mut s = Settings::default();
        s.hotkey = "Ctrl+Win".into();
        s.hotkey_toggle = "Ctrl+Win".into();
        assert_eq!(build_bindings(&s).len(), 1, "o mesmo atalho ficou em dois vínculos");
    }

    #[test]
    fn migracao_do_modo_toggle_preserva_o_comportamento() {
        // versões antigas guardavam o modo num campo à parte
        let antigo = r#"{"hotkey":"Ctrl+Win","mode":"toggle"}"#;
        let mut s: Settings = serde_json::from_str(antigo).unwrap();
        if s.mode == "toggle" && s.hotkey_toggle.is_empty() && !s.hotkey.is_empty() {
            s.hotkey_toggle = std::mem::take(&mut s.hotkey);
        }
        assert_eq!(s.hotkey_toggle, "Ctrl+Win");
        assert!(s.hotkey.is_empty());
        let b = build_bindings(&s);
        assert_eq!(b.len(), 1);
        assert!(matches!(b[0].act, Act::Record { toggle: true, profile: None }));
    }

    #[test]
    fn provedor_de_reescrita_migra_para_gemini() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.rewrite_provider, "gemini", "quem já usava não pode mudar de provedor sozinho");
    }

    #[test]
    fn erros_viram_frases_curtas() {
        assert_eq!(friendly_error("Groq HTTP 401: {\"error\":...}"), "Chave de API inválida ou ausente");
        assert_eq!(friendly_error("Groq HTTP 429: rate limit"), "Limite da API atingido");
        assert_eq!(friendly_error("microfone 'Yeti' não encontrado"), "Microfone não encontrado");
        assert_eq!(friendly_error("transcrição vazia"), "Nada foi dito");
        // sem regra conhecida: mostra o original, curto o bastante para caber
        assert!(friendly_error(&"x".repeat(200)).chars().count() <= 70);
    }
}

pub fn run() {
    let (settings, first_run) = load_settings();
    // migra chaves em texto plano para DPAPI na primeira oportunidade
    let _ = persist_settings(&settings);
    // com Groq configurado como provedor, a GPU fica livre: o local só aquece sob demanda
    let start_local = settings.stt_provider != "groq" || !settings.groq_ready();
    let shared = Arc::new(Shared {
        bindings: Mutex::new(build_bindings(&settings)),
        cancel: Mutex::new(parse_cancel_key(&settings.cancel_key)),
        sidecar: Mutex::new(if start_local { platform::spawn_sidecar(&settings) } else { None }),
        settings: Mutex::new(settings.clone()),
        insert_lock: Mutex::new(()),
        app: OnceLock::new(),
        last_text: Mutex::new(String::new()),
        overlay_gen: AtomicU64::new(0),
    });

    let shared_exit = shared.clone();
    let shared_setup = shared.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // no boot o app pode subir duas vezes (autostart do registro + atalho em
            // shell:startup, ou o "reabrir apps ao entrar" do Windows). A segunda cai
            // aqui e abria as configurações na cara do usuário — quem vem do boot só sai.
            if args.iter().any(|a| a == "--autostart") {
                return;
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            list_mics,
            get_history,
            clear_history,
            get_diagnostics,
            get_autostart,
            set_autostart
        ])
        .setup(move |app| {
            // app de bandeja: no mac o ícone vive só na menu bar, sem Dock
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // impede o App Nap: sem janela visível o macOS suspende o processo e o
            // event tap do atalho para de responder até o app "acordar"
            #[cfg(target_os = "macos")]
            unsafe {
                use objc2::runtime::AnyObject;
                use objc2::{class, msg_send};
                let pi: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
                let reason: *mut AnyObject = msg_send![
                    class!(NSString),
                    stringWithUTF8String: c"hotkey e ditado globais".as_ptr()
                ];
                // NSActivityUserInitiatedAllowingIdleSystemSleep
                let opts: usize = 0x00FF_FFFF & !(1 << 20);
                let token: *mut AnyObject =
                    msg_send![pi, beginActivityWithOptions: opts, reason: reason];
                let _: *mut AnyObject = msg_send![token, retain]; // token vive para sempre
            }

            {
                use tauri_plugin_autostart::ManagerExt;
                let al = app.autolaunch();
                // re-habilitar reescreve a chave do registro com os args atuais
                // (--autostart), inclusive em quem já tinha o autostart de versões antigas
                if first_run || al.is_enabled().unwrap_or(false) {
                    let _ = al.enable();
                }
            }
            // o hook de teclado troca perfil e mostra o balão: precisa do handle
            let _ = shared_setup.app.set(app.handle().clone());
            let tx = spawn_pipeline(shared_setup.clone(), app.handle().clone());
            platform::spawn_hotkey_listener(shared_setup.clone(), tx);

            if let Some(o) = app.get_webview_window("overlay") {
                let _ = o.set_ignore_cursor_events(true);
                // Spaces: sem isto o overlay só existe na mesa onde nasceu (a das
                // configurações) e "some" quando se dita em outra mesa/tela cheia.
                // CanJoinAllSpaces (1<<0) | Stationary (1<<4) | FullScreenAuxiliary (1<<8)
                #[cfg(target_os = "macos")]
                if let Ok(nsw) = o.ns_window() {
                    let nsw = nsw as *mut objc2::runtime::AnyObject;
                    let behavior: usize = (1 << 0) | (1 << 4) | (1 << 8);
                    unsafe {
                        let _: () = objc2::msg_send![nsw, setCollectionBehavior: behavior];
                    }
                }
            }

            let menu_settings = shared_setup.settings.lock().unwrap().clone();
            let menu = build_tray_menu(app.handle(), &menu_settings)?;
            let tray = TrayIconBuilder::with_id("tray");
            // menu bar do mac usa ícone template: só o glifo da onda, monocromático,
            // e o sistema o adapta ao estilo/tema da barra (como os ícones nativos)
            #[cfg(target_os = "macos")]
            let tray = tray
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?)
                .icon_as_template(true);
            #[cfg(not(target_os = "macos"))]
            let tray = tray.icon(app.default_window_icon().unwrap().clone());
            tray.menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .tooltip("Open Flow — segure o atalho para ditar")
                .on_menu_event(move |app, event| {
                    if let Some(name) = event.id.as_ref().strip_prefix("profile:") {
                        let shared = app.state::<Arc<Shared>>();
                        let s = {
                            let mut g = shared.settings.lock().unwrap();
                            g.active_profile = name.to_string();
                            g.clone()
                        };
                        let _ = persist_settings(&s);
                        rebuild_tray_menu(app, &s);
                        let _ = app.emit("settings_changed", ());
                        return;
                    }
                    match event.id.as_ref() {
                        "copy_last" => {
                            let text = app.state::<Arc<Shared>>().last_text.lock().unwrap().clone();
                            if !text.is_empty() {
                                if let Ok(mut c) = arboard::Clipboard::new() {
                                    let _ = c.set_text(text);
                                }
                            }
                        }
                        "show" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("erro ao iniciar o Open Flow")
        .run(move |_app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(child) = shared_exit.sidecar.lock().unwrap().as_mut() {
                    let _ = child.kill();
                }
            }
        });
}
