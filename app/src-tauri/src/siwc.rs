//! Sign in with ChatGPT — gastar a cota do plano do usuário na Responses API.
//!
//! Só o caminho de cliente open-source/local, que a OpenAI abriu sem lista de
//! espera: a primeira autorização usa `client_id=dynamic_agent_client` e o
//! servidor emite um `client_id` próprio no retorno. Não há client secret.
//!
//! **Não serve para transcrição.** A documentação é explícita: "Audio/video
//! input, the Files upload API, and the transcription API are not supported by
//! this flow". Quem transcreve continua sendo o provedor de chave.
//!
//! Tudo aqui foi verificado contra o servidor real antes de virar código, com o
//! protótipo em `open-flow-instalar/siwc-prototipo.py`. Três coisas só
//! apareceram ali:
//!
//! - o `aud` do ID token é **lista**, não string;
//! - `/v1/models` devolve `{"models":[...]}` com `slug`/`display_name`, e não
//!   o `{"data":[...]}` do dialeto comum;
//! - abrir o navegador por `cmd /c start` quebra a URL em cada `&`, e o
//!   servidor recusa com `invalid_authorize_request`.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{config_dir, decrypt_key, encrypt_key};

const AUTH: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const USO_DO_PLANO: &str = "chatgpt.tokens.use.direct";

/// Porta do exemplo da documentação. Só a porta pode variar entre entradas; o
/// esquema, o host e o caminho ficam fixos, e `/callback` não casa com
/// `/auth/callback`.
const PORTA_PREFERIDA: u16 = 1455;
const CAMINHO: &str = "/auth/callback";

/// Tempo que o listener espera o usuário autorizar no navegador.
const ESPERA_CALLBACK: Duration = Duration::from_secs(300);

/// Renova com folga: um ditado que esbarra na renovação vira latência visível.
const FOLGA_RENOVACAO: u64 = 300;

fn b64url() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

fn agora() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn aleatorio(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("fonte de aleatoriedade do sistema");
    b64url().encode(buf)
}

// ---------------------------------------------------------------- credenciais

/// Guardadas fora do `settings.json` de propósito: o settings vai inteiro para
/// o frontend em `load_settings`, e token de acesso não pode viajar para lá.
/// Renovar também não deve reescrever o arquivo de configuração inteiro.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Credenciais {
    /// o `oaiapp_...` emitido no primeiro registro; `dynamic_agent_client` nunca
    /// é salvo aqui
    pub client_id: String,
    /// identificador opaco desta máquina, estável entre reinstalações
    pub host_id: String,
    pub email: String,
    /// guardados com o mesmo prefixo `enc:` das chaves de API
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    /// epoch em segundos
    pub expira_em: u64,
    pub scopes: Vec<String>,
}

impl Credenciais {
    pub fn usa_o_plano(&self) -> bool {
        self.scopes.iter().any(|s| s == USO_DO_PLANO)
    }
    pub fn conectado(&self) -> bool {
        !self.client_id.is_empty() && self.refresh_token.is_some()
    }
}

fn caminho() -> PathBuf {
    config_dir().join("chatgpt.json")
}

pub fn carregar() -> Credenciais {
    std::fs::read_to_string(caminho())
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

fn gravar(c: &Credenciais) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let txt = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    std::fs::write(caminho(), txt).map_err(|e| e.to_string())
}

/// Host ID estável desta máquina. Precisa ser opaco — a documentação proíbe
/// e-mail, ID de usuário ou qualquer coisa que identifique a pessoa. Reinstalar
/// o app não deve criar host novo, por isso ele nasce junto da primeira
/// credencial e só é regerado se sumir.
fn host_id(atual: &str) -> String {
    if !atual.is_empty() {
        return atual.to_string();
    }
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("fonte de aleatoriedade do sistema");
    b[6] = (b[6] & 0x0f) | 0x40; // versão 4
    b[8] = (b[8] & 0x3f) | 0x80; // variante RFC 4122
    format!(
        "urn:uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

// ------------------------------------------------------------------- JWT

/// Lê o payload de um JWT **sem** validar a assinatura.
///
/// Decisão consciente: neste fluxo o `id_token` chega na resposta do token
/// endpoint, numa chamada HTTPS que o próprio app faz para `auth.openai.com` —
/// não passa pelo navegador. O canal já autentica o emissor, então verificar a
/// assinatura contra o JWKS custaria uma dependência de RSA sem acrescentar
/// defesa real aqui. O que importa e **é** conferido: emissor, audiência,
/// expiração e o `nonce` daquela tentativa.
///
/// Se algum dia o token passar a chegar pelo navegador, esta decisão deixa de
/// valer e a validação de assinatura vira obrigatória.
fn claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = b64url().decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// O `aud` vem como lista neste fluxo, mas o OIDC permite string. Aceita os dois.
fn audiencia_bate(c: &serde_json::Value, client_id: &str) -> bool {
    match c.get("aud") {
        Some(serde_json::Value::String(s)) => s == client_id,
        Some(serde_json::Value::Array(v)) => v.iter().any(|x| x.as_str() == Some(client_id)),
        _ => false,
    }
}

// ------------------------------------------------------------- listener

/// Devolve o caminho e a query do GET que o navegador mandou, respondendo uma
/// página de "pode fechar". Lê só a primeira linha: o resto do pedido não
/// interessa e esperar por ele travaria em navegador que não manda corpo.
fn receber(listener: &TcpListener) -> Result<String, String> {
    listener
        .set_nonblocking(false)
        .map_err(|e| format!("listener: {e}"))?;
    let (mut conn, _) = listener.accept().map_err(|e| format!("retorno não chegou: {e}"))?;
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();

    let mut leitor = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
    let mut primeira = String::new();
    leitor.read_line(&mut primeira).map_err(|e| e.to_string())?;

    responder_pagina(&mut conn);
    let alvo = primeira.split_whitespace().nth(1).unwrap_or("").to_string();
    if alvo.is_empty() {
        return Err("o navegador não mandou um pedido GET válido".into());
    }
    Ok(alvo)
}

fn responder_pagina(conn: &mut TcpStream) {
    const CORPO: &str = concat!(
        "<!doctype html><meta charset=utf-8>",
        "<title>Open Flow</title>",
        "<body style=\"font:16px system-ui;padding:3rem;color:#1e1e22;background:#f6f6f3\">",
        "<h2>Open Flow conectado</h2><p>Pode fechar esta aba.</p>"
    );
    let _ = write!(
        conn,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        CORPO.len(),
        CORPO
    );
    let _ = conn.flush();
}

fn query(alvo: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    let q = match alvo.split_once('?') {
        Some((_, q)) => q,
        None => return m,
    };
    for par in q.split('&') {
        if let Some((k, v)) = par.split_once('=') {
            m.insert(k.to_string(), percent_decode(v));
        }
    }
    m
}

fn percent_decode(s: &str) -> String {
    let b = s.replace('+', " ");
    let b = b.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(&b[i + 1..i + 3]), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ------------------------------------------------------------- autorização

#[derive(Serialize, Clone)]
pub struct Conta {
    pub email: String,
    pub usa_o_plano: bool,
    pub scopes: Vec<String>,
}

#[derive(Deserialize)]
struct RespostaToken {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

/// Fluxo completo: abre o navegador, espera o retorno, troca o código e grava.
/// Bloqueia até cinco minutos — o Tauri roda comandos fora da thread da UI.
pub fn conectar<F: Fn(&str)>(abrir_navegador: F) -> Result<Conta, String> {
    let salvas = carregar();
    let host = host_id(&salvas.host_id);

    let state = aleatorio(32);
    let nonce = aleatorio(32);
    let verifier = aleatorio(48);
    let challenge = b64url().encode(Sha256::digest(verifier.as_bytes()));

    // O listener sobe ANTES do navegador: se o usuário autorizar rápido e não
    // houver quem atenda, o retorno se perde e o código morre sem uso.
    let listener = TcpListener::bind(("127.0.0.1", PORTA_PREFERIDA))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .map_err(|e| format!("não consegui abrir a porta de retorno: {e}"))?;
    let porta = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{porta}{CAMINHO}");

    // Registro novo usa `dynamic_agent_client` e manda o nome do app; reentrada
    // usa o client emitido e omite o nome, senão o servidor trata como outro
    // registro.
    let primeira_vez = salvas.client_id.is_empty();
    let mut url = format!(
        "{AUTH}?client_id={}&ext_agent_host_id={}&response_type=code\
         &redirect_uri={}&scope={}&resource={}&state={}&nonce={}\
         &code_challenge_method=S256&code_challenge={}",
        if primeira_vez { "dynamic_agent_client" } else { &salvas.client_id },
        encode(&host),
        encode(&redirect),
        encode(SCOPES),
        encode(RESOURCE),
        encode(&state),
        encode(&nonce),
        encode(&challenge),
    );
    if primeira_vez {
        url.push_str("&agent_name_hint=Open%20Flow");
    } else if let Some(id) = decrypt_key("openflow-chatgpt-id", &salvas.id_token) {
        // pula o seletor de contas e volta para a mesma conta
        url.push_str(&format!("&id_token_hint={}", encode(&id)));
    }

    abrir_navegador(&url);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(receber(&listener));
    });
    let alvo = rx
        .recv_timeout(ESPERA_CALLBACK)
        .map_err(|_| "ninguém autorizou em cinco minutos".to_string())??;

    let q = query(&alvo);
    if let Some(e) = q.get("error") {
        return Err(match e.as_str() {
            "access_denied" => "autorização recusada na tela do ChatGPT".into(),
            outro => format!("o ChatGPT recusou a autorização: {outro}"),
        });
    }
    if q.get("state").map(String::as_str) != Some(state.as_str()) {
        return Err("o retorno não bate com o pedido; tentativa descartada".into());
    }
    let code = q.get("code").ok_or("o retorno veio sem código")?;

    // Registro novo traz o client emitido; reentrada pode omitir, e aí vale o
    // que já estava salvo. Client diferente do salvo é recusado em vez de
    // substituir a conta por baixo.
    let client_id = match q.get("client_id") {
        Some(c) if primeira_vez => c.clone(),
        Some(c) if *c != salvas.client_id => {
            return Err("o ChatGPT devolveu outro aplicativo; nada foi alterado".into())
        }
        _ if primeira_vez => return Err("registro incompleto: o retorno veio sem client_id".into()),
        _ => salvas.client_id.clone(),
    };

    let tok: RespostaToken = crate::cloud_client()
        .post(TOKEN)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .map_err(|e| format!("troca do código falhou: {e}"))
        .and_then(resposta_ou_erro)?;

    let scopes: Vec<String> =
        tok.scope.unwrap_or_default().split_whitespace().map(String::from).collect();

    let mut email = String::new();
    if let Some(id) = &tok.id_token {
        let c = claims(id).ok_or("o ID token veio ilegível")?;
        if c.get("iss").and_then(|v| v.as_str()) != Some("https://auth.openai.com") {
            return Err("o ID token não veio do ChatGPT".into());
        }
        if !audiencia_bate(&c, &client_id) {
            return Err("o ID token foi emitido para outro aplicativo".into());
        }
        if c.get("nonce").and_then(|v| v.as_str()) != Some(nonce.as_str()) {
            return Err("o ID token não corresponde a este pedido".into());
        }
        email = c.get("email").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    }

    let cred = Credenciais {
        client_id,
        host_id: host,
        email: email.clone(),
        access_token: encrypt_key("openflow-chatgpt-at", &Some(tok.access_token)),
        refresh_token: encrypt_key("openflow-chatgpt-rt", &tok.refresh_token),
        id_token: encrypt_key("openflow-chatgpt-id", &tok.id_token),
        expira_em: agora() + tok.expires_in.unwrap_or(3600),
        scopes: scopes.clone(),
    };
    gravar(&cred)?;

    Ok(Conta { email, usa_o_plano: cred.usa_o_plano(), scopes })
}

pub fn desconectar() -> Result<(), String> {
    match std::fs::remove_file(caminho()) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn conta() -> Option<Conta> {
    let c = carregar();
    if !c.conectado() {
        return None;
    }
    Some(Conta { email: c.email.clone(), usa_o_plano: c.usa_o_plano(), scopes: c.scopes.clone() })
}

// --------------------------------------------------------------- renovação

/// Token de acesso válido, renovando quando falta pouco. O refresh é rotativo:
/// cada renovação devolve um substituto com 30 dias novos, e perder o novo
/// significa pedir autorização de novo — por isso a gravação vem antes do uso.
pub fn token_valido() -> Result<String, String> {
    let mut c = carregar();
    if !c.conectado() {
        return Err("o ChatGPT não está conectado na aba Provedores".into());
    }
    if c.expira_em > agora() + FOLGA_RENOVACAO {
        if let Some(t) = decrypt_key("openflow-chatgpt-at", &c.access_token) {
            return Ok(t);
        }
    }
    let rt = decrypt_key("openflow-chatgpt-rt", &c.refresh_token)
        .ok_or("a sessão do ChatGPT expirou; conecte de novo")?;

    let tok: RespostaToken = crate::cloud_client()
        .post(TOKEN)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", c.client_id.as_str()),
            ("refresh_token", rt.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .map_err(|e| format!("renovação falhou: {e}"))
        .and_then(resposta_ou_erro)?;

    let novo = tok.access_token.clone();
    c.access_token = encrypt_key("openflow-chatgpt-at", &Some(tok.access_token));
    if tok.refresh_token.is_some() {
        c.refresh_token = encrypt_key("openflow-chatgpt-rt", &tok.refresh_token);
    }
    if tok.id_token.is_some() {
        c.id_token = encrypt_key("openflow-chatgpt-id", &tok.id_token);
    }
    c.expira_em = agora() + tok.expires_in.unwrap_or(3600);
    if let Some(s) = tok.scope {
        c.scopes = s.split_whitespace().map(String::from).collect();
    }
    gravar(&c)?;
    Ok(novo)
}

fn resposta_ou_erro<T: serde::de::DeserializeOwned>(
    r: reqwest::blocking::Response,
) -> Result<T, String> {
    let status = r.status();
    if status.is_success() {
        return r.json::<T>().map_err(|e| format!("resposta ilegível: {e}"));
    }
    let corpo = r.text().unwrap_or_default();
    Err(format!("HTTP {}: {}", status.as_u16(), corpo.chars().take(300).collect::<String>()))
}

// ---------------------------------------------------------------- catálogo

/// Envelope próprio: `models`, com `slug` e `display_name`. Quem tratar como o
/// `{"data":[{"id":...}]}` dos outros provedores lista vazio e parece "sem
/// catálogo".
pub fn modelos() -> Result<Vec<String>, String> {
    let at = token_valido()?;
    let v: serde_json::Value = crate::cloud_client()
        .get(format!("{RESOURCE}/models"))
        .bearer_auth(at)
        .header("accept", "application/json")
        .send()
        .map_err(|e| e.to_string())
        .and_then(resposta_ou_erro)?;
    Ok(v["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| m["visibility"].as_str().map(|s| s == "list").unwrap_or(true))
                .filter_map(|m| m["slug"].as_str().or_else(|| m["id"].as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default())
}

// --------------------------------------------------------------- inferência

/// Reescrita pela Responses API gastando a cota do plano.
///
/// A rota impõe forma própria, e desobedecer dá 400 em vez de degradar:
/// `store:false` e `stream:true` obrigatórios; `temperature`, `top_p`,
/// `max_output_tokens` e `user` proibidos; item `{role:"system"}` recusado, por
/// isso a instrução vai em `instructions`.
///
/// `effort` vazio deixa o padrão do servidor. Medido em 07/10 no ditado clínico
/// de 2.872 caracteres: `luna` low em 18,3s e `terra` medium em 27,2s acertaram
/// as 7 checagens de fidelidade, enquanto `high` passou de 60s **e piorou** —
/// edição fiel não melhora com mais raciocínio, só ganha liberdade para
/// reinterpretar.
pub fn responder(
    modelo: &str,
    instrucao: &str,
    entrada: &str,
    effort: &str,
    teto: Duration,
) -> Result<String, String> {
    let at = token_valido()?;
    let mut corpo = serde_json::json!({
        "model": modelo,
        "instructions": instrucao,
        "input": [{ "role": "user", "content": entrada }],
        "store": false,
        "stream": true,
    });
    if !effort.is_empty() {
        corpo["reasoning"] = serde_json::json!({ "effort": effort });
    }

    let r = crate::cloud_client()
        .post(format!("{RESOURCE}/responses"))
        .bearer_auth(at)
        .header("accept", "text/event-stream")
        .timeout(teto)
        .json(&corpo)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                "__TIMEOUT__".to_string()
            } else {
                e.to_string()
            }
        })?;

    let status = r.status();
    if !status.is_success() {
        let corpo = r.text().unwrap_or_default();
        return Err(erro_legivel(status.as_u16(), &corpo));
    }
    ler_stream(r)
}

/// Junta os `response.output_text.delta` e só aceita o resultado depois de
/// `response.completed`. Erro de cota chega **depois** de o fluxo abrir, como
/// `response.failed` — quem assume sucesso no primeiro byte cola texto cortado.
fn ler_stream(r: reqwest::blocking::Response) -> Result<String, String> {
    let mut texto = String::new();
    let mut completou = false;
    let mut falha: Option<String> = None;

    for linha in BufReader::new(r).lines() {
        let linha = match linha {
            Ok(l) => l,
            // conexão cortada no meio: o que veio até aqui está incompleto
            Err(e) => return Err(format!("o fluxo da resposta foi interrompido: {e}")),
        };
        let dado = match linha.trim().strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };
        if dado == "[DONE]" {
            break;
        }
        let ev: serde_json::Value = match serde_json::from_str(dado) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match ev["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(d) = ev["delta"].as_str() {
                    texto.push_str(d);
                }
            }
            "response.completed" => completou = true,
            "response.failed" | "response.incomplete" | "error" => {
                let erro = if ev["response"]["error"].is_object() {
                    &ev["response"]["error"]
                } else {
                    &ev["error"]
                };
                falha = Some(erro_legivel(
                    0,
                    &serde_json::json!({ "error": erro }).to_string(),
                ));
            }
            _ => {}
        }
    }

    if let Some(f) = falha {
        return Err(f);
    }
    if !completou {
        return Err("a resposta terminou sem confirmação de conclusão".into());
    }
    Ok(texto.trim().to_string())
}

/// Traduz os códigos que a rota devolve. Mensagem crua de erro vira ruído no
/// balão, e aqui cada caso tem conduta diferente.
fn erro_legivel(status: u16, corpo: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(corpo).unwrap_or_default();
    let codigo = v["error"]["code"].as_str().unwrap_or_default();
    match codigo {
        "subscription_sharing_usage_limit_exceeded" => {
            "a cota do seu plano ChatGPT acabou por agora".into()
        }
        "subscription_sharing_user_not_eligible" => {
            "esta conta do ChatGPT não permite uso do plano por aplicativos".into()
        }
        "subscription_sharing_usage_unavailable" => {
            "o uso do plano ChatGPT está indisponível no momento".into()
        }
        _ => {
            // a admissão pode responder {"detail":"..."} sem o objeto `error`
            let detalhe = v["detail"]
                .as_str()
                .or_else(|| v["error"]["message"].as_str())
                .unwrap_or(corpo);
            let curto: String = detalhe.chars().take(200).collect();
            // status 0 = erro que veio dentro do fluxo, não de uma resposta HTTP
            if status == 0 {
                curto
            } else {
                format!("HTTP {status}: {curto}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vetor do RFC 7636, apêndice B: é o que prova que o desafio sai no formato
    /// que o servidor espera. Padding de `=` faz o servidor recusar.
    #[test]
    fn pkce_segue_o_vetor_do_rfc_7636() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let desafio = b64url().encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(desafio, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert!(!desafio.contains('='), "padding faz o servidor recusar");
    }

    /// O `aud` deste fluxo vem como lista; tratar só string rejeitaria toda
    /// entrada válida.
    #[test]
    fn audiencia_aceita_lista_e_string() {
        let id = "oaiapp_exemplo";
        let lista = serde_json::json!({ "aud": [id] });
        let texto = serde_json::json!({ "aud": id });
        let outro = serde_json::json!({ "aud": ["oaiapp_outro"] });
        assert!(audiencia_bate(&lista, id));
        assert!(audiencia_bate(&texto, id));
        assert!(!audiencia_bate(&outro, id), "aplicativo errado tem de ser recusado");
        assert!(!audiencia_bate(&serde_json::json!({}), id));
    }

    #[test]
    fn host_id_e_opaco_e_estavel() {
        let novo = host_id("");
        assert!(novo.starts_with("urn:uuid:"));
        assert_eq!(novo.len(), 9 + 36);
        assert_eq!(host_id(&novo), novo, "host existente nunca é regerado");
        assert_ne!(host_id(""), novo, "cada host tem o seu");
    }

    /// Erro de cota não pode chegar ao usuário como JSON cru, e cada código tem
    /// conduta diferente: cota acabada se resolve esperando, inelegibilidade não.
    #[test]
    fn erros_de_cota_viram_texto_legivel() {
        let cota = erro_legivel(
            429,
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#,
        );
        assert!(cota.contains("cota"), "{cota}");
        assert!(!cota.contains('{'), "JSON cru não pode vazar: {cota}");

        let ineleg = erro_legivel(
            403,
            r#"{"error":{"code":"subscription_sharing_user_not_eligible"}}"#,
        );
        assert!(ineleg.contains("não permite"), "{ineleg}");

        // admissão responde sem o objeto `error`
        let adm = erro_legivel(503, r#"{"detail":"direct routing unavailable"}"#);
        assert!(adm.contains("direct routing unavailable"), "{adm}");
    }

    #[test]
    fn query_do_callback_decodifica_porcento() {
        let q = query("/auth/callback?code=abc&state=x%2Dy&scope=openid+email");
        assert_eq!(q.get("code").unwrap(), "abc");
        assert_eq!(q.get("state").unwrap(), "x-y");
        assert_eq!(q.get("scope").unwrap(), "openid email");
    }

    /// A URL de autorização carrega valores com `:` e `/`; sem escapar, o
    /// servidor recusa o pedido inteiro.
    #[test]
    fn encode_escapa_o_que_precisa() {
        assert_eq!(encode("http://127.0.0.1:1455/auth/callback"),
                   "http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback");
        assert_eq!(encode("openid profile email"), "openid%20profile%20email");
        assert_eq!(encode("urn:uuid:abc-123_x.y~z"), "urn%3Auuid%3Aabc-123_x.y~z");
    }
}
