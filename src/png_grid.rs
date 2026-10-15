use crate::tiff::{Chunk, Info, Pixels};
use image::DynamicImage;
use std::path::{Path, PathBuf};

pub struct PngGrid {
    paths: Vec<Option<PathBuf>>,
    tile_width: u32,
    tile_height: u32,
    bits: u8,
    columns: u32,
    rows: u32,
    rotate_tiles: bool,
    name: String,
}

fn coordinates(path: &Path) -> Option<(f64, f64)> {
    if !path.extension()?.eq_ignore_ascii_case("png") {
        return None;
    }
    let name = path.file_stem()?.to_str()?.strip_prefix("Scan_")?;
    let mut fields = name.split('_');
    let x: f64 = fields.next()?.parse().ok()?;
    let y: f64 = fields.next()?.parse().ok()?;
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

fn png_paths(dir: &Path) -> Result<Vec<(PathBuf, f64, f64)>, String> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_file() {
            if let Some((x, y)) = coordinates(&path) {
                paths.push((path, x, y));
            }
        }
    }
    paths.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(paths)
}

fn unique(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_unstable_by(f64::total_cmp);
    values.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    values
}

fn index(values: &[f64], value: f64) -> Result<usize, String> {
    values
        .iter()
        .position(|v| (*v - value).abs() < 1e-9)
        .ok_or_else(|| "Cannot map PNG coordinates to the grid".into())
}

impl PngGrid {
    pub fn directory(&self) -> Option<&Path> {
        self.paths.iter().flatten().next()?.parent()
    }

    pub fn open(dir: &Path) -> Result<Self, String> {
        let mut files = png_paths(dir)?;
        let mut selected_dir = dir.to_path_buf();
        if files.is_empty() {
            let bf = dir.join("bf");
            let df = dir.join("df");
            let bf_files = if bf.is_dir() {
                png_paths(&bf)?
            } else {
                Vec::new()
            };
            let df_files = if df.is_dir() {
                png_paths(&df)?
            } else {
                Vec::new()
            };
            match (bf_files.is_empty(), df_files.is_empty()) {
                (false, true) => {
                    files = bf_files;
                    selected_dir = bf;
                }
                (true, false) => {
                    files = df_files;
                    selected_dir = df;
                }
                (false, false) => return Err("Select the bf or df subdirectory".into()),
                (true, true) => return Err("No Scan_X_Y_*.png files found".into()),
            }
        }
        let dir = selected_dir.as_path();
        let first = image::open(&files[0].0).map_err(|e| e.to_string())?;
        let (tile_width, tile_height, bits) = match first {
            DynamicImage::ImageLuma8(ref im) => (im.width(), im.height(), 8),
            DynamicImage::ImageLuma16(ref im) => (im.width(), im.height(), 16),
            _ => return Err("PNG directory must contain 8-bit or 16-bit grayscale images".into()),
        };
        if tile_width == 0 || tile_height == 0 {
            return Err("Empty PNG tile".into());
        }
        let xs = unique(files.iter().map(|p| p.1).collect());
        let ys = unique(files.iter().map(|p| p.2).collect());
        let columns = u32::try_from(xs.len()).map_err(|e| e.to_string())?;
        let rows = u32::try_from(ys.len()).map_err(|e| e.to_string())?;
        columns.checked_mul(tile_width).ok_or("Image is too wide")?;
        rows.checked_mul(tile_height).ok_or("Image is too tall")?;
        let len = columns.checked_mul(rows).ok_or("Too many tiles")? as usize;
        let mut paths = vec![None; len];

        // Apply the same grid transformations as ScanStitcher-cpp.
        let config_path = dir
            .parent()
            .unwrap_or(dir)
            .join("raport/stitch_summary.json");
        let config = std::fs::read_to_string(config_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let flag = |key: &str, default| {
            config
                .as_ref()
                .and_then(|v| v.get("config"))
                .and_then(|v| v.get(key))
                .and_then(|v| v.as_bool())
                .unwrap_or(default)
        };
        let reverse_x = flag("reverse_x", false);
        let reverse_y = flag("reverse_y", false);
        let rotate_rows = flag("rotate_rows_180", true);
        let rotate_whole = flag("rotate_whole_180", true);
        let rotate_tiles = rotate_rows ^ rotate_whole;
        let flip_columns = reverse_x ^ rotate_rows ^ rotate_whole;
        let flip_rows = reverse_y ^ rotate_whole;
        for (path, x, y) in files {
            let mut col = index(&xs, x)? as u32;
            let mut row = index(&ys, y)? as u32;
            if flip_columns {
                col = columns - 1 - col;
            }
            if flip_rows {
                row = rows - 1 - row;
            }
            let slot = &mut paths[(row * columns + col) as usize];
            if slot.is_some() {
                return Err(format!("Two PNG files map to position ({col}, {row})"));
            }
            *slot = Some(path);
        }
        let name = dir.display().to_string();
        Ok(Self {
            paths,
            tile_width,
            tile_height,
            bits,
            columns,
            rows,
            rotate_tiles,
            name,
        })
    }

    pub fn info(&self) -> Info {
        Info {
            name: self.name.clone(),
            width: self.columns * self.tile_width,
            height: self.rows * self.tile_height,
            bits: self.bits,
            channels: 1,
            white_is_zero: false,
            chunk_width: self.tile_width,
            chunk_height: self.tile_height,
            chunks_x: self.columns,
            chunks_y: self.rows,
        }
    }

    pub fn read_chunk(&self, index: u32) -> Result<Chunk, String> {
        let info = self.info();
        let rect = info.rect(index);
        let count = rect.w as usize * rect.h as usize;
        let mut pixels = match self.paths.get(index as usize).ok_or("Invalid tile index")? {
            Some(path) => {
                let image = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
                if image.width() != rect.w || image.height() != rect.h {
                    return Err(format!("Invalid tile dimensions: {}", path.display()));
                }
                match (self.bits, image) {
                    (8, DynamicImage::ImageLuma8(im)) => Pixels::U8(im.into_raw()),
                    (16, DynamicImage::ImageLuma16(im)) => Pixels::U16(im.into_raw()),
                    _ => return Err(format!("Tile format does not match: {}", path.display())),
                }
            }
            None if self.bits == 8 => Pixels::U8(vec![0; count]),
            None => Pixels::U16(vec![0; count]),
        };
        if self.rotate_tiles {
            match &mut pixels {
                Pixels::U8(v) => v.reverse(),
                Pixels::U16(v) => v.reverse(),
            }
        }
        Ok(Chunk { rect, pixels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma};

    #[test]
    fn provided_experiment_uses_full_resolution_and_stitch_grid() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("experiment");
        if !root.join("bf").exists() {
            return;
        }
        let grid = PngGrid::open(&root).unwrap();
        let info = grid.info();
        assert_eq!(
            (info.width, info.height, info.bits, info.channels),
            (61600, 55000, 8, 1)
        );
        assert_eq!(info.count(), 700);
        let first = grid.read_chunk(0).unwrap();
        assert_eq!((first.rect.w, first.rect.h), (2200, 2200));
        assert!(matches!(first.pixels, Pixels::U8(_)));
    }

    #[test]
    fn grayscale_16_keeps_values_and_rotates_like_stitcher() {
        let root = std::env::temp_dir().join(format!("stitchviewer-grid-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let low = ImageBuffer::<Luma<u16>, Vec<u16>>::from_raw(2, 2, vec![1, 2, 3, 4]).unwrap();
        let high =
            ImageBuffer::<Luma<u16>, Vec<u16>>::from_raw(2, 2, vec![5, 6, 7, 65535]).unwrap();
        low.save(root.join("Scan_1.0_2.0_test.png")).unwrap();
        high.save(root.join("Scan_1.0_3.0_test.png")).unwrap();
        let grid = PngGrid::open(&root).unwrap();
        assert_eq!(grid.info().bits, 16);
        assert_eq!(grid.info().height, 4);
        let top = grid.read_chunk(0).unwrap();
        let bottom = grid.read_chunk(1).unwrap();
        assert_eq!(top.rgb(&grid.info(), 1, 1), [65535; 3]);
        assert_eq!(bottom.rgb(&grid.info(), 0, 0), [1; 3]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn report_rotation_rotates_pixels_without_resizing() {
        let root = std::env::temp_dir().join(format!("stitchviewer-rotate-{}", std::process::id()));
        let bf = root.join("bf");
        let report = root.join("raport");
        std::fs::create_dir_all(&bf).unwrap();
        std::fs::create_dir_all(&report).unwrap();
        std::fs::write(
            report.join("stitch_summary.json"),
            r#"{"config":{"rotate_rows_180":true,"rotate_whole_180":false}}"#,
        )
        .unwrap();
        let image = ImageBuffer::<Luma<u8>, Vec<u8>>::from_raw(2, 2, vec![1, 2, 3, 4]).unwrap();
        image.save(bf.join("Scan_1.0_2.0_test.png")).unwrap();
        let grid = PngGrid::open(&bf).unwrap();
        assert_eq!((grid.info().width, grid.info().height), (2, 2));
        let chunk = grid.read_chunk(0).unwrap();
        assert!(matches!(chunk.pixels, Pixels::U8(ref v) if v == &[4, 3, 2, 1]));
        std::fs::remove_dir_all(root).unwrap();
    }
}
