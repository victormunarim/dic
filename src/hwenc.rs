//! Codificação H.264 por hardware, por um `gst-launch-1.0` filho.
//!
//! Os quadros RGBA vão para o stdin do filho; ele converte para NV12, codifica na GPU (VA-API, NVENC ou
//! Quick Sync, o que houver) e escreve cada quadro codificado no stdout, empacotado pelo `gdppay` do
//! GStreamer (cabeçalho de 62 bytes com tipo e tamanho), que dá o limite exato de cada quadro sem
//! analisar o fluxo. Assim não há dependência de compilação do GStreamer: só os plugins em tempo de
//! execução (`gstreamer1.0-tools`, `gstreamer1.0-plugins-bad` e o driver de vídeo da GPU).
//!
//! O processo não aceita comandos: o que o [`crate::video::H264Encoder`] controla por quadro no
//! openh264 (QP, camadas temporais) aqui não existe. Quadro-chave sob demanda, mudança de resolução e
//! corte grande da taxa recomeçam o processo, que sempre abre com um quadro-chave. A saída chega com
//! alguns quadros de atraso, então [`HwEncoder::encode`] devolve o que já saiu, não o quadro que entrou.

use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::video::RawFrame;

/// Tamanho do cabeçalho do GDP (GStreamer Data Protocol).
const GDP_HEADER: usize = 62;
/// Tipo de pacote GDP que carrega um buffer (os outros são caps e eventos).
const GDP_BUFFER: u16 = 1;
/// Bit de `GST_BUFFER_FLAG_DELTA_UNIT` nas marcas do buffer: quadro que depende de outro.
const GDP_DELTA_UNIT: u16 = 1 << 13;
/// Quadros esperando para entrar no filho; mais que isso, o quadro novo é largado (a imagem velha não serve).
const FEED_DEPTH: usize = 4;
/// Sem nenhum quadro codificado depois de tanto tempo, o filho não está funcionando.
const STARTUP: Duration = Duration::from_secs(8);
/// Reinícios seguidos sem produzir nada antes de desistir do hardware.
const MAX_FAILURES: u32 = 3;
/// A taxa só sobe de novo (reinício, quadro-chave) depois deste tempo.
const RAISE_AFTER: Duration = Duration::from_secs(10);

/// Um codificador que o `gst-launch-1.0` sabe usar: nome do elemento e suas propriedades para uma taxa
/// em kbit/s e um intervalo máximo de quadros-chave.
struct Element {
    name: &'static str,
    props: fn(kbps: u32, gop: u32) -> String,
}

/// Em ordem de preferência.
const ELEMENTS: &[Element] = &[
    Element { name: "vah264lpenc", props: |k, g| format!("vah264lpenc bitrate={k} rate-control=cbr key-int-max={g} b-frames=0") },
    Element { name: "vah264enc", props: |k, g| format!("vah264enc bitrate={k} rate-control=cbr key-int-max={g} b-frames=0") },
    Element { name: "nvh264enc", props: |k, g| format!("nvh264enc bitrate={k} rc-mode=cbr gop-size={g} bframes=0 zerolatency=true") },
    Element { name: "qsvh264enc", props: |k, g| format!("qsvh264enc bitrate={k} rate-control=cbr gop-size={g} b-frames=0") },
    Element { name: "vaapih264enc", props: |k, g| format!("vaapih264enc bitrate={k} rate-control=cbr keyframe-period={g} max-bframes=0") },
    // Só para testar o encanamento onde não há GPU: codifica na CPU pelo mesmo caminho.
    #[cfg(test)]
    Element { name: "x264enc", props: |k, g| format!("x264enc tune=zerolatency speed-preset=ultrafast bitrate={k} key-int-max={g} bframes=0 byte-stream=true") },
];

/// Primeiro codificador por hardware que este computador tem (`gst-inspect-1.0 --exists`), se algum.
/// Chama processos: não use no executor assíncrono sem `spawn_blocking`. No Windows não há (o filho lê
/// os quadros por um descritor de arquivo).
pub fn detect() -> Option<&'static str> {
    if cfg!(windows) {
        return None;
    }
    ELEMENTS.iter().filter(|e| e.name != "x264enc").find(|e| exists(e.name)).map(|e| e.name)
}

pub(crate) fn exists(element: &str) -> bool {
    Command::new("gst-inspect-1.0").args(["--exists", element]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[derive(Debug, thiserror::Error)]
pub enum HwError {
    #[error("codificador por hardware parou de responder: {0}")]
    Dead(String),
    #[error("codificador desconhecido: {0}")]
    Unknown(String),
}

/// Quadro codificado que saiu do filho.
pub struct Au {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Um filho em execução.
struct Run {
    child: Child,
    feed: SyncSender<Vec<u8>>,
    out: Receiver<Au>,
    dims: (usize, usize),
    kbps: u32,
    started: Instant,
    got_any: bool,
    errors: Arc<Mutex<String>>,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct HwEncoder {
    element: &'static Element,
    run: Option<Run>,
    /// Esperando o quadro-chave de um processo recém-aberto: quadros dependentes que saírem antes não servem.
    awaiting_key: bool,
    failures: u32,
}

impl HwEncoder {
    pub fn new(element: &str) -> Result<Self, HwError> {
        let element = ELEMENTS.iter().find(|e| e.name == element).ok_or_else(|| HwError::Unknown(element.into()))?;
        Ok(Self { element, run: None, awaiting_key: true, failures: 0 })
    }

    pub fn name(&self) -> &'static str {
        self.element.name
    }

    /// Entrega `frame` (dimensões pares) ao codificador e devolve o quadro codificado mais antigo que já
    /// saiu, se houver. `bitrate` em bits/s e `fps` valem para o processo que estiver aberto: corte
    /// grande da taxa (ou alta depois de um tempo) e mudança de resolução o recomeçam. `force_key` pede
    /// um quadro-chave (recomeça o processo, uma vez por pedido). `Err`: o hardware não funciona; quem
    /// chama deve voltar ao openh264.
    pub fn encode(&mut self, frame: &RawFrame, bitrate: u32, fps: f32, force_key: bool) -> Result<Option<Au>, HwError> {
        let dims = (frame.width, frame.height);
        let kbps = (bitrate / 1000).max(50);
        let restart = match &self.run {
            None => true,
            Some(r) => r.dims != dims || (force_key && !self.awaiting_key) || kbps < r.kbps * 3 / 4 || (kbps > r.kbps * 4 / 3 && r.started.elapsed() > RAISE_AFTER),
        };
        if restart {
            self.start(dims, kbps, fps)?;
        }
        let run = self.run.as_mut().expect("aberto acima");
        // Cheio: o filho está atrasado, e a imagem que sobraria na fila já é velha.
        let _ = run.feed.try_send(frame.rgba.clone());
        loop {
            match run.out.try_recv() {
                Ok(au) => {
                    run.got_any = true;
                    self.failures = 0;
                    if self.awaiting_key && !au.keyframe {
                        continue;
                    }
                    self.awaiting_key = false;
                    return Ok(Some(au));
                }
                Err(TryRecvError::Empty) => {
                    if !run.got_any && run.started.elapsed() > STARTUP {
                        return self.fail();
                    }
                    return Ok(None);
                }
                Err(TryRecvError::Disconnected) => return self.fail(),
            }
        }
    }

    /// O filho morreu ou não produziu nada: tenta de novo algumas vezes, depois desiste.
    fn fail(&mut self) -> Result<Option<Au>, HwError> {
        self.failures += 1;
        let why = self.run.as_ref().map(|r| r.errors.lock().unwrap().clone()).unwrap_or_default();
        self.run = None;
        self.awaiting_key = true;
        if self.failures >= MAX_FAILURES { Err(HwError::Dead(why)) } else { Ok(None) }
    }

    fn start(&mut self, dims: (usize, usize), kbps: u32, fps: f32) -> Result<(), HwError> {
        self.run = None;
        let (w, h) = dims;
        let gop = (fps * 30.0) as u32;
        // Qualquer perfil que o openh264 decodifica; sem B-quadros.
        let pipeline = format!(
            "fdsrc fd=0 blocksize={} ! rawvideoparse format=rgba width={w} height={h} framerate={}/2 ! videoconvert ! video/x-raw,format=NV12 ! {} ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au,profile=(string){{constrained-baseline,baseline,main}} ! gdppay ! fdsink fd=1",
            w * h * 4,
            (fps * 2.0).round() as u32,
            (self.element.props)(kbps, gop),
        );
        let mut child = Command::new("gst-launch-1.0")
            .arg("-q")
            .args(pipeline.split_whitespace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| HwError::Dead(format!("não deu para rodar o gst-launch-1.0: {e}")))?;
        let mut stdin = child.stdin.take().expect("tubo pedido");
        let mut stdout = child.stdout.take().expect("tubo pedido");
        let mut stderr = child.stderr.take().expect("tubo pedido");
        let (feed, frames) = sync_channel::<Vec<u8>>(FEED_DEPTH);
        let (sink, out) = sync_channel::<Au>(64);
        let errors = Arc::new(Mutex::new(String::new()));
        std::thread::spawn(move || {
            for f in frames {
                if stdin.write_all(&f).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            let mut header = [0u8; GDP_HEADER];
            while stdout.read_exact(&mut header).is_ok() {
                let kind = u16::from_be_bytes([header[4], header[5]]);
                let len = u32::from_be_bytes([header[6], header[7], header[8], header[9]]) as usize;
                let flags = u16::from_be_bytes([header[42], header[43]]);
                // Um quadro de verdade nunca chega perto disso; um cabeçalho torto não pode alocar o mundo.
                if len > 16 << 20 {
                    break;
                }
                let mut data = vec![0u8; len];
                if stdout.read_exact(&mut data).is_err() {
                    break;
                }
                if kind == GDP_BUFFER && !data.is_empty() && sink.send(Au { data, keyframe: flags & GDP_DELTA_UNIT == 0 }).is_err() {
                    break;
                }
            }
        });
        let seen = errors.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            *seen.lock().unwrap() = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default().to_string();
        });
        self.awaiting_key = true;
        self.run = Some(Run { child, feed, out, dims, kbps, started: Instant::now(), got_any: false, errors });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::H264Decoder;

    /// Há `gst-launch-1.0` e `x264enc` neste computador? Senão os testes de encanamento não têm como rodar.
    fn available() -> bool {
        let ok = !cfg!(windows) && exists("x264enc") && Command::new("gst-launch-1.0").arg("--version").stdout(Stdio::null()).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("sem gst-launch-1.0 com x264enc: teste pulado");
        }
        ok
    }

    fn frame(t: usize) -> RawFrame {
        let (w, h) = (320usize, 240usize);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[((x + t * 6) % 256) as u8, (y % 256) as u8, 90, 255]);
            }
        }
        RawFrame::new(w, h, rgba).unwrap()
    }

    /// Alimenta o codificador até sair um quadro (ou 15 s).
    fn next(enc: &mut HwEncoder, t: &mut usize, force_key: bool) -> Au {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            *t += 1;
            if let Some(au) = enc.encode(&frame(*t), 500_000, 15.0, force_key && started.elapsed() < Duration::from_millis(30)).unwrap() {
                return au;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        panic!("o codificador não devolveu nada");
    }

    #[test]
    fn unknown_elements_are_refused() {
        assert!(matches!(HwEncoder::new("nao-existe"), Err(HwError::Unknown(_))));
    }

    #[test]
    fn the_child_pipeline_yields_a_decodable_keyframe_then_dependent_frames() {
        if !available() {
            return;
        }
        let mut enc = HwEncoder::new("x264enc").unwrap();
        let mut t = 0;
        let first = next(&mut enc, &mut t, false);
        assert!(first.keyframe, "o primeiro quadro tem de ser chave");
        let mut dec = H264Decoder::new().unwrap();
        let img = dec.decode(&first.data).unwrap().expect("imagem");
        assert_eq!((img.width, img.height), (320, 240));
        let second = next(&mut enc, &mut t, false);
        assert!(!second.keyframe);
        assert!(dec.decode(&second.data).unwrap().is_some());
    }

    #[test]
    fn a_keyframe_request_restarts_the_child_with_a_new_keyframe() {
        if !available() {
            return;
        }
        let mut enc = HwEncoder::new("x264enc").unwrap();
        let mut t = 0;
        next(&mut enc, &mut t, false);
        assert!(!next(&mut enc, &mut t, false).keyframe);
        // O pedido recomeça o processo: tudo o que sair antes do novo quadro-chave é descartado.
        enc.encode(&frame(t), 500_000, 15.0, true).unwrap();
        assert!(enc.awaiting_key);
        assert!(next(&mut enc, &mut t, false).keyframe);
    }

    #[test]
    fn a_broken_pipeline_gives_up_after_a_few_tries() {
        if !available() {
            return;
        }
        // Resolução ímpar: o `videoconvert` para NV12 e o x264 recusam, o filho sai e nada é produzido.
        let bad = RawFrame::new(7, 5, vec![0; 7 * 5 * 4]).unwrap();
        let mut enc = HwEncoder::new("x264enc").unwrap();
        let started = Instant::now();
        let err = loop {
            match enc.encode(&bad, 500_000, 15.0, false) {
                Err(e) => break e,
                Ok(_) => assert!(started.elapsed() < Duration::from_secs(30), "nunca desistiu"),
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(matches!(err, HwError::Dead(_)));
    }
}
