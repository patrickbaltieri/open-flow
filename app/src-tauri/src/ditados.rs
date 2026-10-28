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

pub(crate) fn dir() -> PathBuf {
    crate::config_dir().join("ditados")
}

const FORMATO: &str = "%Y-%m-%d_%H%M%S";

/// Um ditado em andamento: nasce no início da gravação.
pub(crate) struct Ditado {
    inicio: chrono::DateTime<chrono::Local>,
    pub profile: String,
}

#[derive(Serialize)]
struct Registro<'a> {
    inicio: String,
    fim: String,
    profile: &'a str,
    audio: String,
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
    pub(crate) fn novo(profile: String) -> Self {
        Self::novo_em(&dir(), profile)
    }

    /// O nome tem resolução de segundo: parar e começar outro ditado no mesmo
    /// segundo sobrescreveria o anterior. Avança um segundo até o nome estar livre.
    fn novo_em(pasta: &Path, profile: String) -> Self {
        let mut d = Ditado { inicio: chrono::Local::now(), profile };
        while pasta.join(format!("{}.wav", d.nome())).exists() || pasta.join(format!("{}.json", d.nome())).exists() {
            d.inicio += chrono::Duration::seconds(1);
        }
        d
    }

    fn nome(&self) -> String {
        self.inicio.format(FORMATO).to_string()
    }

    /// grava o áudio antes do STT: se ele falhar, a fala ainda existe
    pub(crate) fn salvar_wav(&self, wav: &[u8]) -> Result<(), String> {
        self.salvar_wav_em(&dir(), wav)
    }

    fn salvar_wav_em(&self, pasta: &Path, wav: &[u8]) -> Result<(), String> {
        std::fs::create_dir_all(pasta).map_err(|e| e.to_string())?;
        std::fs::write(pasta.join(format!("{}.wav", self.nome())), wav).map_err(|e| e.to_string())
    }

    /// `resultado`: o texto cru do STT, ou o motivo da falha
    pub(crate) fn salvar_json(
        &self,
        secs: f32,
        stt: &str,
        stt_segundos: Option<f64>,
        resultado: Result<&str, &str>,
    ) -> Result<(), String> {
        self.salvar_json_em(&dir(), secs, stt, stt_segundos, resultado)
    }

    fn salvar_json_em(
        &self,
        pasta: &Path,
        secs: f32,
        stt: &str,
        stt_segundos: Option<f64>,
        resultado: Result<&str, &str>,
    ) -> Result<(), String> {
        let nome = self.nome();
        let reg = Registro {
            inicio: self.inicio.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            fim: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            profile: &self.profile,
            audio: format!("{nome}.wav"),
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
pub(crate) fn limpar_antigos(keep_days: u32) -> usize {
    limpar_antigos_em(&dir(), keep_days, SystemTime::now())
}

fn limpar_antigos_em(pasta: &Path, keep_days: u32, agora: SystemTime) -> usize {
    let Ok(rd) = std::fs::read_dir(pasta) else {
        return 0;
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
        let d = Ditado::novo_em(&p, "Laudo".into());
        d.salvar_wav_em(&p, b"RIFF").unwrap();
        d.salvar_json_em(&p, 12.34, "large-v3-turbo local cuda/float16", Some(0.8), Ok("Elemento 26.")).unwrap();
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

        d.salvar_json_em(&p, 1.0, "x", None, Err("rede caiu")).unwrap();
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
        let a = Ditado::novo_em(&p, "Laudo".into());
        a.salvar_wav_em(&p, b"a").unwrap();
        let b = Ditado::novo_em(&p, "Laudo".into());
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
        assert_eq!(limpar_antigos_em(&p, 7, agora.into()), 2, "só o de 8 dias, .wav e .json");
        assert!(p.join(format!("{}.json", nome(6))).exists());
        assert!(p.join("anotacao.txt").exists());
        assert_eq!(limpar_antigos_em(&p, 0, agora.into()), 4, "0 apaga tudo o que é do módulo");
        let _ = std::fs::remove_dir_all(&p);
    }
}
