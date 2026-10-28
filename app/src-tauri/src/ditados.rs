//! Ditados que outro programa vai buscar: o perfil com "só salvar" grava aqui
//! em vez de colar.
//!
//! Nasceu para o laudo tomográfico do TOMO, que na hora do "áudio pronto" pega
//! o ditado mais recente desta pasta, descobre sozinho o paciente aberto e roda
//! a correção dele. Cada ditado vira `<AAAA-MM-DD_HHMMSS>.wav` e um `.json` com
//! o mesmo nome (hora local do início da gravação). O `.json` é escrito por
//! último, via `.tmp` + rename: quem lê a pasta nunca pega um ditado pela metade.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

/// Pasta configurada (`ditados_dir`), ou a padrão do app quando vazia. Apontar
/// para uma pasta sincronizada (o TOMO no Dropbox) deixa ditar num PC e fazer o
/// laudo no outro.
pub(crate) fn dir(configurada: &str) -> PathBuf {
    let padrao = || crate::config_dir().join("ditados");
    let p = expandir(configurada.trim());
    if p.is_empty() {
        return padrao();
    }
    let p = PathBuf::from(p);
    // relativo iria parar no diretório do processo, onde ninguém procura
    if !p.is_absolute() {
        eprintln!("[ditados] pasta {p:?} não é absoluta; usando a padrão");
        return padrao();
    }
    p
}

/// Troca `%VAR%` pelo valor da variável de ambiente (quem copia o exemplo da
/// tela, `%APPDATA%\...`, espera que funcione). Variável inexistente fica como está.
fn expandir(s: &str) -> String {
    let mut out = String::new();
    let mut resto = s;
    while let Some(i) = resto.find('%') {
        out.push_str(&resto[..i]);
        let depois = &resto[i + 1..];
        match depois.find('%') {
            Some(j) => match std::env::var(&depois[..j]) {
                Ok(v) if j > 0 => {
                    out.push_str(&v);
                    resto = &depois[j + 1..];
                }
                _ => {
                    out.push('%');
                    resto = depois;
                }
            },
            None => {
                out.push('%');
                resto = depois;
            }
        }
    }
    out.push_str(resto);
    out
}

/// Marca a pasta como "dos ditados do Open Flow". A limpeza só apaga onde ela
/// existe: apontar a configuração para Downloads, por engano, não pode apagar
/// arquivos de outro programa que por acaso tenham o mesmo padrão de nome.
const MARCA: &str = ".openflow-ditados";

fn marcar(pasta: &Path) -> Result<(), String> {
    std::fs::create_dir_all(pasta).map_err(|e| e.to_string())?;
    let m = pasta.join(MARCA);
    if !m.exists() {
        std::fs::write(&m, "pasta dos ditados do Open Flow: a limpeza por prazo só roda onde este arquivo existe\n")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

const FORMATO: &str = "%Y-%m-%d_%H%M%S";

/// Um ditado em andamento: nasce no início da gravação.
pub(crate) struct Ditado {
    inicio: chrono::DateTime<chrono::Local>,
    pub profile: String,
    /// decidida no início: trocar a configuração no meio não divide o ditado
    pasta: PathBuf,
    /// o `.wav` foi gravado; se não, o `.json` diz isso em vez de apontar para
    /// um arquivo que não existe
    wav_ok: bool,
}

#[derive(Serialize)]
struct Registro<'a> {
    inicio: String,
    fim: String,
    profile: &'a str,
    /// `null` quando o `.wav` não pôde ser gravado
    audio: Option<String>,
    audio_secs: f32,
    raw: &'a str,
    /// modelo e dispositivo informados pelo sidecar na resposta (ex.:
    /// "large-v3-turbo local cuda/float16"): prova de que foi local
    stt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stt_segundos: Option<f64>,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    erro: Option<&'a str>,
}

impl Ditado {
    /// O nome tem resolução de segundo: parar e começar outro ditado no mesmo
    /// segundo sobrescreveria o anterior. Avança um segundo até o nome estar livre.
    pub(crate) fn novo(pasta: PathBuf, profile: String) -> Self {
        let mut d = Ditado { inicio: chrono::Local::now(), profile, pasta, wav_ok: false };
        let pasta = d.pasta.clone();
        while pasta.join(format!("{}.wav", d.nome())).exists() || pasta.join(format!("{}.json", d.nome())).exists() {
            d.inicio += chrono::Duration::seconds(1);
        }
        d
    }

    fn nome(&self) -> String {
        self.inicio.format(FORMATO).to_string()
    }

    /// grava o áudio antes do STT: se ele falhar, a fala ainda existe
    pub(crate) fn salvar_wav(&mut self, wav: &[u8]) -> Result<(), String> {
        marcar(&self.pasta)?;
        std::fs::write(self.pasta.join(format!("{}.wav", self.nome())), wav).map_err(|e| e.to_string())?;
        self.wav_ok = true;
        Ok(())
    }

    /// `resultado`: o texto cru do STT, ou o motivo da falha
    pub(crate) fn salvar_json(
        &self,
        secs: f32,
        stt: &str,
        stt_segundos: Option<f64>,
        resultado: Result<&str, &str>,
    ) -> Result<(), String> {
        let pasta = &self.pasta;
        let nome = self.nome();
        let reg = Registro {
            inicio: self.inicio.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            fim: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            profile: &self.profile,
            audio: self.wav_ok.then(|| format!("{nome}.wav")),
            audio_secs: (secs * 10.0).round() / 10.0,
            raw: resultado.unwrap_or(""),
            stt,
            stt_segundos,
            status: if resultado.is_ok() { "ok" } else { "falhou" },
            erro: resultado.err(),
        };
        let json = serde_json::to_string_pretty(&reg).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(pasta).map_err(|e| e.to_string())?;
        let tmp = pasta.join(format!("{nome}.json.tmp"));
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, pasta.join(format!("{nome}.json"))).map_err(|e| e.to_string())
    }
}

/// Apaga `.wav`, `.json` e `.tmp` com mais de `keep_days` dias. A idade vem do
/// nome (início da gravação), como em `audios`; arquivo de outro formato fica.
pub(crate) fn limpar_antigos(pasta: &Path, keep_days: u32) -> usize {
    limpar_antigos_em(pasta, keep_days, SystemTime::now())
}

fn limpar_antigos_em(pasta: &Path, keep_days: u32, agora: SystemTime) -> usize {
    if !pasta.join(MARCA).exists() {
        return 0;
    }
    let rd = match std::fs::read_dir(pasta) {
        Ok(rd) => rd,
        Err(e) => {
            eprintln!("[ditados] limpeza não leu {pasta:?}: {e}");
            return 0;
        }
    };
    let limite = Duration::from_secs(keep_days as u64 * 86_400);
    let mut apagados = 0;
    for e in rd.filter_map(|e| e.ok()) {
        let Ok(nome) = e.file_name().into_string() else { continue };
        let Some((data, ext)) = nome.split_once('.') else { continue };
        if !matches!(ext, "wav" | "json" | "json.tmp") {
            continue;
        }
        let Ok(dt) = chrono::NaiveDateTime::parse_from_str(data, FORMATO) else { continue };
        let Some(gravado) = dt.and_local_timezone(chrono::Local).earliest() else { continue };
        let gravado: SystemTime = gravado.into();
        let vencido = keep_days == 0 || agora.duration_since(gravado).is_ok_and(|d| d > limite);
        if vencido && std::fs::remove_file(e.path()).is_ok() {
            apagados += 1;
        }
    }
    apagados
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pasta(nome: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("openflow-ditados-{nome}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn ditado_vira_wav_e_json_com_o_mesmo_nome() {
        let p = pasta("grava");
        let mut d = Ditado::novo(p.clone(), "Laudo".into());
        d.salvar_wav(b"RIFF").unwrap();
        d.salvar_json(12.34, "large-v3-turbo local cuda/float16", Some(0.8), Ok("Elemento 26.")).unwrap();
        let nome = d.nome();
        assert!(p.join(format!("{nome}.wav")).exists());
        assert!(!p.join(format!("{nome}.json.tmp")).exists(), "o .tmp tem que virar .json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(p.join(format!("{nome}.json"))).unwrap()).unwrap();
        assert_eq!(v["raw"], "Elemento 26.");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["audio"], format!("{nome}.wav"));
        assert_eq!(v["profile"], "Laudo");
        assert!(v.get("erro").is_none());
        assert_eq!(v["stt"], "large-v3-turbo local cuda/float16");
        assert_eq!(v["stt_segundos"], 0.8);
        assert!(v["inicio"].as_str().unwrap().len() >= 25, "ISO com fuso: {}", v["inicio"]);

        d.salvar_json(1.0, "x", None, Err("rede caiu")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(p.join(format!("{nome}.json"))).unwrap()).unwrap();
        assert_eq!(v["status"], "falhou");
        assert_eq!(v["erro"], "rede caiu");
        assert_eq!(v["raw"], "");
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn dois_ditados_no_mesmo_segundo_nao_se_sobrescrevem() {
        let p = pasta("mesmo-segundo");
        let mut a = Ditado::novo(p.clone(), "Laudo".into());
        a.salvar_wav(b"a").unwrap();
        let b = Ditado::novo(p.clone(), "Laudo".into());
        assert_ne!(a.nome(), b.nome());
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn limpeza_apaga_so_o_que_passou_do_prazo() {
        let p = pasta("limpa");
        let agora = chrono::Local::now();
        let nome = |dias: i64| (agora - chrono::Duration::days(dias)).format(FORMATO).to_string();
        for d in [1, 6, 8] {
            std::fs::write(p.join(format!("{}.wav", nome(d))), b"x").unwrap();
            std::fs::write(p.join(format!("{}.json", nome(d))), b"{}").unwrap();
        }
        std::fs::write(p.join("anotacao.txt"), b"fica").unwrap();
        assert_eq!(limpar_antigos_em(&p, 7, agora.into()), 0, "pasta sem a marca não é limpa");
        marcar(&p).unwrap();
        assert_eq!(limpar_antigos_em(&p, 7, agora.into()), 2, "só o de 8 dias, .wav e .json");
        assert!(p.join(format!("{}.json", nome(6))).exists());
        assert!(p.join("anotacao.txt").exists());
        assert!(p.join(MARCA).exists(), "a marca não é apagada");
        assert_eq!(limpar_antigos_em(&p, 0, agora.into()), 4, "0 apaga tudo o que é do módulo");
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn pasta_configurada_expande_variavel_e_recusa_relativa() {
        std::env::set_var("OPENFLOW_TESTE_DIR", r"C:\x");
        assert_eq!(dir(r"%OPENFLOW_TESTE_DIR%\ditados"), PathBuf::from(r"C:\x\ditados"));
        assert_eq!(dir("  "), crate::config_dir().join("ditados"));
        assert_eq!(dir(r"ditados\relativo"), crate::config_dir().join("ditados"), "relativo cai na padrão");
        assert_eq!(expandir("%NAO_EXISTE_XYZ%\\a"), "%NAO_EXISTE_XYZ%\\a");
        assert_eq!(expandir("100%"), "100%");
    }

    #[test]
    fn json_diz_quando_o_wav_nao_foi_gravado() {
        let p = pasta("sem-wav");
        let d = Ditado::novo(p.clone(), "Laudo".into());
        d.salvar_json(1.0, "x", None, Ok("t")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(p.join(format!("{}.json", d.nome()))).unwrap()).unwrap();
        assert!(v["audio"].is_null());
        let _ = std::fs::remove_dir_all(&p);
    }
}
