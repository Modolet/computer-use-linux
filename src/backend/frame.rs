//! @file frame.rs
//! @brief 不经编码的原始预览像素与共享内存帧租约
//! @author modolet <y@xxyx.io>
//! @date 2026-09-11

use memmap2::Mmap;
use std::{os::fd::OwnedFd, sync::Arc};

pub struct DmaPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}
pub struct DmaImage {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmaPlane>,
}
pub enum Image {
    Memory(RawFrame),
    Dma(Arc<DmaImage>),
}
impl Image {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Memory(frame) => (frame.width, frame.height),
            Self::Dma(frame) => (frame.width, frame.height),
        }
    }
    pub fn transport(&self) -> &'static str {
        match self {
            Self::Memory(_) => "SHM",
            Self::Dma(_) => "DMA-BUF",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PixelFormat {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

pub enum Pixels {
    /// Keeps the capture buffer unavailable for reuse until GTK releases its bytes.
    Shared(Arc<Mmap>),
    Owned(Vec<u8>),
}
impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Shared(map) => map.as_ref(),
            Self::Owned(bytes) => bytes,
        }
    }
}

pub struct RawFrame {
    pub pixels: Pixels,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
}
impl RawFrame {
    /// Only MCP screenshots and transformed outputs require a CPU conversion.
    pub fn rgba(&self) -> image::RgbaImage {
        let mut result = image::RgbaImage::new(self.width, self.height);
        for (row, target) in self
            .pixels
            .as_ref()
            .chunks_exact(self.stride as usize)
            .zip(result.as_mut().chunks_exact_mut(self.width as usize * 4))
        {
            for (pixel, output) in row.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
                let (r, b) = match self.format {
                    PixelFormat::Bgra | PixelFormat::Bgrx => (pixel[2], pixel[0]),
                    PixelFormat::Rgba | PixelFormat::Rgbx => (pixel[0], pixel[2]),
                };
                output.copy_from_slice(&[r, pixel[1], b, 255]);
            }
        }
        result
    }
}
