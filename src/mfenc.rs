//! Codificação H.264 por GPU no Windows, pelo Media Foundation.
//!
//! O codificador é um MFT de **hardware** (`MFT_ENUM_FLAG_HARDWARE`: NVENC, AMF, Quick Sync, o que o
//! driver registrar). Não há MFT de software da Microsoft aqui: o software já é o openh264, e um
//! codificador lento escondido atrás do nome "hardware" só enganaria quem lê o diagnóstico. Sem MFT
//! de hardware que abra, [`detect`] devolve `None` e [`MfEncoder::new`] erra, e o chamador fica no openh264.
//!
//! A entrada é o quadro RGBA da CPU convertido para NV12 (BT.601, faixa limitada, a mesma conta do
//! openh264, para o decodificador do outro lado ver as mesmas cores), repartido entre os núcleos. Não há
//! zero-copy: a captura já traz o quadro para a CPU. A saída é Annex B com SPS/PPS em todo quadro-chave
//! (o que o fragmentador e o openh264 esperam); se o driver não repetir o cabeçalho, ele é colocado aqui.
//!
//! O perfil é Baseline, CBR, baixa latência e sem B-quadros: o decodificador do outro lado é o openh264 e o
//! circuito não tolera quadro que depende do futuro. A taxa muda em andamento por `ICodecAPI::SetValue`;
//! mudar a resolução recria o MFT. Propriedade que o driver recusa não é inventada: fica em
//! [`MfEncoder::notes`] (o diagnóstico da GUI a mostra) e o MFT segue com o padrão dele.

use crate::hwenc::Au;
use std::{
    collections::VecDeque,
    sync::{
        Once,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Media::MediaFoundation::*,
        System::{
            Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED},
            Variant::VARIANT,
        },
    },
    core::{GUID, Interface, PWSTR},
};

#[derive(Debug, thiserror::Error)]
pub enum MfError {
    #[error("Media Foundation: {0}")]
    Api(String),
    #[error("nenhum codificador H.264 por hardware no Media Foundation")]
    NoHardware,
    #[error("codificador desconhecido: {0}")]
    Unknown(String),
}

fn api<T>(what: &str, r: windows::core::Result<T>) -> Result<T, MfError> {
    r.map_err(|e| MfError::Api(format!("{what}: {e}")))
}

/// Quanto o codificador espera pela saída do quadro que acabou de entrar. Passou disso, o quadro sai na
/// chamada seguinte (a GPU está atrasada; a imagem não é perdida).
const OUTPUT_WAIT: Duration = Duration::from_millis(120);
/// Quanto espera o MFT pedir a próxima entrada antes de dar o hardware por travado.
const INPUT_WAIT: Duration = Duration::from_millis(500);
/// Nenhuma saída depois de tantos quadros entrados: o MFT aceita e não produz.
const STALL_FRAMES: u32 = 90;
/// Mudança de taxa menor que isto (em %) não é enviada ao MFT.
const RATE_SLACK: u32 = 10;
/// Eventos do MFT assíncrono.
const NEED_INPUT: u32 = 601;
const HAVE_OUTPUT: u32 = 602;

/// Inicia o Media Foundation uma vez por processo e o COM na thread que chama (várias threads usam o
/// codificador ao longo da vida: a que abre e a que codifica nem sempre são a mesma).
#[allow(unsafe_code)]
fn init() -> Result<(), MfError> {
    static STARTUP: Once = Once::new();
    static STARTED: AtomicBool = AtomicBool::new(false);
    thread_local! {
        static COM: () = {
            // SAFETY: inicialização do COM na thread atual; se ela já está em outro modo (a thread da GUI),
            // o erro é ignorado, porque os MFTs do sistema são livres de apartamento.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        };
    }
    COM.with(|_| ());
    STARTUP.call_once(|| {
        // SAFETY: `MFStartup` não tem pré-condições além de ser chamada antes do uso; nunca é desfeita
        // (o processo inteiro usa o Media Foundation até sair).
        STARTED.store(unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.is_ok(), Ordering::Release);
    });
    if STARTED.load(Ordering::Acquire) { Ok(()) } else { Err(MfError::Api("MFStartup falhou".into())) }
}

/// Um MFT de hardware achado na enumeração.
struct Candidate {
    name: String,
    activate: IMFActivate,
}

#[allow(unsafe_code)]
fn enumerate() -> Result<Vec<Candidate>, MfError> {
    init()?;
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: os ponteiros de saída apontam para locais válidos; a lista devolvida é do chamador e é
    // liberada com `CoTaskMemFree` depois de os elementos serem movidos para fora.
    unsafe {
        api(
            "MFTEnumEx",
            MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&output), &mut list, &mut count),
        )?;
    }
    let mut found = Vec::new();
    if !list.is_null() {
        for i in 0..count as usize {
            // SAFETY: `i < count`; `take` move o elemento, deixando `None` no lugar para a liberação abaixo.
            let Some(activate) = (unsafe { (*list.add(i)).take() }) else { continue };
            let name = friendly_name(&activate).unwrap_or_else(|| "MFT H.264".into());
            found.push(Candidate { name, activate });
        }
        // SAFETY: bloco alocado pelo MFTEnumEx com o alocador do COM.
        unsafe { CoTaskMemFree(Some(list.cast())) };
    }
    Ok(found)
}

#[allow(unsafe_code)]
fn friendly_name(a: &IMFActivate) -> Option<String> {
    let mut text = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: saída em locais válidos; a string devolvida é liberada com `CoTaskMemFree`.
    unsafe {
        a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut len).ok()?;
        let s = text.to_string().ok();
        CoTaskMemFree(Some(text.0.cast()));
        s
    }
}

/// Nome do primeiro MFT de hardware que de fato abre e aceita uma configuração de teste, se algum.
/// Demora algumas centenas de ms (abre o driver); não use no executor assíncrono sem `spawn_blocking`.
pub fn detect() -> Option<String> {
    let found = enumerate().ok()?;
    found.into_iter().find(|c| Session::open(&c.activate, (1280, 720), 30.0, 2_000_000).is_ok()).map(|c| c.name)
}

/// Um MFT aberto e configurado para uma resolução, taxa de quadros e de bits.
struct Session {
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    dims: (usize, usize),
    fps: f32,
    kbps: u32,
    /// Propriedades que o driver recusou.
    notes: Vec<String>,
    /// Pedidos de entrada e saídas prontas ainda não atendidos (só no MFT assíncrono).
    need_input: u32,
    have_output: u32,
    provides_samples: bool,
    out_size: u32,
    in_size: u32,
    started: Instant,
    since_output: u32,
    out: VecDeque<Au>,
    /// SPS e PPS (com start codes) vistos até agora, para repor num quadro-chave que venha sem eles.
    header: Vec<u8>,
}

impl Session {
    #[allow(unsafe_code)]
    fn open(activate: &IMFActivate, dims: (usize, usize), fps: f32, bitrate: u32) -> Result<Self, MfError> {
        init()?;
        let (w, h) = (u32::try_from(dims.0).map_err(|_| MfError::Api("largura".into()))?, u32::try_from(dims.1).map_err(|_| MfError::Api("altura".into()))?);
        // SAFETY: ativação de um objeto COM pedido ao próprio `IMFActivate`.
        let mft: IMFTransform = api("ActivateObject", unsafe { activate.ActivateObject::<IMFTransform>() })?;
        let mut s = Session {
            events: mft.cast().ok(),
            codec: mft.cast().ok(),
            mft,
            dims,
            fps,
            kbps: bitrate / 1000,
            notes: Vec::new(),
            need_input: 0,
            have_output: 0,
            provides_samples: false,
            out_size: 0,
            in_size: 0,
            started: Instant::now(),
            since_output: 0,
            out: VecDeque::new(),
            header: Vec::new(),
        };
        s.unlock_async();
        s.set_properties(bitrate);
        let kbps_bits = bitrate.max(50_000);
        s.set_types(w, h, fps, kbps_bits)?;
        s.start()?;
        Ok(s)
    }

    /// MFT assíncrono (todo o de hardware) só aceita chamadas depois deste atributo; o síncrono o ignora.
    #[allow(unsafe_code)]
    fn unlock_async(&mut self) {
        // SAFETY: atributos do próprio MFT; nada é guardado além da chamada.
        unsafe {
            if let Ok(attrs) = self.mft.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
        }
        if self.events.is_none() {
            self.notes.push("MFT sem gerador de eventos (usado como síncrono)".into());
        }
    }

    #[allow(unsafe_code)]
    fn set_value(&mut self, name: &str, key: &GUID, value: VARIANT) {
        let Some(codec) = &self.codec else { return };
        // SAFETY: `key` e `value` valem durante a chamada; o MFT copia o valor.
        if let Err(e) = unsafe { codec.SetValue(key, &value) } {
            self.notes.push(format!("{name}: {e}"));
        }
    }

    /// Propriedades do codificador. Cada recusa é registrada e o resto segue: o MFT fica com o padrão dele.
    fn set_properties(&mut self, bitrate: u32) {
        if self.codec.is_none() {
            self.notes.push("MFT sem ICodecAPI".into());
            return;
        }
        self.set_value("AVEncCommonRateControlMode", &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
        self.set_value("AVEncCommonMeanBitRate", &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
        self.set_value("AVLowLatencyMode", &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
        self.set_value("AVEncMPVDefaultBPictureCount", &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
        // Um quadro-chave só quando pedido (e a cada ≈ 30 s): o quadro-chave grande congela a imagem no circuito.
        let gop = (self.fps * 30.0).max(1.0) as u32;
        self.set_value("AVEncMPVGOPSize", &CODECAPI_AVEncMPVGOPSize, VARIANT::from(gop));
        // Baseline não tem CABAC.
        self.set_value("AVEncH264CABACEnable", &CODECAPI_AVEncH264CABACEnable, VARIANT::from(false));
    }

    #[allow(unsafe_code)]
    fn set_types(&mut self, w: u32, h: u32, fps: f32, bitrate: u32) -> Result<(), MfError> {
        let rate = (fps * 1000.0).round().max(1000.0) as u32;
        // SAFETY: tipos de mídia criados e preenchidos aqui; o MFT os copia ao aceitar.
        unsafe {
            let out = api("MFCreateMediaType", MFCreateMediaType())?;
            api("saída/tipo", out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video))?;
            api("saída/subtipo", out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))?;
            api("saída/taxa", out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate))?;
            api("saída/tamanho", out.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(w) << 32) | u64::from(h)))?;
            api("saída/quadros", out.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(rate) << 32) | 1000))?;
            api("saída/aspecto", out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1))?;
            api("saída/entrelaçamento", out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
            if let Err(e) = out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32) {
                self.notes.push(format!("perfil Baseline: {e}"));
            }
            api("SetOutputType", self.mft.SetOutputType(0, &out, 0))?;

            let input = api("MFCreateMediaType", MFCreateMediaType())?;
            api("entrada/tipo", input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video))?;
            api("entrada/subtipo", input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))?;
            api("entrada/tamanho", input.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(w) << 32) | u64::from(h)))?;
            api("entrada/quadros", input.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(rate) << 32) | 1000))?;
            api("entrada/aspecto", input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1))?;
            api("entrada/entrelaçamento", input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))?;
            api("SetInputType", self.mft.SetInputType(0, &input, 0))?;

            let info = api("GetOutputStreamInfo", self.mft.GetOutputStreamInfo(0))?;
            self.provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            self.out_size = info.cbSize.max(1 << 20);
            let mut info = MFT_INPUT_STREAM_INFO::default();
            api("GetInputStreamInfo", self.mft.GetInputStreamInfo(0, &mut info))?;
            self.in_size = info.cbSize;
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn start(&mut self) -> Result<(), MfError> {
        // SAFETY: mensagens de controle sem ponteiros.
        unsafe {
            api("BEGIN_STREAMING", self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0))?;
            api("START_OF_STREAM", self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))?;
        }
        self.started = Instant::now();
        Ok(())
    }

    /// Muda a taxa em andamento.
    fn set_bitrate(&mut self, bitrate: u32) {
        self.kbps = bitrate / 1000;
        self.set_value("AVEncCommonMeanBitRate", &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
    }

    /// Lê os eventos pendentes do MFT assíncrono sem esperar.
    #[allow(unsafe_code)]
    fn pump_events(&mut self) -> Result<(), MfError> {
        let Some(events) = &self.events else { return Ok(()) };
        loop {
            // SAFETY: `GetEvent` sem espera só lê a fila de eventos do MFT.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    // SAFETY: evento devolvido acima.
                    let kind = unsafe { ev.GetType() }.map_err(|e| MfError::Api(format!("GetType: {e}")))?;
                    match kind {
                        NEED_INPUT => self.need_input += 1,
                        HAVE_OUTPUT => self.have_output += 1,
                        _ => {}
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(MfError::Api(format!("GetEvent: {e}"))),
            }
        }
    }

    /// Entrega um quadro NV12 (`nv12` já com Y e UV seguidos).
    #[allow(unsafe_code)]
    fn feed(&mut self, nv12: &[u8]) -> Result<(), MfError> {
        let len = nv12.len().max(self.in_size as usize);
        let time = (self.started.elapsed().as_nanos() / 100) as i64;
        let duration = (10_000_000.0 / self.fps.max(1.0)) as i64;
        // SAFETY: o buffer é criado com `len` bytes e só é escrito dentro do tamanho alocado, entre
        // `Lock` e `Unlock`; o ponteiro não é guardado.
        let sample = unsafe {
            let buf = api("MFCreateMemoryBuffer", MFCreateMemoryBuffer(len as u32))?;
            let mut ptr = std::ptr::null_mut();
            api("Lock", buf.Lock(&mut ptr, None, None))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            api("Unlock", buf.Unlock())?;
            api("SetCurrentLength", buf.SetCurrentLength(nv12.len() as u32))?;
            let sample = api("MFCreateSample", MFCreateSample())?;
            api("AddBuffer", sample.AddBuffer(&buf))?;
            api("SetSampleTime", sample.SetSampleTime(time))?;
            api("SetSampleDuration", sample.SetSampleDuration(duration))?;
            sample
        };
        let deadline = Instant::now() + INPUT_WAIT;
        loop {
            self.pump_events()?;
            if self.events.is_some() {
                if self.need_input > 0 {
                    self.need_input -= 1;
                    break;
                }
                // Saída pronta ocupa o MFT: esvazia antes de esperar a entrada.
                self.drain()?;
            } else {
                break;
            }
            if Instant::now() > deadline {
                return Err(MfError::Api("o MFT não pediu entrada".into()));
            }
            std::thread::sleep(Duration::from_micros(300));
        }
        // SAFETY: amostra válida acima.
        let r = unsafe { self.mft.ProcessInput(0, &sample, 0) };
        match r {
            Ok(()) => {
                self.since_output += 1;
                Ok(())
            }
            // MFT síncrono cheio: esvazia a saída e tenta uma vez mais.
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                self.drain()?;
                // SAFETY: idem.
                api("ProcessInput", unsafe { self.mft.ProcessInput(0, &sample, 0) })?;
                self.since_output += 1;
                Ok(())
            }
            Err(e) => Err(MfError::Api(format!("ProcessInput: {e}"))),
        }
    }

    /// Retira do MFT toda saída pronta e a põe na fila.
    #[allow(unsafe_code)]
    fn drain(&mut self) -> Result<(), MfError> {
        self.pump_events()?;
        loop {
            if self.events.is_some() {
                if self.have_output == 0 {
                    return Ok(());
                }
                self.have_output -= 1;
            }
            let own = if self.provides_samples {
                None
            } else {
                // SAFETY: amostra com buffer próprio para o MFT preencher.
                Some(unsafe {
                    let buf = api("MFCreateMemoryBuffer", MFCreateMemoryBuffer(self.out_size))?;
                    let sample = api("MFCreateSample", MFCreateSample())?;
                    api("AddBuffer", sample.AddBuffer(&buf))?;
                    sample
                })
            };
            let mut data = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: std::mem::ManuallyDrop::new(own), dwStatus: 0, pEvents: std::mem::ManuallyDrop::new(None) }];
            let mut status = 0u32;
            // SAFETY: um buffer de saída válido; as posses de `pSample` e `pEvents` voltam para este lado
            // e são soltas logo abaixo.
            let r = unsafe { self.mft.ProcessOutput(0, &mut data, &mut status) };
            // SAFETY: `ManuallyDrop::take` é chamado uma vez em cada campo, e os valores são soltos aqui.
            let (sample, _events) = unsafe { (std::mem::ManuallyDrop::take(&mut data[0].pSample), std::mem::ManuallyDrop::take(&mut data[0].pEvents)) };
            match r {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let bytes = read_sample(&sample)?;
                        self.since_output = 0;
                        self.push_output(bytes);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                // O MFT trocou o formato de saída (SPS novo, por exemplo): relê o tamanho de saída e segue.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // SAFETY: tipo de saída disponível pedido ao MFT.
                    unsafe {
                        if let Ok(t) = self.mft.GetOutputAvailableType(0, 0) {
                            api("SetOutputType", self.mft.SetOutputType(0, &t, 0))?;
                        }
                    }
                }
                Err(e) => return Err(MfError::Api(format!("ProcessOutput: {e}"))),
            }
            if self.events.is_none() && !self.out.is_empty() {
                // Síncrono: pode haver mais de uma; o laço termina em NEED_MORE_INPUT.
                continue;
            }
        }
    }

    /// Confere o Annex B, repõe o cabeçalho se faltar e enfileira o quadro.
    fn push_output(&mut self, mut data: Vec<u8>) {
        let Some(info) = scan(&data) else {
            self.notes.push("saída sem start code (não é Annex B)".into());
            return;
        };
        if info.sps_pps.is_some() {
            self.header = info.sps_pps.unwrap_or_default();
        }
        let key = info.idr;
        if key && !info.has_headers && !self.header.is_empty() {
            let mut with = self.header.clone();
            with.extend_from_slice(&data);
            data = with;
        }
        self.out.push_back(Au { data, keyframe: key });
    }
}

/// Copia o conteúdo de uma amostra para a memória da CPU.
#[allow(unsafe_code)]
fn read_sample(sample: &IMFSample) -> Result<Vec<u8>, MfError> {
    // SAFETY: o buffer contíguo é travado, copiado com o tamanho que ele mesmo informa e destravado.
    unsafe {
        let buf = api("ConvertToContiguousBuffer", sample.ConvertToContiguousBuffer())?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        api("Lock", buf.Lock(&mut ptr, None, Some(&mut len)))?;
        let bytes = if ptr.is_null() { Vec::new() } else { std::slice::from_raw_parts(ptr, len as usize).to_vec() };
        api("Unlock", buf.Unlock())?;
        Ok(bytes)
    }
}

/// O que importa saber de um quadro Annex B.
struct Scan {
    idr: bool,
    /// Tem SPS e PPS.
    has_headers: bool,
    /// SPS e PPS com os start codes, para reaproveitar.
    sps_pps: Option<Vec<u8>>,
}

/// Percorre os NALs do quadro. `None` se não começa com start code.
fn scan(data: &[u8]) -> Option<Scan> {
    // Posições (início do start code, início do NAL).
    let mut marks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data.get(i..i + 3) == Some(&[0, 0, 1]) {
            let begin = if i > 0 && data.get(i - 1) == Some(&0) { i - 1 } else { i };
            marks.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    if marks.first().map(|m| m.0) != Some(0) {
        return None;
    }
    let (mut idr, mut sps, mut pps) = (false, false, false);
    let mut header = Vec::new();
    for (n, &(begin, start)) in marks.iter().enumerate() {
        let end = marks.get(n + 1).map_or(data.len(), |m| m.0);
        let kind = data.get(start).map_or(0, |b| b & 0x1f);
        match kind {
            5 => idr = true,
            7 | 8 => {
                if kind == 7 {
                    sps = true;
                } else {
                    pps = true;
                }
                header.extend_from_slice(data.get(begin..end).unwrap_or_default());
            }
            _ => {}
        }
    }
    Some(Scan { idr, has_headers: sps && pps, sps_pps: (sps && pps).then_some(header) })
}

/// RGBA para NV12 (BT.601, faixa limitada), repartido entre os núcleos. `out` tem `w*h*3/2` bytes: o plano
/// Y e depois o UV intercalado, com a média de cada bloco 2×2.
fn rgba_to_nv12(rgba: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let pairs = h / 2;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8).min(pairs.max(1));
    let per = pairs.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let rows = y_plane.chunks_mut(per * 2 * w).zip(uv_plane.chunks_mut(per * w)).zip(rgba.chunks(per * 2 * w * 4));
        for ((y, uv), src) in rows {
            s.spawn(move || convert_rows(src, w, y, uv));
        }
    });
}

/// Converte um número par de linhas.
fn convert_rows(src: &[u8], w: usize, y: &mut [u8], uv: &mut [u8]) {
    for ((src2, y2), uv_row) in src.chunks_exact(w * 8).zip(y.chunks_exact_mut(w * 2)).zip(uv.chunks_exact_mut(w)) {
        let (top, bottom) = src2.split_at(w * 4);
        let (y_top, y_bottom) = y2.split_at_mut(w);
        for (((a, b), (ya, yb)), uvp) in top.as_chunks::<8>().0.iter().zip(bottom.as_chunks::<8>().0.iter()).zip(y_top.as_chunks_mut::<2>().0.iter_mut().zip(y_bottom.as_chunks_mut::<2>().0.iter_mut())).zip(uv_row.as_chunks_mut::<2>().0.iter_mut()) {
            let (mut r, mut g, mut bl) = (0i32, 0i32, 0i32);
            let (ya0, ya1) = ya.split_at_mut(1);
            let (yb0, yb1) = yb.split_at_mut(1);
            for (px, out) in [(a.get(0..4), ya0.first_mut()), (a.get(4..8), ya1.first_mut()), (b.get(0..4), yb0.first_mut()), (b.get(4..8), yb1.first_mut())] {
                let (Some([pr, pg, pb, _]), Some(out)) = (px, out) else { continue };
                let (pr, pg, pb) = (i32::from(*pr), i32::from(*pg), i32::from(*pb));
                *out = (((66 * pr + 129 * pg + 25 * pb + 128) >> 8) + 16) as u8;
                r += pr;
                g += pg;
                bl += pb;
            }
            let (r, g, bl) = ((r + 2) >> 2, (g + 2) >> 2, (bl + 2) >> 2);
            let [u, v] = uvp;
            *u = ((((-38 * r - 74 * g + 112 * bl + 128) >> 8) + 128).clamp(0, 255)) as u8;
            *v = ((((112 * r - 94 * g - 18 * bl + 128) >> 8) + 128).clamp(0, 255)) as u8;
        }
    }
}

/// Codificador H.264 por um MFT de hardware.
pub struct MfEncoder {
    name: String,
    session: Option<Session>,
    nv12: Vec<u8>,
    /// Diagnóstico acumulado das propriedades recusadas, mesmo depois de a sessão ser recriada.
    notes: Vec<String>,
}

// SAFETY: o MFT é livre de apartamento (MTA) e o `MfEncoder` só é usado por uma thread por vez (a que
// codifica); as interfaces COM não guardam afinidade de thread.
#[allow(unsafe_code)]
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Prepara o MFT de hardware chamado `name` (o nome devolvido por [`detect`]). O MFT só abre no primeiro quadro.
    pub fn new(name: &str) -> Result<Self, MfError> {
        let found = enumerate()?;
        if found.is_empty() {
            return Err(MfError::NoHardware);
        }
        if !found.iter().any(|c| c.name == name) {
            return Err(MfError::Unknown(name.into()));
        }
        Ok(Self { name: name.into(), session: None, nv12: Vec::new(), notes: Vec::new() })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Propriedades que o driver recusou (o MFT seguiu com o padrão dele), para o diagnóstico.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Como `HwEncoder::encode`: entrega `frame` (dimensões pares) e devolve o quadro codificado que já
    /// saiu. `bitrate` em bits/s; `force_key` pede quadro-chave. `Err`: o hardware não funciona e o
    /// chamador deve voltar ao openh264.
    pub fn encode(&mut self, frame: &crate::video::RawFrame, bitrate: u32, fps: f32, force_key: bool) -> Result<Option<Au>, MfError> {
        let dims = (frame.width, frame.height);
        let stale = self.session.as_ref().is_some_and(|s| s.dims != dims || (s.fps - fps).abs() > s.fps * 0.25);
        if stale {
            self.session = None;
        }
        let s = match &mut self.session {
            Some(s) => s,
            slot => {
                let activate = enumerate()?.into_iter().find(|c| c.name == self.name).ok_or_else(|| MfError::Unknown(self.name.clone()))?;
                let opened = Session::open(&activate.activate, dims, fps, bitrate)?;
                self.notes = opened.notes.clone();
                slot.insert(opened)
            }
        };
        let old = s.kbps.max(1);
        if bitrate / 1000 * 100 > old * (100 + RATE_SLACK) || bitrate / 1000 * 100 < old * (100 - RATE_SLACK) {
            s.set_bitrate(bitrate);
        }
        if force_key {
            s.set_value("AVEncVideoForceKeyFrame", &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32));
        }
        let size = frame.width * frame.height * 3 / 2;
        self.nv12.resize(size, 0);
        rgba_to_nv12(&frame.rgba, frame.width, frame.height, &mut self.nv12);
        s.feed(&self.nv12)?;
        let deadline = Instant::now() + OUTPUT_WAIT;
        loop {
            s.drain()?;
            if !s.out.is_empty() || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_micros(300));
        }
        if s.since_output > STALL_FRAMES {
            return Err(MfError::Api(format!("o MFT aceitou {STALL_FRAMES} quadros e não devolveu nenhum")));
        }
        for n in &s.notes {
            if !self.notes.contains(n) {
                self.notes.push(n.clone());
            }
        }
        Ok(s.out.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{H264Decoder, RawFrame};

    #[test]
    fn rgba_to_nv12_keeps_grays_and_primaries_in_range() {
        let (w, h) = (4usize, 2usize);
        let mut rgba = Vec::new();
        for _ in 0..w * h {
            rgba.extend_from_slice(&[255, 255, 255, 255]);
        }
        let mut out = vec![0u8; w * h * 3 / 2];
        rgba_to_nv12(&rgba, w, h, &mut out);
        assert!(out[..w * h].iter().all(|&y| (234..=236).contains(&y)), "branco = Y 235");
        assert!(out[w * h..].iter().all(|&c| (127..=129).contains(&c)), "cinza sem croma");
        let mut black = vec![0u8; w * h * 3 / 2];
        rgba_to_nv12(&vec![0; w * h * 4], w, h, &mut black);
        assert!(black[..w * h].iter().all(|&y| y == 16));
    }

    #[test]
    fn rgba_to_nv12_handles_uneven_thread_splits() {
        // Muitas linhas e poucos pares por thread: o resultado não pode depender da repartição.
        let (w, h) = (6usize, 34usize);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut a = vec![0u8; w * h * 3 / 2];
        rgba_to_nv12(&rgba, w, h, &mut a);
        let mut b = vec![0u8; w * h * 3 / 2];
        let (y, uv) = b.split_at_mut(w * h);
        convert_rows(&rgba, w, y, uv);
        assert_eq!(a, b);
    }

    #[test]
    fn scan_finds_idr_and_headers() {
        let key = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9];
        let s = scan(&key).unwrap();
        assert!(s.idr && s.has_headers);
        assert_eq!(s.sps_pps.unwrap(), [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
        let delta = [0, 0, 0, 1, 0x41, 5, 5];
        let s = scan(&delta).unwrap();
        assert!(!s.idr && !s.has_headers && s.sps_pps.is_none());
        assert!(scan(&[1, 2, 3, 4]).is_none());
        assert!(scan(&[]).is_none());
    }

    /// Tela de documento: texto parado (blocos pequenos de contraste alto) e uma janela de 300×300 que se mexe,
    /// como num compartilhamento de verdade. Conteúdo aleatório quadro a quadro não serve: nenhum controle de
    /// taxa cumpre 2 Mbps com ruído em 1080p.
    fn synthetic(w: usize, h: usize, t: usize) -> RawFrame {
        let mut rgba = Vec::with_capacity(w * h * 4);
        let (mx, my) = ((t * 9) % (w - 300), (t * 5) % (h - 300));
        for y in 0..h {
            for x in 0..w {
                let text = if (x / 4 + y / 14) % 3 == 0 && y % 14 < 9 { 30 } else { 235 };
                let moving = x >= mx && x < mx + 300 && y >= my && y < my + 300;
                let px = if moving { [((x - mx) % 256) as u8, ((y - my) % 256) as u8, (t % 256) as u8, 255] } else { [text, text, text, 255] };
                rgba.extend_from_slice(&px);
            }
        }
        RawFrame::new(w, h, rgba).unwrap()
    }

    /// Manual: precisa de GPU com MFT H.264 de hardware. `cargo test --features video --lib -- --ignored mf_ --nocapture`
    #[test]
    #[ignore]
    fn mf_encodes_synthetic_screens_that_the_clam_decoder_opens() {
        let Some(name) = detect() else {
            eprintln!("sem MFT H.264 de hardware: nada a medir");
            return;
        };
        eprintln!("MFT: {name}");
        for (w, h, kbps) in [(1920usize, 1080usize, 8_000u32), (2560, 1440, 8_000), (1920, 1080, 2_000)] {
            let mut enc = MfEncoder::new(&name).unwrap();
            let mut dec = H264Decoder::new().unwrap();
            let (mut total_ms, mut count, mut bytes, mut keys, mut decoded) = (0.0f64, 0u32, 0usize, 0u32, 0u32);
            let mut worst = 0.0f64;
            let mut first_ms = 0.0;
            let mut t = 0;
            let wall = Instant::now();
            while count < 120 {
                t += 1;
                // Ritmo de 30 quadros/s: o controle de taxa do driver depende do tempo real entre quadros.
                let due = Duration::from_millis(33) * t as u32;
                if let Some(wait) = due.checked_sub(wall.elapsed()) {
                    std::thread::sleep(wait);
                }
                let f = synthetic(w, h, t);
                let started = Instant::now();
                let au = enc.encode(&f, kbps * 1000, 30.0, t == 1).unwrap();
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                if t == 1 {
                    first_ms = ms;
                    assert!(au.is_none() || au.as_ref().unwrap().keyframe, "o primeiro quadro tem de ser chave");
                } else {
                    total_ms += ms;
                    worst = worst.max(ms);
                    count += 1;
                }
                if let Some(au) = au {
                    bytes += au.data.len();
                    keys += u32::from(au.keyframe);
                    if let Some(img) = dec.decode(&au.data).unwrap() {
                        assert_eq!((img.width, img.height), (w, h));
                        decoded += 1;
                    }
                }
                assert!(t < 400, "o codificador quase não devolve quadros");
            }
            eprintln!(
                "{w}x{h} alvo {kbps} kbps: {:.1} ms/quadro (pior {worst:.1}, primeiro {first_ms:.1}), {} B/quadro, {keys} chaves, {decoded} decodificados; propriedades recusadas: {:?}",
                total_ms / f64::from(count),
                bytes / t,
                enc.notes()
            );
            eprintln!("  taxa real: {:.0} kbps em {:.1} s", bytes as f64 * 8.0 / 1000.0 / wall.elapsed().as_secs_f64(), wall.elapsed().as_secs_f64());
            assert!(decoded > 100, "o decodificador do clam tem de abrir a saída");
        }
    }

    /// Chama `encode` até sair um quadro (ou 3 s), pedindo quadro-chave só na primeira volta.
    fn next_au(enc: &mut MfEncoder, w: usize, h: usize, kbps: u32, t: &mut usize, mut key: bool) -> Au {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            *t += 1;
            if let Some(au) = enc.encode(&synthetic(w, h, *t), kbps * 1000, 30.0, std::mem::take(&mut key)).unwrap() {
                return au;
            }
        }
        panic!("sem quadro em 3 s");
    }

    /// Manual: quadro-chave sob demanda, mudança de taxa e de resolução, e o `H264Encoder` inteiro por cima do MFT.
    #[test]
    #[ignore]
    fn mf_keyframe_on_demand_rate_change_resize_and_h264encoder() {
        let Some(name) = detect() else {
            eprintln!("sem MFT H.264 de hardware: nada a medir");
            return;
        };
        let mut enc = MfEncoder::new(&name).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let mut t = 0;
        let first = next_au(&mut enc, 1280, 720, 4000, &mut t, true);
        assert!(first.keyframe && scan(&first.data).unwrap().has_headers, "quadro-chave com SPS/PPS");
        assert!(dec.decode(&first.data).unwrap().is_some());
        let delta = next_au(&mut enc, 1280, 720, 4000, &mut t, false);
        assert!(!delta.keyframe, "sem pedido, quadro dependente");
        let mut keys = usize::from(next_au(&mut enc, 1280, 720, 4000, &mut t, true).keyframe);
        for _ in 0..10 {
            keys += usize::from(next_au(&mut enc, 1280, 720, 4000, &mut t, false).keyframe);
        }
        eprintln!("quadro-chave pedido: {keys} chave(s) nos 11 quadros seguintes");
        assert!(keys >= 1, "o pedido de quadro-chave não valeu");
        // Taxa baixa em andamento: o tamanho médio tem de cair.
        let avg = |enc: &mut MfEncoder, kbps: u32, t: &mut usize| (0..40).map(|_| next_au(enc, 1280, 720, kbps, t, false).data.len()).sum::<usize>() / 40;
        let high = avg(&mut enc, 4000, &mut t);
        let low = avg(&mut enc, 500, &mut t);
        eprintln!("média por quadro: {high} B a 4000 kbps, {low} B depois de baixar para 500 kbps");
        assert!(low < high / 2, "SetValue de taxa não surtiu efeito");
        // Resolução nova: sessão nova, quadro-chave decodificável nas dimensões novas.
        let big = next_au(&mut enc, 1920, 1080, 4000, &mut t, true);
        assert!(big.keyframe);
        let mut dec = H264Decoder::new().unwrap();
        let img = dec.decode(&big.data).unwrap().expect("imagem");
        assert_eq!((img.width, img.height), (1920, 1080));
        // O H264Encoder do pipeline, com o MFT no lugar do openh264.
        let mut pipe = crate::video::H264Encoder::new(crate::video::H264Encoder::default_bitrate()).unwrap();
        assert!(pipe.set_hardware(Some(&name)));
        assert_eq!(pipe.hardware(), Some(name.as_str()));
        let mut dec = H264Decoder::new().unwrap();
        let (mut got, mut shown) = (0, 0);
        for i in 0..90 {
            if let Some(f) = pipe.encode(&synthetic(1920, 1080, i), i == 0).unwrap() {
                got += 1;
                shown += usize::from(dec.decode(&f.data).unwrap().is_some());
            }
        }
        eprintln!("H264Encoder: {got} quadros, {shown} decodificados, {:.1} ms/quadro, hw_note {:?}", pipe.encode_ms, pipe.hw_note);
        assert!(shown > 60 && pipe.hw_note.is_none() && pipe.hardware().is_some());
    }
}
