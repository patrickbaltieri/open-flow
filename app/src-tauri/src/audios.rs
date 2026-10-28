//! Cópia em disco de cada ditado, para retranscrever quando a transcrição falha.
//!
//! Antes daqui o áudio só existia na memória: se o STT caía (rede, Groq fora,
//! teto estourado), a fala se perdia junto com o erro. Agora cada ditado que
//! passa pelos filtros de duração e silêncio vira `audio/<ms>.wav`; uma falha
//! deixa ao lado um `<ms>.falhou` com o motivo, que some quando uma
//! retranscrição dá certo. Tudo com mais de `keep_days` dias é apagado.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub(crate) fn dir() -> PathBuf {
    crate::config_dir().join("audio")
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct Audio {
    /// nome do arquivo (`<ms>.wav`): é a chave que a interface devolve
    pub name: String,
    /// momento da gravação, em segundos desde a época
    pub ts: u64,
    pub secs: f32,
    /// motivo da última falha, se a transcrição não deu certo
    pub erro: Option<String>,
}

/// Aceita só o nome que este módulo gera: impede a interface de pedir
/// `..\settings.json` ou qualquer caminho fora da pasta.
pub(crate) fn nome_valido(name: &str) -> bool {
    match name.strip_suffix(".wav") {
        Some(stem) => !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

pub(crate) fn caminho(name: &str) -> Option<PathBuf> {
    nome_valido(name).then(|| dir().join(name))
}

fn marca(wav: &Path) -> PathBuf {
    wav.with_extension("falhou")
}

/// grava o WAV já pronto para o STT; devolve o caminho para marcar falha depois
pub(crate) fn salvar(wav: &[u8]) -> Result<PathBuf, String> {
    salvar_em(&dir(), wav)
}

fn salvar_em(pasta: &Path, wav: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(pasta).map_err(|e| e.to_string())?;
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let p = pasta.join(format!("{ms}.wav"));
    std::fs::write(&p, wav).map_err(|e| e.to_string())?;
    Ok(p)
}

pub(crate) fn marcar_falha(wav: &Path, erro: &str) {
    let _ = std::fs::write(marca(wav), erro);
}

pub(crate) fn limpar_falha(wav: &Path) {
    let _ = std::fs::remove_file(marca(wav));
}

/// bytes do WAV e duração em segundos, lida do cabeçalho
pub(crate) fn carregar(wav: &Path) -> Result<(Vec<u8>, f32), String> {
    let bytes = std::fs::read(wav).map_err(|e| format!("áudio não encontrado: {e}"))?;
    let r = hound::WavReader::new(std::io::Cursor::new(&bytes)).map_err(|e| format!("WAV inválido: {e}"))?;
    let secs = r.duration() as f32 / r.spec().sample_rate.max(1) as f32;
    Ok((bytes, secs))
}

/// do mais recente para o mais antigo
pub(crate) fn listar() -> Vec<Audio> {
    listar_em(&dir())
}

fn listar_em(pasta: &Path) -> Vec<Audio> {
    let Ok(rd) = std::fs::read_dir(pasta) else {
        return Vec::new();
    };
    let mut v: Vec<Audio> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !nome_valido(&name) {
                return None;
            }
            let ms: u64 = name.trim_end_matches(".wav").parse().ok()?;
            // só o cabeçalho: abrir a aba não pode ler centenas de MB de áudio
            let secs = hound::WavReader::open(e.path())
                .map(|r| r.duration() as f32 / r.spec().sample_rate.max(1) as f32)
                .unwrap_or(0.0);
            let erro = std::fs::read_to_string(marca(&e.path())).ok();
            Some(Audio { name, ts: ms / 1000, secs, erro })
        })
        .collect();
    v.sort_by(|a, b| b.name.len().cmp(&a.name.len()).then(b.name.cmp(&a.name)));
    v
}

/// pelo nome (momento da gravação), sem abrir arquivo nenhum
pub(crate) fn mais_recente() -> Option<PathBuf> {
    mais_recente_em(&dir())
}

fn mais_recente_em(pasta: &Path) -> Option<PathBuf> {
    std::fs::read_dir(pasta)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| nome_valido(n))
        .max_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)))
        .map(|n| pasta.join(n))
}

/// apaga WAV e marca de falha com mais de `keep_days` dias; 0 ("não guardar")
/// apaga tudo o que já estava guardado
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
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        if ext != "wav" && ext != "falhou" {
            continue;
        }
        // a idade vem do nome (momento da gravação), não da data do arquivo:
        // copiar a pasta ou o antivírus tocar no arquivo não pode adiar a limpeza
        let Some(ms) = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let gravado = UNIX_EPOCH + Duration::from_millis(ms);
        let vencido = keep_days == 0 || agora.duration_since(gravado).map(|d| d > limite).unwrap_or(false);
        if vencido && std::fs::remove_file(&p).is_ok() {
            apagados += 1;
        }
    }
    apagados
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pasta_teste(nome: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("openflow-audios-{nome}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn nome_fora_do_padrao_e_recusado() {
        assert!(nome_valido("1730000000000.wav"));
        assert!(!nome_valido("..\\settings.json"));
        assert!(!nome_valido("../1730000000000.wav"));
        assert!(!nome_valido("abc.wav"));
        assert!(!nome_valido(".wav"));
        assert!(!nome_valido("1730000000000.falhou"));
    }

    #[test]
    fn limpeza_apaga_so_o_que_passou_do_prazo() {
        let pasta = pasta_teste("limpeza");
        let agora = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let ms = |dias: u64| (1_800_000_000 - dias * 86_400) * 1000;
        for d in [1u64, 4, 6, 10] {
            std::fs::write(pasta.join(format!("{}.wav", ms(d))), b"x").unwrap();
        }
        std::fs::write(pasta.join(format!("{}.falhou", ms(6))), b"erro").unwrap();
        std::fs::write(pasta.join("outro.txt"), b"nao mexer").unwrap();

        assert_eq!(limpar_antigos_em(&pasta, 5, agora), 3, "6 e 10 dias + a marca de 6 dias");
        let mut sobra: Vec<String> = std::fs::read_dir(&pasta)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        sobra.sort();
        assert_eq!(sobra, vec![format!("{}.wav", ms(4)), format!("{}.wav", ms(1)), "outro.txt".into()]);
        assert_eq!(limpar_antigos_em(&pasta, 0, agora), 2, "não guardar apaga o que restou");
        assert!(pasta.join("outro.txt").exists(), "arquivo que não é do módulo fica");
        let _ = std::fs::remove_dir_all(&pasta);
    }

    #[test]
    fn lista_traz_duracao_falha_e_ordem_do_mais_novo() {
        let pasta = pasta_teste("lista");
        let wav = crate::wav_bytes(&vec![0.0f32; 32_000], 16_000); // 2s
        let a = salvar_em(&pasta, &wav).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let b = salvar_em(&pasta, &wav).unwrap();
        marcar_falha(&a, "rede caiu");

        let l = listar_em(&pasta);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].name, b.file_name().unwrap().to_str().unwrap(), "mais recente primeiro");
        assert_eq!(mais_recente_em(&pasta), Some(b.clone()));
        assert!((l[0].secs - 2.0).abs() < 0.01);
        assert_eq!(l[0].erro, None);
        assert_eq!(l[1].erro.as_deref(), Some("rede caiu"));

        limpar_falha(&a);
        assert_eq!(listar_em(&pasta)[1].erro, None, "retranscrição bem-sucedida tira a marca");
        let _ = std::fs::remove_dir_all(&pasta);
    }
}
