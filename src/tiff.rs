#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::{
    io::{Cursor, Read, Seek},
    sync::Arc,
};
use tiff::{
    ColorType,
    decoder::{ChunkType, Decoder, DecodingResult},
    tags::Tag,
};

#[derive(Clone)]
pub enum Source {
    #[cfg(not(target_arch = "wasm32"))]
    Path(PathBuf),
    #[cfg(not(target_arch = "wasm32"))]
    PngGrid(Arc<crate::png_grid::PngGrid>),
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Bytes(Arc<[u8]>),
}
trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}
impl Source {
    fn decoder(&self) -> Result<Decoder<Box<dyn ReadSeek>>, String> {
        let reader: Box<dyn ReadSeek> = match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Path(path) => Box::new(std::fs::File::open(path).map_err(|e| e.to_string())?),
            Self::Bytes(bytes) => Box::new(Cursor::new(bytes.clone())),
            #[cfg(not(target_arch = "wasm32"))]
            Self::PngGrid(_) => return Err("This source contains PNG tiles".into()),
        };
        Decoder::new(reader).map_err(|e| e.to_string())
    }
}

#[derive(Clone)]
pub struct Info {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bits: u8,
    pub channels: usize,
    pub white_is_zero: bool,
    pub chunk_width: u32,
    pub chunk_height: u32,
    pub chunks_x: u32,
    pub chunks_y: u32,
}
#[derive(Clone, Copy)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}
impl Info {
    pub fn count(&self) -> u32 {
        self.chunks_x * self.chunks_y
    }
    pub fn rect(&self, index: u32) -> Rect {
        let x = index % self.chunks_x * self.chunk_width;
        let y = index / self.chunks_x * self.chunk_height;
        Rect {
            x,
            y,
            w: self.chunk_width.min(self.width - x),
            h: self.chunk_height.min(self.height - y),
        }
    }
    pub fn index_at(&self, x: u32, y: u32) -> u32 {
        y / self.chunk_height * self.chunks_x + x / self.chunk_width
    }
}

pub fn inspect(source: &Source, name: String) -> Result<Info, String> {
    #[cfg(not(target_arch = "wasm32"))]
    if let Source::PngGrid(grid) = source {
        return Ok(grid.info());
    }
    let mut decoder = source.decoder()?;
    let (width, height) = decoder.dimensions().map_err(|e| e.to_string())?;
    if width == 0 || height == 0 {
        return Err("Empty image".into());
    }
    let color = decoder.colortype().map_err(|e| e.to_string())?;
    let (bits, channels) = match color {
        ColorType::Gray(8) => (8, 1),
        ColorType::Gray(16) => (16, 1),
        ColorType::RGB(8) => (8, 3),
        ColorType::RGB(16) => (16, 3),
        ColorType::RGBA(8) => (8, 4),
        ColorType::RGBA(16) => (16, 4),
        _ => return Err(format!("Unsupported pixel format: {color:?}")),
    };
    let planar: u16 = decoder
        .find_tag_unsigned(Tag::PlanarConfiguration)
        .map_err(|e| e.to_string())?
        .unwrap_or(1);
    if planar != 1 {
        return Err("Only contiguous TIFF pixel layout is supported".into());
    }
    let white_is_zero = decoder
        .find_tag_unsigned::<u16>(Tag::PhotometricInterpretation)
        .map_err(|e| e.to_string())?
        .unwrap_or(1)
        == 0;
    let (chunk_width, chunk_height) = decoder.chunk_dimensions();
    if chunk_width == 0 || chunk_height == 0 {
        return Err("Invalid chunk size".into());
    }
    let chunks_x = width.div_ceil(chunk_width);
    let chunks_y = height.div_ceil(chunk_height);
    let count = chunks_x
        .checked_mul(chunks_y)
        .ok_or("Too many TIFF chunks")?;
    let actual = match decoder.get_chunk_type() {
        ChunkType::Strip => decoder.strip_count(),
        ChunkType::Tile => decoder.tile_count(),
    }
    .map_err(|e| e.to_string())?;
    if actual < count {
        return Err("Image chunks are missing".into());
    }
    Ok(Info {
        name,
        width,
        height,
        bits,
        channels,
        white_is_zero,
        chunk_width,
        chunk_height,
        chunks_x,
        chunks_y,
    })
}

pub enum Pixels {
    U8(Vec<u8>),
    U16(Vec<u16>),
}
impl Pixels {
    pub fn bytes_len(&self) -> usize {
        match self {
            Self::U8(v) => v.len(),
            Self::U16(v) => v.len() * 2,
        }
    }
    pub fn value(&self, index: usize) -> u16 {
        match self {
            Self::U8(v) => v[index] as u16 * 257,
            Self::U16(v) => v[index],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff::encoder::{TiffEncoder, colortype};

    #[test]
    fn gray16_profile_preserves_values() {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
            encoder
                .write_image::<colortype::Gray16>(4, 2, &[0, 1024, 32768, 65535, 10, 20, 30, 40])
                .unwrap();
        }
        let source = Source::Bytes(Arc::from(bytes.into_inner()));
        let info = inspect(&source, "gray16".into()).unwrap();
        assert_eq!(
            (info.width, info.height, info.bits, info.channels),
            (4, 2, 16, 1)
        );
        let chunk = Arc::new(read_chunk(&source, &info, 0).unwrap());
        assert_eq!(chunk.rgb(&info, 2, 0), [32768; 3]);
        let line = profile_with(&info, (0.0, 0.0), (3.0, 0.0), |_| Ok(chunk.clone())).unwrap();
        assert_eq!(
            line.iter().map(|p| p.1[0]).collect::<Vec<_>>(),
            vec![0, 1024, 32768, 65535]
        );
    }

    #[test]
    fn provided_large_deflate_tiff_decodes_by_strip() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stitched_bf.tiff");
        if !path.exists() {
            return;
        }
        let source = Source::Path(path);
        let info = inspect(&source, "stitched_bf.tiff".into()).unwrap();
        assert_eq!(
            (
                info.width,
                info.height,
                info.bits,
                info.channels,
                info.count()
            ),
            (15400, 27500, 8, 3, 20)
        );
        for index in [0, info.count() - 1] {
            let chunk = read_chunk(&source, &info, index).unwrap();
            assert_eq!(chunk.rect.w, 15400);
            assert_eq!(chunk.rect.h, 1375);
            let _ = chunk.rgb(&info, 0, 0);
            let _ = chunk.rgb(&info, chunk.rect.w - 1, chunk.rect.h - 1);
        }
    }
}
pub struct Chunk {
    pub rect: Rect,
    pub pixels: Pixels,
}
impl Chunk {
    pub fn rgb(&self, info: &Info, x: u32, y: u32) -> [u16; 3] {
        let index = (y as usize * self.rect.w as usize + x as usize) * info.channels;
        if info.channels == 1 {
            let v = self.pixels.value(index);
            let v = if info.white_is_zero { 65535 - v } else { v };
            [v, v, v]
        } else {
            [
                self.pixels.value(index),
                self.pixels.value(index + 1),
                self.pixels.value(index + 2),
            ]
        }
    }
}

pub fn read_chunk(source: &Source, info: &Info, index: u32) -> Result<Chunk, String> {
    #[cfg(not(target_arch = "wasm32"))]
    if let Source::PngGrid(grid) = source {
        return grid.read_chunk(index);
    }
    let mut decoder = source.decoder()?;
    let rect = info.rect(index);
    let pixels = match decoder.read_chunk(index).map_err(|e| e.to_string())? {
        DecodingResult::U8(v) => Pixels::U8(v),
        DecodingResult::U16(v) => Pixels::U16(v),
        other => return Err(format!("Unsupported chunk data type: {other:?}")),
    };
    let expected = rect.w as usize * rect.h as usize * info.channels;
    let actual = match &pixels {
        Pixels::U8(v) => v.len(),
        Pixels::U16(v) => v.len(),
    };
    if actual < expected {
        return Err(format!("Chunk {index} is incomplete"));
    }
    Ok(Chunk { rect, pixels })
}

pub fn sample_overview(info: &Info, chunk: &Chunk, step: u32) -> Vec<(usize, [u16; 3])> {
    let overview_width = info.width.div_ceil(step) as usize;
    let mut samples = Vec::new();
    let r = chunk.rect;
    for oy in r.y.div_ceil(step)..(r.y + r.h).div_ceil(step) {
        for ox in r.x.div_ceil(step)..(r.x + r.w).div_ceil(step) {
            let x = ox * step - r.x;
            let y = oy * step - r.y;
            if x < r.w && y < r.h {
                let mut sum = [0u32; 3];
                for sy in 0..3 {
                    for sx in 0..3 {
                        let px = (x + (sx * step / 3)).min(r.w - 1);
                        let py = (y + (sy * step / 3)).min(r.h - 1);
                        let rgb = chunk.rgb(info, px, py);
                        for c in 0..3 {
                            sum[c] += rgb[c] as u32;
                        }
                    }
                }
                samples.push((
                    oy as usize * overview_width + ox as usize,
                    [
                        sum[0].div_ceil(9) as u16,
                        sum[1].div_ceil(9) as u16,
                        sum[2].div_ceil(9) as u16,
                    ],
                ));
            }
        }
    }
    samples
}

pub fn profile_with<F>(
    info: &Info,
    a: (f32, f32),
    b: (f32, f32),
    mut load: F,
) -> Result<Vec<(f32, [u16; 3])>, String>
where
    F: FnMut(u32) -> Result<Arc<Chunk>, String>,
{
    let distance = ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let n = (distance.ceil() as usize + 1).clamp(2, 100_000);
    let mut points = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / (n - 1) as f32;
        let x = (a.0 + (b.0 - a.0) * t)
            .round()
            .clamp(0.0, (info.width - 1) as f32) as u32;
        let y = (a.1 + (b.1 - a.1) * t)
            .round()
            .clamp(0.0, (info.height - 1) as f32) as u32;
        points.push((i, distance * t, x, y, info.index_at(x, y)));
    }
    points.sort_unstable_by_key(|p| p.4);
    let mut values = vec![(0.0, [0; 3]); n];
    let mut current = u32::MAX;
    let mut chunk = None;
    for (i, d, x, y, index) in points {
        if index != current {
            chunk = Some(load(index)?);
            current = index;
        }
        let c = chunk.as_ref().unwrap();
        values[i] = (d, c.rgb(info, x - c.rect.x, y - c.rect.y));
    }
    Ok(values)
}
