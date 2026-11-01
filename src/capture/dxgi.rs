//! Captura da tela no Windows por DXGI Desktop Duplication.
//!
//! O compositor entrega o quadro já pronto na GPU e só avisa quando a tela mudou, então não há
//! `BitBlt` por quadro nem imagem repetida. A cópia para a CPU (textura de preparo + `Map`) ainda
//! existe porque o codificador atual é o openh264; a GPU só deixa de ser o gargalo da captura.
//! Cai para o GDI ([`super::GdiScreen`]) quando a duplicação não abre (sessão remota, tela
//! protegida, driver sem suporte).

use crate::video::{RawFrame, ScreenSource};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

/// Quanto o `grab` espera por uma mudança de tela antes de devolver `None`.
const WAIT_MS: u32 = 16;

/// Duplicação da primeira saída (monitor principal) do adaptador padrão.
pub struct DxgiScreen {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    /// Textura legível pela CPU, recriada quando a resolução muda.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

// SAFETY: o dispositivo D3D11 criado sem `D3D11_CREATE_DEVICE_SINGLETHREADED` é seguro entre
// threads, e `DxgiScreen` só é usada por uma thread por vez (a do `Prefetch`).
#[allow(unsafe_code)]
unsafe impl Send for DxgiScreen {}

#[allow(unsafe_code)]
fn open_duplication(device: &ID3D11Device) -> windows::core::Result<IDXGIOutputDuplication> {
    // SAFETY: chamadas COM com ponteiros válidos devolvidos pelo próprio D3D/DXGI.
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = dxgi.GetAdapter()?;
        let output = adapter.EnumOutputs(0)?;
        let output1: IDXGIOutput1 = output.cast()?;
        output1.DuplicateOutput(device)
    }
}

impl DxgiScreen {
    /// Abre a duplicação; erro quando o sistema não a oferece (o chamador usa o GDI).
    #[allow(unsafe_code)]
    pub fn open() -> Result<Self, super::CaptureError> {
        let mut device = None;
        let mut ctx = None;
        // SAFETY: os ponteiros de saída apontam para locais válidos durante a chamada.
        let created = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
        };
        let (Ok(()), Some(device), Some(ctx)) = (created, device, ctx) else {
            return Err(super::CaptureError::Dxgi);
        };
        let dup = open_duplication(&device).map_err(|_| super::CaptureError::Dxgi)?;
        Ok(Self { device, ctx, dup, staging: None })
    }

    /// Textura de preparo do tamanho pedido, criada de novo se a resolução mudou.
    #[allow(unsafe_code)]
    fn staging_for(&mut self, w: u32, h: u32, src: &D3D11_TEXTURE2D_DESC) -> Option<ID3D11Texture2D> {
        if let Some((t, sw, sh)) = &self.staging {
            if (*sw, *sh) == (w, h) {
                return Some(t.clone());
            }
        }
        let mut desc = *src;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut tex = None;
        // SAFETY: `desc` descreve uma textura de preparo válida; `tex` é local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)).ok()? };
        let tex = tex?;
        self.staging = Some((tex.clone(), w, h));
        Some(tex)
    }
}

impl ScreenSource for DxgiScreen {
    #[allow(unsafe_code)]
    fn grab(&mut self) -> Option<RawFrame> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        // SAFETY: saídas locais; todo quadro adquirido é liberado com `ReleaseFrame` abaixo.
        let acquired = unsafe { self.dup.AcquireNextFrame(WAIT_MS, &mut info, &mut res) };
        if let Err(e) = acquired {
            if e.code() == DXGI_ERROR_ACCESS_LOST {
                // Troca de resolução, bloqueio de tela, UAC: a duplicação morreu, abre outra.
                if let Ok(d) = open_duplication(&self.device) {
                    self.dup = d;
                    self.staging = None;
                }
            } else if e.code() != DXGI_ERROR_WAIT_TIMEOUT {
                return None;
            }
            return None;
        }
        let frame = (|| {
            let tex: ID3D11Texture2D = res?.cast().ok()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: `desc` é local e `tex` é uma textura viva.
            unsafe { tex.GetDesc(&mut desc) };
            let (w, h) = (desc.Width, desc.Height);
            let stage = self.staging_for(w, h, &desc)?;
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: copia entre texturas do mesmo formato e tamanho e mapeia a de preparo só
            // para leitura; o `Unmap` vem antes de qualquer retorno depois do `Map`.
            unsafe {
                self.ctx.CopyResource(&stage, &tex);
                self.ctx.Map(&stage, 0, D3D11_MAP_READ, 0, Some(&mut map)).ok()?;
            }
            let (w, h) = (w as usize, h as usize);
            let pitch = map.RowPitch as usize;
            let mut rgba = vec![0u8; w.checked_mul(h)?.checked_mul(4)?];
            // SAFETY: o mapa cobre `pitch * h` bytes legíveis até o `Unmap`.
            let src = unsafe { std::slice::from_raw_parts(map.pData as *const u8, pitch * h) };
            for (row, out) in src.chunks_exact(pitch).zip(rgba.chunks_exact_mut(w * 4)) {
                for (s, d) in row.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                    d.copy_from_slice(&[s[2], s[1], s[0], 255]);
                }
            }
            // SAFETY: desfaz o `Map` acima.
            unsafe { self.ctx.Unmap(&stage, 0) };
            Some(RawFrame::new(w as u32, h as u32, rgba))
        })();
        // SAFETY: libera o quadro adquirido, haja ou não imagem.
        let _ = unsafe { self.dup.ReleaseFrame() };
        frame
    }
}
