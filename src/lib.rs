#[cfg(not(target_arch = "wasm32"))]
mod png_grid;
mod tiff;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use std::io::Write;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
};
use tiff::{Chunk, Info, Source};

const TILE: u32 = 512;
const CACHE_TILES: usize = 96;
const MINIMAP_MAX: Vec2 = Vec2::new(220.0, 170.0);
type TileKey = (u8, u32, u32);
type BlockCache = Arc<Mutex<HashMap<u32, Arc<Chunk>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}
impl Selection {
    fn from_points(a: (f32, f32), b: (f32, f32), info: &Info) -> Option<Self> {
        let x0 = a.0.min(b.0).floor().clamp(0.0, info.width as f32) as u32;
        let y0 = a.1.min(b.1).floor().clamp(0.0, info.height as f32) as u32;
        let x1 = a.0.max(b.0).ceil().clamp(0.0, info.width as f32) as u32;
        let y1 = a.1.max(b.1).ceil().clamp(0.0, info.height as f32) as u32;
        (x0 < x1 && y0 < y1).then_some(Self { x0, y0, x1, y1 })
    }
    fn width(self) -> u32 {
        self.x1 - self.x0
    }
    fn height(self) -> u32 {
        self.y1 - self.y0
    }
}

fn selection_blocks(
    cache: &BlockCache,
    info: &Info,
    area: Selection,
) -> Result<BlockCache, String> {
    let blocks = cache.lock().unwrap();
    let mut selected = HashMap::new();
    for cy in area.y0 / info.chunk_height..=(area.y1 - 1) / info.chunk_height {
        for cx in area.x0 / info.chunk_width..=(area.x1 - 1) / info.chunk_width {
            let index = cy * info.chunks_x + cx;
            selected.insert(
                index,
                blocks
                    .get(&index)
                    .cloned()
                    .ok_or_else(|| format!("Image chunk {index} is not loaded yet"))?,
            );
        }
    }
    Ok(Arc::new(Mutex::new(selected)))
}

fn write_selection_png<W: std::io::Write>(
    output: W,
    info: &Info,
    cache: &BlockCache,
    area: Selection,
) -> Result<(), String> {
    let color = match info.channels {
        1 => png::ColorType::Grayscale,
        3 => png::ColorType::Rgb,
        4 => png::ColorType::Rgba,
        _ => return Err("Unsupported channel count for PNG".into()),
    };
    let depth = match info.bits {
        8 => png::BitDepth::Eight,
        16 => png::BitDepth::Sixteen,
        _ => return Err("Unsupported bit depth for PNG".into()),
    };
    let row_len = (area.width() as usize)
        .checked_mul(info.channels)
        .and_then(|size| size.checked_mul((info.bits / 8) as usize))
        .ok_or("Selected image is too wide")?;
    let mut encoder = png::Encoder::new(output, area.width(), area.height());
    encoder.set_color(color);
    encoder.set_depth(depth);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    let mut stream = writer.stream_writer().map_err(|error| error.to_string())?;
    let mut row = Vec::with_capacity(row_len);
    for y in area.y0..area.y1 {
        row.clear();
        let mut x = area.x0;
        while x < area.x1 {
            let chunk = cached_chunk(cache, info.index_at(x, y))?;
            let end = area.x1.min(chunk.rect.x + chunk.rect.w);
            for px in x..end {
                let base = ((y - chunk.rect.y) as usize * chunk.rect.w as usize
                    + (px - chunk.rect.x) as usize)
                    * info.channels;
                for channel in 0..info.channels {
                    let mut value = chunk.pixels.value(base + channel);
                    if info.channels == 1 && info.white_is_zero {
                        value = 65535 - value;
                    }
                    if info.bits == 8 {
                        row.push((value / 257) as u8);
                    } else {
                        row.extend_from_slice(&value.to_be_bytes());
                    }
                }
            }
            x = end;
        }
        stream.write_all(&row).map_err(|error| error.to_string())?;
    }
    stream.finish().map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn web_tiff_url() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search)
        .ok()?
        .get("tiff")
        .filter(|url| !url.trim().is_empty())
}

#[cfg(target_arch = "wasm32")]
async fn fetch_tiff(url: &str) -> Result<Arc<[u8]>, String> {
    use wasm_bindgen::JsCast;
    let window = web_sys::window().ok_or("Browser window is unavailable")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| format!("Connection to {url} failed: {e:?}"))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| "Invalid HTTP response")?;
    if !response.ok() {
        return Err(format!("HTTP {} for {url}", response.status()));
    }
    let buffer = response
        .array_buffer()
        .map_err(|e| format!("Could not read response: {e:?}"))?;
    let buffer = wasm_bindgen_futures::JsFuture::from(buffer)
        .await
        .map_err(|e| format!("Could not read response data: {e:?}"))?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    if bytes.is_empty() {
        return Err("Empty HTTP response".into());
    }
    Ok(bytes.into())
}

#[cfg(not(target_arch = "wasm32"))]
fn native_source(path: &std::path::Path) -> Result<Source, String> {
    if path.is_dir() {
        png_grid::PngGrid::open(path).map(|grid| Source::PngGrid(Arc::new(grid)))
    } else {
        Ok(Source::Path(path.to_path_buf()))
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn last_directory_file() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| std::path::PathBuf::from(home).join("AppData/Roaming"))
        });
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        });
    base.map(|dir| dir.join("StitchViewer/last-directory"))
}

#[cfg(not(target_arch = "wasm32"))]
fn read_last_directory(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let directory = std::path::PathBuf::from(std::fs::read_to_string(path).ok()?);
    directory.is_dir().then_some(directory)
}

#[cfg(not(target_arch = "wasm32"))]
fn save_last_directory(path: &std::path::Path, directory: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, directory.to_string_lossy().as_bytes())
}

#[cfg(not(target_arch = "wasm32"))]
fn source_directory(source: &Source) -> Option<std::path::PathBuf> {
    let directory = match source {
        Source::Path(path) => path.canonicalize().ok()?.parent()?.to_path_buf(),
        Source::PngGrid(grid) => grid.directory()?.to_path_buf(),
        Source::Bytes(_) => return None,
    };
    directory.canonicalize().ok()
}

enum Job {
    Open(u64, Source, String),
    Overview(u64, Source, Arc<Info>, u32, u32),
    Detail(u64, Arc<Info>, Vec<TileKey>, u16, u16),
    Profile(u64, Arc<Info>, (f32, f32), (f32, f32)),
}
impl Job {
    fn generation(&self) -> u64 {
        match self {
            Self::Open(id, ..)
            | Self::Overview(id, ..)
            | Self::Detail(id, ..)
            | Self::Profile(id, ..) => *id,
        }
    }
}
struct Tile {
    key: TileKey,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    pixel_w: u32,
    pixel_h: u32,
    rgba: Vec<u8>,
}
enum Message {
    Chosen(Source, String),
    #[cfg(not(target_arch = "wasm32"))]
    ChooseError(String),
    #[cfg(target_arch = "wasm32")]
    Fetched(u64, String, Result<Arc<[u8]>, String>),
    Opened(u64, Result<Info, String>),
    Overview(u64, Vec<(usize, [u16; 3])>),
    Detail(u64, Vec<Tile>),
    Profile(u64, Result<Vec<(f32, [u16; 3])>, String>),
    Exported(u64, Result<String, String>),
}

fn color(rgb: [u16; 3], low: u16, high: u16) -> Color32 {
    let channel = |v: u16| {
        ((v.saturating_sub(low) as u32 * 255) / (high.saturating_sub(low).max(1) as u32)).min(255)
            as u8
    };
    Color32::from_rgb(channel(rgb[0]), channel(rgb[1]), channel(rgb[2]))
}
fn tile_level(zoom: f32) -> u8 {
    let mut level = 0;
    while level < 16 && zoom * ((1u32 << (level + 1)) as f32) <= 0.75 {
        level += 1;
    }
    level
}

fn make_tile(
    info: &Info,
    cache: &BlockCache,
    key: TileKey,
    low: u16,
    high: u16,
) -> Result<Tile, String> {
    let (level, tx, ty) = key;
    let scale = 1u32 << level;
    let span = TILE * scale;
    let x = tx * span;
    let y = ty * span;
    let w = span.min(info.width - x);
    let h = span.min(info.height - y);
    let pixel_w = w.div_ceil(scale);
    let pixel_h = h.div_ceil(scale);
    let cx0 = x / info.chunk_width;
    let cy0 = y / info.chunk_height;
    let cx1 = (x + w - 1) / info.chunk_width;
    let cy1 = (y + h - 1) / info.chunk_height;
    let chunk_cols = (cx1 - cx0 + 1) as usize;
    let mut chunks = Vec::with_capacity(chunk_cols * (cy1 - cy0 + 1) as usize);
    {
        let blocks = cache.lock().unwrap();
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                let index = cy * info.chunks_x + cx;
                chunks.push(
                    blocks
                        .get(&index)
                        .cloned()
                        .ok_or_else(|| format!("Image chunk {index} is not loaded yet"))?,
                );
            }
        }
    }
    let mut rgba = Vec::with_capacity(pixel_w as usize * pixel_h as usize * 4);
    for row in 0..pixel_h {
        let sy = (y + row * scale + scale / 2).min(info.height - 1);
        let cy = sy / info.chunk_height;
        for col in 0..pixel_w {
            let sx = (x + col * scale + scale / 2).min(info.width - 1);
            let cx = sx / info.chunk_width;
            let chunk = &chunks[((cy - cy0) as usize) * chunk_cols + (cx - cx0) as usize];
            let c = color(
                chunk.rgb(info, sx - chunk.rect.x, sy - chunk.rect.y),
                low,
                high,
            );
            rgba.extend_from_slice(&[c.r(), c.g(), c.b(), 255]);
        }
    }
    Ok(Tile {
        key,
        x,
        y,
        w,
        h,
        pixel_w,
        pixel_h,
        rgba,
    })
}
fn cached_chunk(cache: &BlockCache, index: u32) -> Result<Arc<Chunk>, String> {
    cache
        .lock()
        .unwrap()
        .get(&index)
        .cloned()
        .ok_or_else(|| format!("Image chunk {index} is not loaded yet"))
}
fn run_job(job: Job, cache: &BlockCache, generation: &AtomicU64) -> Message {
    match job {
        Job::Open(id, source, name) => Message::Opened(id, tiff::inspect(&source, name)),
        Job::Overview(id, source, info, index, step) => {
            let result = tiff::read_chunk(&source, &info, index).map(|chunk| {
                let samples = tiff::sample_overview(&info, &chunk, step);
                let mut blocks = cache.lock().unwrap();
                if generation.load(Ordering::Relaxed) == id {
                    blocks.insert(index, Arc::new(chunk));
                }
                samples
            });
            match result {
                Ok(samples) => Message::Overview(id, samples),
                Err(e) => Message::Opened(id, Err(e)),
            }
        }
        Job::Detail(id, info, keys, low, high) => {
            let result = keys
                .into_iter()
                .map(|key| make_tile(&info, cache, key, low, high))
                .collect::<Result<Vec<_>, _>>();
            match result {
                Ok(tiles) => Message::Detail(id, tiles),
                Err(e) => Message::Opened(id, Err(e)),
            }
        }
        Job::Profile(id, info, a, b) => {
            let result = tiff::profile_with(&info, a, b, |index| cached_chunk(cache, index));
            Message::Profile(id, result)
        }
    }
}

struct Jobs {
    output: Arc<Mutex<VecDeque<Message>>>,
    generation: Arc<AtomicU64>,
    detail_level: Arc<AtomicU8>,
    cache: BlockCache,
    #[cfg(not(target_arch = "wasm32"))]
    sender: std::sync::mpsc::Sender<Job>,
}
impl Jobs {
    fn new(ctx: egui::Context) -> Self {
        let output = Arc::new(Mutex::new(VecDeque::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let detail_level = Arc::new(AtomicU8::new(u8::MAX));
        let cache: BlockCache = Arc::new(Mutex::new(HashMap::new()));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (sender, receiver) = std::sync::mpsc::channel::<Job>();
            let receiver = Arc::new(Mutex::new(receiver));
            let workers = std::thread::available_parallelism()
                .map(|v| v.get())
                .unwrap_or(1);
            for _ in 0..workers {
                let receiver = receiver.clone();
                let output = output.clone();
                let ctx = ctx.clone();
                let generation = generation.clone();
                let detail_level = detail_level.clone();
                let cache = cache.clone();
                std::thread::spawn(move || {
                    loop {
                        let job = { receiver.lock().unwrap().recv() };
                        let Ok(job) = job else { break };
                        if job.generation() != generation.load(Ordering::Relaxed) {
                            continue;
                        }
                        if matches!(&job, Job::Detail(_, _, keys, _, _) if keys[0].0 != detail_level.load(Ordering::Relaxed))
                        {
                            continue;
                        }
                        let msg = run_job(job, &cache, &generation);
                        output.lock().unwrap().push_back(msg);
                        ctx.request_repaint();
                    }
                });
            }
            Self {
                output,
                generation,
                detail_level,
                cache,
                sender,
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            Self {
                output,
                generation,
                detail_level,
                cache,
            }
        }
    }
    fn send(&self, job: Job, ctx: &egui::Context) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = ctx;
            let _ = self.sender.send(job);
        }
        #[cfg(target_arch = "wasm32")]
        {
            let output = self.output.clone();
            let generation = self.generation.clone();
            let detail_level = self.detail_level.clone();
            let cache = self.cache.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(1).await;
                if job.generation() != generation.load(Ordering::Relaxed) {
                    return;
                }
                if matches!(&job, Job::Detail(_, _, keys, _, _) if keys[0].0 != detail_level.load(Ordering::Relaxed))
                {
                    return;
                }
                output
                    .lock()
                    .unwrap()
                    .push_back(run_job(job, &cache, &generation));
                ctx.request_repaint();
            });
        }
    }
    fn pop(&self) -> Option<Message> {
        self.output.lock().unwrap().pop_front()
    }
    fn set_generation(&self, id: u64) {
        self.generation.store(id, Ordering::Relaxed);
        self.detail_level.store(u8::MAX, Ordering::Relaxed);
        self.cache.lock().unwrap().clear();
    }
    fn set_detail_level(&self, level: u8) {
        self.detail_level.store(level, Ordering::Relaxed);
    }
    fn cache_stats(&self) -> (usize, usize) {
        let cache = self.cache.lock().unwrap();
        (
            cache.len(),
            cache.values().map(|chunk| chunk.pixels.bytes_len()).sum(),
        )
    }
}

struct CachedTile {
    texture: TextureHandle,
    level: u8,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}
pub struct Viewer {
    jobs: Jobs,
    #[cfg(not(target_arch = "wasm32"))]
    last_directory: Option<std::path::PathBuf>,
    #[cfg(not(target_arch = "wasm32"))]
    remember_directory: bool,
    generation: u64,
    source: Option<Source>,
    info: Option<Arc<Info>>,
    status: String,
    #[cfg(target_arch = "wasm32")]
    url_input: String,
    #[cfg(target_arch = "wasm32")]
    url_request: u64,
    #[cfg(target_arch = "wasm32")]
    embedded: bool,
    step: u32,
    overview: Vec<[u16; 3]>,
    overview_texture: Option<TextureHandle>,
    overview_done: u32,
    low: u16,
    high: u16,
    tiles: HashMap<TileKey, CachedTile>,
    order: VecDeque<TileKey>,
    pending: HashSet<TileKey>,
    active_level: Option<u8>,
    fallback_level: Option<u8>,
    zoom: f32,
    origin: Vec2,
    fit_pending: bool,
    line: Option<((f32, f32), (f32, f32))>,
    line_start: Option<(f32, f32)>,
    profile: Option<Vec<(f32, [u16; 3])>>,
    select_mode: bool,
    selection_start: Option<(f32, f32)>,
    selection_end: Option<(f32, f32)>,
    selection: Option<Selection>,
    minimap_drag: bool,
}
impl Viewer {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut this = Self {
            jobs: Jobs::new(cc.egui_ctx.clone()),
            #[cfg(not(target_arch = "wasm32"))]
            last_directory: last_directory_file().and_then(|path| read_last_directory(&path)),
            #[cfg(not(target_arch = "wasm32"))]
            remember_directory: false,
            generation: 0,
            source: None,
            info: None,
            status: "Open a TIFF file or PNG directory".into(),
            #[cfg(target_arch = "wasm32")]
            url_input: String::new(),
            #[cfg(target_arch = "wasm32")]
            url_request: 0,
            #[cfg(target_arch = "wasm32")]
            embedded: false,
            step: 1,
            overview: Vec::new(),
            overview_texture: None,
            overview_done: 0,
            low: 0,
            high: 65535,
            tiles: HashMap::new(),
            order: VecDeque::new(),
            pending: HashSet::new(),
            active_level: None,
            fallback_level: None,
            zoom: 1.0,
            origin: Vec2::ZERO,
            fit_pending: true,
            line: None,
            line_start: None,
            profile: None,
            select_mode: false,
            selection_start: None,
            selection_end: None,
            selection: None,
            minimap_drag: false,
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = std::env::args_os()
                .skip(1)
                .filter(|arg| arg != "--")
                .last()
                .map(std::path::PathBuf::from);
            let remember_directory = path.is_some();
            let path = path.or_else(|| {
                let path = std::path::PathBuf::from("stitched_bf.tiff");
                path.exists().then_some(path)
            });
            if let Some(path) = path {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                match native_source(&path) {
                    Ok(source) => this.open(source, name, remember_directory, &cc.egui_ctx),
                    Err(e) => this.status = format!("Error: {e}"),
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            this.status = "Open a TIFF file or enter a URL".into();
            if let Some(url) = web_tiff_url() {
                this.embedded = true;
                this.url_input = url.clone();
                this.load_url(url, &cc.egui_ctx);
            }
        }
        this
    }
    fn open(
        &mut self,
        source: Source,
        name: String,
        remember_directory: bool,
        ctx: &egui::Context,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.remember_directory = remember_directory;
        }
        #[cfg(target_arch = "wasm32")]
        let _ = remember_directory;
        self.generation += 1;
        self.jobs.set_generation(self.generation);
        self.source = Some(source.clone());
        self.info = None;
        self.status = format!("Opening {name}…");
        self.overview.clear();
        self.overview_texture = None;
        self.overview_done = 0;
        self.tiles.clear();
        self.order.clear();
        self.pending.clear();
        self.active_level = None;
        self.fallback_level = None;
        self.profile = None;
        self.line = None;
        self.selection = None;
        self.selection_start = None;
        self.selection_end = None;
        self.fit_pending = true;
        self.jobs
            .send(Job::Open(self.generation, source, name), ctx);
    }
    fn choose_file(&self, ctx: &egui::Context) {
        let output = self.jobs.output.clone();
        let ctx = ctx.clone();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let last_directory = self.last_directory.clone();
            std::thread::spawn(move || {
                let mut dialog = rfd::FileDialog::new().add_filter("TIFF", &["tif", "tiff"]);
                if let Some(directory) = last_directory {
                    dialog = dialog.set_directory(directory);
                }
                let file = dialog.pick_file();
                if let Some(path) = file {
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    output
                        .lock()
                        .unwrap()
                        .push_back(Message::Chosen(Source::Path(path), name));
                    ctx.request_repaint();
                }
            });
        }
        #[cfg(target_arch = "wasm32")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(file) = rfd::AsyncFileDialog::new()
                    .add_filter("TIFF", &["tif", "tiff"])
                    .pick_file()
                    .await
                {
                    let name = file.file_name();
                    let bytes: Arc<[u8]> = file.read().await.into();
                    output
                        .lock()
                        .unwrap()
                        .push_back(Message::Chosen(Source::Bytes(bytes), name));
                    ctx.request_repaint();
                }
            });
        }
    }
    fn save_selection(&mut self, ctx: &egui::Context) {
        let (Some(area), Some(info)) = (self.selection, self.info.clone()) else {
            return;
        };
        if self.overview_done != info.count() {
            self.status = "Image is still loading".into();
            return;
        }
        let cache = match selection_blocks(&self.jobs.cache, &info, area) {
            Ok(cache) => cache,
            Err(error) => {
                self.status = format!("Export error: {error}");
                return;
            }
        };
        let filename = format!(
            "stitchviewer-{}-{}-{}x{}.png",
            area.x0,
            area.y0,
            area.width(),
            area.height()
        );
        let output = self.jobs.output.clone();
        let ctx = ctx.clone();
        let generation = self.generation;
        self.status = format!("Saving {filename}…");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let last_directory = self.last_directory.clone();
            std::thread::spawn(move || {
                let mut dialog = rfd::FileDialog::new()
                    .add_filter("PNG", &["png"])
                    .set_file_name(&filename);
                if let Some(directory) = last_directory {
                    dialog = dialog.set_directory(directory);
                }
                let result = match dialog.save_file() {
                    Some(mut path) => {
                        if path.extension().is_none() {
                            path.set_extension("png");
                        }
                        std::fs::File::create(&path)
                            .map_err(|error| error.to_string())
                            .and_then(|file| write_selection_png(file, &info, &cache, area))
                            .map(|()| format!("Saved {}", path.display()))
                    }
                    None => Ok("Export cancelled".into()),
                };
                output
                    .lock()
                    .unwrap()
                    .push_back(Message::Exported(generation, result));
                ctx.request_repaint();
            });
        }
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async move {
            let mut bytes = Vec::new();
            let result = match write_selection_png(&mut bytes, &info, &cache, area) {
                Ok(()) => match rfd::AsyncFileDialog::new()
                    .add_filter("PNG", &["png"])
                    .set_file_name(&filename)
                    .save_file()
                    .await
                {
                    Some(file) => file
                        .write(&bytes)
                        .await
                        .map(|()| format!("Saved {filename}"))
                        .map_err(|error| error.to_string()),
                    None => Ok("Export cancelled".into()),
                },
                Err(error) => Err(error),
            };
            output
                .lock()
                .unwrap()
                .push_back(Message::Exported(generation, result));
            ctx.request_repaint();
        });
    }
    #[cfg(target_arch = "wasm32")]
    fn load_url(&mut self, url: String, ctx: &egui::Context) {
        let url = url.trim().to_owned();
        if url.is_empty() {
            self.status = "Enter a TIFF URL".into();
            return;
        }
        self.url_request += 1;
        let id = self.url_request;
        self.status = format!("Downloading {url}…");
        let output = self.jobs.output.clone();
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = fetch_tiff(&url).await;
            output
                .lock()
                .unwrap()
                .push_back(Message::Fetched(id, url, result));
            ctx.request_repaint();
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn choose_folder(&self, ctx: &egui::Context) {
        let output = self.jobs.output.clone();
        let ctx = ctx.clone();
        let last_directory = self.last_directory.clone();
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(directory) = last_directory {
                dialog = dialog.set_directory(directory);
            }
            if let Some(path) = dialog.pick_folder() {
                let name = path.display().to_string();
                let msg = match native_source(&path) {
                    Ok(source) => Message::Chosen(source, name),
                    Err(e) => Message::ChooseError(e),
                };
                output.lock().unwrap().push_back(msg);
                ctx.request_repaint();
            }
        });
    }
    fn receive(&mut self, ctx: &egui::Context) {
        while let Some(msg) = self.jobs.pop() {
            match msg {
                Message::Chosen(source, name) => {
                    #[cfg(target_arch = "wasm32")]
                    {
                        self.url_request += 1;
                    }
                    self.open(source, name, true, ctx)
                }
                #[cfg(not(target_arch = "wasm32"))]
                Message::ChooseError(e) => self.status = format!("Error: {e}"),
                #[cfg(target_arch = "wasm32")]
                Message::Fetched(id, url, result) if id == self.url_request => match result {
                    Ok(bytes) => self.open(Source::Bytes(bytes), url, false, ctx),
                    Err(e) => self.status = format!("Download error: {e}"),
                },
                Message::Opened(id, result) if id == self.generation => match result {
                    Ok(info) => {
                        #[cfg(not(target_arch = "wasm32"))]
                        if let Some(directory) = self
                            .source
                            .as_ref()
                            .filter(|_| self.remember_directory)
                            .and_then(source_directory)
                        {
                            if self.last_directory.as_ref() != Some(&directory) {
                                self.last_directory = Some(directory.clone());
                                if let Some(path) = last_directory_file() {
                                    if let Err(error) = save_last_directory(&path, &directory) {
                                        eprintln!("Could not save last directory: {error}");
                                    }
                                }
                            }
                        }
                        self.step = info
                            .width
                            .div_ceil(2048)
                            .max(info.height.div_ceil(2048))
                            .max(1);
                        self.overview = vec![
                            [0; 3];
                            (info.width.div_ceil(self.step) * info.height.div_ceil(self.step))
                                as usize
                        ];
                        self.low = 0;
                        self.high = 65535;
                        self.status = format!(
                            "{}: {} × {}, {} bit, {} chunks",
                            info.name,
                            info.width,
                            info.height,
                            info.bits,
                            info.count()
                        );
                        let info = Arc::new(info);
                        self.info = Some(info.clone());
                        for index in 0..info.count() {
                            self.jobs.send(
                                Job::Overview(
                                    id,
                                    self.source.as_ref().unwrap().clone(),
                                    info.clone(),
                                    index,
                                    self.step,
                                ),
                                ctx,
                            );
                        }
                    }
                    Err(e) => {
                        self.status = format!("Error: {e}");
                        self.pending.clear();
                    }
                },
                Message::Overview(id, samples) if id == self.generation => {
                    for (index, rgb) in samples {
                        if let Some(slot) = self.overview.get_mut(index) {
                            *slot = rgb;
                        }
                    }
                    self.overview_done += 1;
                    if let Some(info) = &self.info {
                        if self.overview_done == info.count()
                            && info.bits == 16
                            && info.channels == 1
                        {
                            self.auto_contrast();
                        }
                    }
                    if let Some(info) = &self.info {
                        let count = info.count();
                        let interval = (count / 30).max(1);
                        if self.overview_done % interval == 0 || self.overview_done == count {
                            self.update_overview(ctx);
                        }
                        if self.overview_done == count {
                            self.overview = Vec::new();
                        }
                    }
                }
                Message::Detail(id, tiles) if id == self.generation => {
                    for tile in tiles {
                        self.pending.remove(&tile.key);
                        if self.active_level != Some(tile.key.0) {
                            continue;
                        }
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [tile.pixel_w as usize, tile.pixel_h as usize],
                            &tile.rgba,
                        );
                        let texture = ctx.load_texture(
                            format!("tile-{}-{:?}", id, tile.key),
                            image,
                            TextureOptions::LINEAR,
                        );
                        self.tiles.insert(
                            tile.key,
                            CachedTile {
                                texture,
                                level: tile.key.0,
                                x: tile.x,
                                y: tile.y,
                                w: tile.w,
                                h: tile.h,
                            },
                        );
                        self.order.retain(|k| *k != tile.key);
                        self.order.push_back(tile.key);
                    }
                    while self.order.len() > CACHE_TILES {
                        if let Some(key) = self.order.pop_front() {
                            self.tiles.remove(&key);
                        }
                    }
                }
                Message::Profile(id, result) if id == self.generation => match result {
                    Ok(data) => self.profile = Some(data),
                    Err(e) => self.status = format!("Profile error: {e}"),
                },
                Message::Exported(id, result) if id == self.generation => {
                    self.status = match result {
                        Ok(message) => message,
                        Err(error) => format!("Export error: {error}"),
                    };
                }
                _ => {}
            }
        }
    }
    fn auto_contrast(&mut self) {
        let mut bins = vec![0u32; 65536];
        for p in &self.overview {
            bins[p[0] as usize] += 1;
        }
        let count = self.overview.len() as u32;
        let lo_target = count / 200;
        let hi_target = count - count / 200;
        let mut sum = 0;
        for (i, n) in bins.iter().enumerate() {
            sum += n;
            if sum >= lo_target {
                self.low = i as u16;
                break;
            }
        }
        sum = 0;
        for (i, n) in bins.iter().enumerate() {
            sum += n;
            if sum >= hi_target {
                self.high = i as u16;
                break;
            }
        }
        if self.high <= self.low {
            self.low = 0;
            self.high = 65535;
        }
    }
    fn update_overview(&mut self, ctx: &egui::Context) {
        let Some(info) = &self.info else { return };
        let size = [
            info.width.div_ceil(self.step) as usize,
            info.height.div_ceil(self.step) as usize,
        ];
        let pixels = self
            .overview
            .iter()
            .map(|p| color(*p, self.low, self.high))
            .collect();
        let image = egui::ColorImage::new(size, pixels);
        if let Some(texture) = &mut self.overview_texture {
            texture.set(image, TextureOptions::LINEAR);
        } else {
            self.overview_texture =
                Some(ctx.load_texture("overview", image, TextureOptions::LINEAR));
        }
    }
    fn fit(&mut self, rect: Rect, info: &Info) {
        self.zoom = (rect.width() / info.width as f32)
            .min(rect.height() / info.height as f32)
            .max(0.001);
        self.origin = Vec2::new(
            (rect.width() - info.width as f32 * self.zoom) / 2.0,
            (rect.height() - info.height as f32 * self.zoom) / 2.0,
        );
        self.fit_pending = false;
    }
    fn image_point(&self, rect: Rect, screen: Pos2) -> (f32, f32) {
        (
            (screen.x - rect.min.x - self.origin.x) / self.zoom,
            (screen.y - rect.min.y - self.origin.y) / self.zoom,
        )
    }
    fn image_rect(&self, canvas: Rect, x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect::from_min_size(
            canvas.min + self.origin + Vec2::new(x as f32 * self.zoom, y as f32 * self.zoom),
            Vec2::new(w as f32 * self.zoom, h as f32 * self.zoom),
        )
    }
    fn paint_tiles(&self, painter: &egui::Painter, canvas: Rect, level: u8) {
        for tile in self.tiles.values() {
            if tile.level != level {
                continue;
            }
            let image_rect = self.image_rect(canvas, tile.x, tile.y, tile.w, tile.h);
            if canvas.intersects(image_rect) {
                painter.image(
                    tile.texture.id(),
                    image_rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        }
    }
    fn request_tiles(&mut self, canvas: Rect, ctx: &egui::Context) -> Option<(u8, bool)> {
        let Some(info) = self.info.clone() else {
            return None;
        };
        if self.overview_done < info.count() {
            return None;
        }
        let (ax, ay) = self.image_point(canvas, canvas.min);
        let (bx, by) = self.image_point(canvas, canvas.max);
        let x0 = ax.floor().max(0.0) as u32;
        let y0 = ay.floor().max(0.0) as u32;
        let x1 = bx.ceil().min(info.width as f32) as u32;
        let y1 = by.ceil().min(info.height as f32) as u32;
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let mut level = tile_level(self.zoom);
        let mut span = TILE * (1u32 << level);
        while level < 16
            && ((x1 - 1) / span - x0 / span + 1) as usize
                * ((y1 - 1) / span - y0 / span + 1) as usize
                > CACHE_TILES / 2
        {
            level += 1;
            span *= 2;
        }
        if self.active_level != Some(level) {
            self.active_level = Some(level);
            self.pending.clear();
            self.jobs.set_detail_level(level);
        }
        let complete = self.queue_tile_region(info.clone(), level, (x0, y0, x1, y1), ctx);
        let margin = 2 * span;
        let expanded = (
            x0.saturating_sub(margin),
            y0.saturating_sub(margin),
            x1.saturating_add(margin).min(info.width),
            y1.saturating_add(margin).min(info.height),
        );
        let expanded_tiles = (expanded.2 - expanded.0).div_ceil(span) as usize
            * (expanded.3 - expanded.1).div_ceil(span) as usize;
        if expanded_tiles <= CACHE_TILES / 2 {
            self.queue_tile_region(info.clone(), level, expanded, ctx);
        }
        if complete {
            self.fallback_level = Some(level);
        }
        Some((level, complete))
    }
    fn queue_tile_region(
        &mut self,
        info: Arc<Info>,
        level: u8,
        (x0, y0, x1, y1): (u32, u32, u32, u32),
        ctx: &egui::Context,
    ) -> bool {
        let span = TILE * (1u32 << level);
        let mut complete = true;
        for ty in y0 / span..=(y1 - 1) / span {
            for tx in x0 / span..=(x1 - 1) / span {
                let key = (level, tx, ty);
                if !self.tiles.contains_key(&key) {
                    complete = false;
                    if self.pending.insert(key) {
                        self.jobs.send(
                            Job::Detail(
                                self.generation,
                                info.clone(),
                                vec![key],
                                self.low,
                                self.high,
                            ),
                            ctx,
                        );
                    }
                }
            }
        }
        complete
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            #[cfg(target_arch = "wasm32")]
            if !self.embedded && ui.button("Open TIFF").clicked() {
                self.choose_file(ui.ctx());
            }
            #[cfg(not(target_arch = "wasm32"))]
            if ui.button("Open TIFF").clicked() {
                self.choose_file(ui.ctx());
            }
            #[cfg(not(target_arch = "wasm32"))]
            if ui.button("Open PNG directory").clicked() {
                self.choose_folder(ui.ctx());
            }
            if ui.button("Fit").clicked() {
                self.fit_pending = true;
            }
            if ui.button("1:1").clicked() {
                self.zoom = 1.0;
                self.origin = Vec2::ZERO;
            }
            ui.toggle_value(&mut self.select_mode, "Select area");
            if ui
                .add_enabled(
                    self.selection.is_some()
                        && self
                            .info
                            .as_ref()
                            .is_some_and(|info| self.overview_done == info.count()),
                    egui::Button::new("Save PNG"),
                )
                .clicked()
            {
                self.save_selection(ui.ctx());
            }
            ui.separator();
            ui.label(format!("{:.1}%", self.zoom * 100.0));
            if let Some(info) = &self.info {
                ui.separator();
                ui.label(format!(
                    "{} × {} · {} bit · {}/{} chunks",
                    info.width,
                    info.height,
                    info.bits,
                    self.overview_done,
                    info.count()
                ));
                let (loaded, bytes) = self.jobs.cache_stats();
                ui.separator();
                ui.label(format!(
                    "RAM: {loaded}/{} chunks · {:.2} GiB",
                    info.count(),
                    bytes as f64 / 1073741824.0
                ));
            }
        });
        #[cfg(target_arch = "wasm32")]
        if !self.embedded {
            ui.horizontal(|ui| {
                ui.label("TIFF URL:");
                let width = (ui.available_width() - 90.0).max(120.0);
                let input =
                    ui.add(egui::TextEdit::singleline(&mut self.url_input).desired_width(width));
                let submit = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Download").clicked() || submit {
                    self.load_url(self.url_input.clone(), ui.ctx());
                }
            });
        }
        #[cfg(target_arch = "wasm32")]
        let open_help = if self.embedded {
            ""
        } else {
            " · Ctrl+O: open"
        };
        #[cfg(not(target_arch = "wasm32"))]
        let open_help = " · Ctrl+O: open";
        ui.label(format!("Left drag: pan · Alt+left drag or Select area: select region · Ctrl+S: save PNG · wheel: zoom at cursor · Ctrl+wheel: pan (+Shift: horizontal) · right drag: intensity profile (Esc: close) · arrows: pan · F: fit · 1: 1:1{open_help}"));
        ui.label(&self.status);
    }

    fn draw_profile(&self, ui: &mut egui::Ui) {
        let Some(data) = &self.profile else { return };
        let size = Vec2::new(ui.available_width(), 170.0);
        let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(20, 23, 28));
        let inner = rect.shrink2(Vec2::new(38.0, 12.0));
        let max_distance = data.last().map(|p| p.0).unwrap_or(1.0).max(1.0);
        let lo = self.low as f32;
        let range = (self.high - self.low).max(1) as f32;
        let display_divisor = if self.info.as_ref().is_some_and(|i| i.bits == 8) {
            257
        } else {
            1
        };
        let channels = if self.info.as_ref().is_some_and(|i| i.channels == 1) {
            1
        } else {
            3
        };
        let colors = [Color32::RED, Color32::GREEN, Color32::LIGHT_BLUE];
        for channel in 0..channels {
            let step = (data.len() / inner.width().max(1.0) as usize).max(1);
            let points: Vec<Pos2> = data
                .iter()
                .step_by(step)
                .map(|(d, rgb)| {
                    let x = inner.left() + d / max_distance * inner.width();
                    let y = inner.bottom()
                        - ((rgb[channel] as f32 - lo) / range).clamp(0.0, 1.0) * inner.height();
                    Pos2::new(x, y)
                })
                .collect();
            painter.add(egui::Shape::line(
                points,
                egui::Stroke::new(
                    1.4,
                    if channels == 1 {
                        Color32::LIGHT_GREEN
                    } else {
                        colors[channel]
                    },
                ),
            ));
        }
        painter.text(
            Pos2::new(rect.left() + 4.0, rect.top() + 2.0),
            egui::Align2::LEFT_TOP,
            format!("{}", self.high / display_divisor),
            egui::FontId::monospace(11.0),
            Color32::GRAY,
        );
        painter.text(
            Pos2::new(rect.left() + 4.0, rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            format!("{}", self.low / display_divisor),
            egui::FontId::monospace(11.0),
            Color32::GRAY,
        );
        painter.text(
            Pos2::new(rect.right() - 4.0, rect.bottom() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("{max_distance:.0} px"),
            egui::FontId::monospace(11.0),
            Color32::GRAY,
        );
        if let Some(pos) = response.hover_pos() {
            let t = ((pos.x - inner.left()) / inner.width()).clamp(0.0, 1.0);
            let i = ((data.len() - 1) as f32 * t).round() as usize;
            if let Some((d, rgb)) = data.get(i) {
                painter.line_segment(
                    [
                        Pos2::new(pos.x, inner.top()),
                        Pos2::new(pos.x, inner.bottom()),
                    ],
                    egui::Stroke::new(1.0, Color32::GRAY),
                );
                let px = inner.left() + d / max_distance * inner.width();
                for channel in 0..channels {
                    let y = inner.bottom()
                        - ((rgb[channel] as f32 - lo) / range).clamp(0.0, 1.0) * inner.height();
                    painter.circle_filled(
                        Pos2::new(px, y),
                        3.0,
                        if channels == 1 {
                            Color32::LIGHT_GREEN
                        } else {
                            colors[channel]
                        },
                    );
                }
                let label = if channels == 1 {
                    format!("{d:.1} px  ·  value {}", rgb[0] / display_divisor)
                } else {
                    format!(
                        "{d:.1} px  ·  R {}  G {}  B {}",
                        rgb[0] / display_divisor,
                        rgb[1] / display_divisor,
                        rgb[2] / display_divisor
                    )
                };
                let galley =
                    painter.layout_no_wrap(label, egui::FontId::monospace(12.0), Color32::WHITE);
                let text_rect = egui::Align2::RIGHT_TOP.anchor_size(
                    Pos2::new(rect.right() - 6.0, rect.top() + 4.0),
                    galley.size(),
                );
                painter.rect_filled(
                    text_rect.expand(3.0),
                    2.0,
                    Color32::from_rgba_unmultiplied(0, 0, 0, 190),
                );
                painter.galley(text_rect.min, galley, Color32::WHITE);
            }
        }
    }

    fn draw_canvas(&mut self, ui: &mut egui::Ui) {
        let size = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        ui.painter_at(rect)
            .rect_filled(rect, 0.0, Color32::from_rgb(24, 27, 31));
        let Some(info) = self.info.clone() else {
            #[cfg(target_arch = "wasm32")]
            let empty_hint = if self.embedded {
                "Loading TIFF…"
            } else {
                "Open a TIFF file or enter a URL"
            };
            #[cfg(not(target_arch = "wasm32"))]
            let empty_hint = "Open a TIFF file or PNG directory";
            ui.painter_at(rect).text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                empty_hint,
                egui::FontId::proportional(22.0),
                Color32::GRAY,
            );
            return;
        };
        if self.fit_pending {
            self.fit(rect, &info);
        }
        let ctx = ui.ctx().clone();
        let mm_scale = (MINIMAP_MAX.x / info.width as f32).min(MINIMAP_MAX.y / info.height as f32);
        let mm_size = Vec2::new(info.width as f32 * mm_scale, info.height as f32 * mm_scale);
        let mm_rect =
            Rect::from_min_size(rect.right_bottom() - mm_size - Vec2::splat(10.0), mm_size);
        let pointer = ctx.input(|i| i.pointer.interact_pos());
        let over_minimap = pointer.is_some_and(|p| mm_rect.expand(2.0).contains(p));
        let keyboard = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::Num1),
            )
        });
        let delta = Vec2::new(rect.width() * 0.18, rect.height() * 0.18);
        if keyboard.0 {
            self.origin.x += delta.x;
        }
        if keyboard.1 {
            self.origin.x -= delta.x;
        }
        if keyboard.2 {
            self.origin.y += delta.y;
        }
        if keyboard.3 {
            self.origin.y -= delta.y;
        }
        if keyboard.4 {
            self.fit(rect, &info);
        }
        if keyboard.5 {
            self.zoom = 1.0;
            self.origin = Vec2::ZERO;
        }
        let primary_pressed = ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary));
        let primary_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Primary));
        let select_gesture = self.select_mode || ctx.input(|i| i.modifiers.alt);
        if primary_pressed && response.hovered() && !over_minimap && select_gesture {
            if let Some(pos) = pointer {
                let point = self.image_point(rect, pos);
                self.selection_start = Some(point);
                self.selection_end = Some(point);
                self.selection = None;
            }
        }
        let selecting = self.selection_start.is_some();
        if let Some(start) = self.selection_start {
            if primary_down {
                if let Some(pos) = pointer {
                    self.selection_end = Some(self.image_point(rect, pos));
                }
            } else {
                self.selection = self
                    .selection_end
                    .and_then(|end| Selection::from_points(start, end, &info));
                self.selection_start = None;
                self.selection_end = None;
            }
        }
        if primary_pressed && over_minimap && !selecting {
            self.minimap_drag = true;
        }
        let minimap_active = self.minimap_drag;
        if !primary_down {
            self.minimap_drag = false;
        }
        if minimap_active {
            if let Some(pos) = pointer {
                let ix = ((pos.x - mm_rect.min.x) / mm_scale).clamp(0.0, info.width as f32);
                let iy = ((pos.y - mm_rect.min.y) / mm_scale).clamp(0.0, info.height as f32);
                self.origin = rect.size() / 2.0 - Vec2::new(ix * self.zoom, iy * self.zoom);
            }
        } else if !selecting && response.dragged_by(egui::PointerButton::Primary) {
            self.origin += response.drag_delta();
        }
        if response.hovered() && !over_minimap {
            let (ctrl, shift) =
                ctx.input(|i| (i.modifiers.ctrl || i.modifiers.command, i.modifiers.shift));
            if ctrl {
                // Ctrl + wheel pans (Shift or horizontal wheel: left/right, otherwise up/down).
                // Read raw wheel events, because egui turns Ctrl+wheel into a zoom gesture.
                let wheel = ctx.input(|i| {
                    let mut sum = Vec2::ZERO;
                    for event in &i.events {
                        if let egui::Event::MouseWheel { unit, delta, .. } = event {
                            let factor = match unit {
                                egui::MouseWheelUnit::Point => 1.0,
                                egui::MouseWheelUnit::Line => 40.0,
                                egui::MouseWheelUnit::Page => rect.height(),
                            };
                            sum += *delta * factor;
                        }
                    }
                    sum
                });
                let mut pan = wheel;
                if shift && pan.x == 0.0 {
                    pan = Vec2::new(pan.y, 0.0);
                }
                self.origin += pan;
            }
            let scroll = if ctrl {
                0.0
            } else {
                ctx.input(|i| i.smooth_scroll_delta.y)
            };
            if scroll.abs() > 0.0 {
                if let Some(cursor) = ctx.input(|i| i.pointer.hover_pos()) {
                    let before = self.image_point(rect, cursor);
                    self.zoom = (self.zoom * (scroll * 0.0015).exp()).clamp(0.005, 32.0);
                    self.origin =
                        cursor - rect.min - Vec2::new(before.0 * self.zoom, before.1 * self.zoom);
                }
            }
        }
        let right_pressed = ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary));
        let right_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
        let right_released =
            ctx.input(|i| i.pointer.button_released(egui::PointerButton::Secondary));
        if right_pressed && response.hovered() && !over_minimap {
            if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                self.line_start = Some(self.image_point(rect, pos));
            }
        }
        if let Some(start) = self.line_start {
            if right_down {
                if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
                    self.line = Some((start, self.image_point(rect, pos)));
                }
            }
            if right_released {
                self.line_start = None;
                if let Some(line) = self.line {
                    self.profile = None;
                    if self.overview_done == info.count() {
                        self.jobs.send(
                            Job::Profile(self.generation, info.clone(), line.0, line.1),
                            &ctx,
                        );
                    } else {
                        self.status = "Image is still loading".into();
                    }
                }
            }
        }
        let detail = self.request_tiles(rect, &ctx);
        let painter = ui.painter_at(rect);
        if !matches!(detail, Some((_, true))) {
            if let Some(texture) = &self.overview_texture {
                painter.image(
                    texture.id(),
                    self.image_rect(rect, 0, 0, info.width, info.height),
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        }
        if let Some((level, complete)) = detail {
            if !complete {
                if let Some(previous) = self.fallback_level.filter(|previous| *previous != level) {
                    self.paint_tiles(&painter, rect, previous);
                }
            }
            self.paint_tiles(&painter, rect, level);
        }
        let area = self.selection.or_else(|| {
            self.selection_start
                .zip(self.selection_end)
                .and_then(|(start, end)| Selection::from_points(start, end, &info))
        });
        if let Some(area) = area {
            let selection_rect =
                self.image_rect(rect, area.x0, area.y0, area.width(), area.height());
            painter.rect_filled(
                selection_rect,
                0.0,
                Color32::from_rgba_unmultiplied(255, 220, 0, 35),
            );
            painter.rect_stroke(
                selection_rect,
                0.0,
                egui::Stroke::new(1.5, Color32::YELLOW),
                egui::StrokeKind::Inside,
            );
        }
        if let Some((a, b)) = self.line {
            let point = |p: (f32, f32)| {
                rect.min + self.origin + Vec2::new(p.0 * self.zoom, p.1 * self.zoom)
            };
            painter.line_segment(
                [point(a), point(b)],
                egui::Stroke::new(2.0, Color32::YELLOW),
            );
        }
        // Minimap (bottom-right): drag to move the main view.
        painter.rect_filled(
            mm_rect.expand(3.0),
            2.0,
            Color32::from_rgba_unmultiplied(10, 12, 15, 220),
        );
        if let Some(texture) = &self.overview_texture {
            painter.image(
                texture.id(),
                mm_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        let (vax, vay) = self.image_point(rect, rect.min);
        let (vbx, vby) = self.image_point(rect, rect.max);
        let viewport = Rect::from_min_max(
            mm_rect.min + Vec2::new(vax * mm_scale, vay * mm_scale),
            mm_rect.min + Vec2::new(vbx * mm_scale, vby * mm_scale),
        )
        .intersect(mm_rect);
        if viewport.is_positive() {
            painter.rect_filled(
                viewport,
                0.0,
                Color32::from_rgba_unmultiplied(255, 220, 0, 40),
            );
            painter.rect_stroke(
                viewport,
                0.0,
                egui::Stroke::new(1.5, Color32::YELLOW),
                egui::StrokeKind::Inside,
            );
        }
        painter.rect_stroke(
            mm_rect.expand(3.0),
            2.0,
            egui::Stroke::new(1.0, Color32::GRAY),
            egui::StrokeKind::Inside,
        );
        if let Some(pos) = response
            .hover_pos()
            .filter(|p| !mm_rect.expand(3.0).contains(*p))
        {
            let (x, y) = self.image_point(rect, pos);
            if x >= 0.0 && y >= 0.0 && x < info.width as f32 && y < info.height as f32 {
                painter.text(
                    rect.right_top() + Vec2::new(-8.0, 8.0),
                    egui::Align2::RIGHT_TOP,
                    format!("x {}  y {}", x as u32, y as u32),
                    egui::FontId::monospace(12.0),
                    Color32::WHITE,
                );
            }
        }
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive(&ctx);
        #[cfg(target_arch = "wasm32")]
        let can_open = !self.embedded;
        #[cfg(not(target_arch = "wasm32"))]
        let can_open = true;
        if can_open && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.choose_file(&ctx);
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save_selection(&ctx);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.profile = None;
            self.line = None;
            self.line_start = None;
            self.selection = None;
            self.selection_start = None;
            self.selection_end = None;
        }
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if self.profile.is_some() {
            egui::Panel::bottom("profile")
                .resizable(true)
                .default_size(188.0)
                .show(ui, |ui| self.draw_profile(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| self.draw_canvas(ui));
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn start() {
    wasm_bindgen_futures::spawn_local(async {
        use wasm_bindgen::JsCast;
        let canvas = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .get_element_by_id("viewer_canvas")
            .unwrap()
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .unwrap();
        eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| Ok(Box::new(Viewer::new(cc)))),
            )
            .await
            .unwrap();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::tiff::encoder::{TiffEncoder, colortype};
    use std::io::Cursor;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn last_directory_survives_reload() {
        let root = std::env::temp_dir().join(format!(
            "stitchviewer-directory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = root.join("obrazy");
        std::fs::create_dir_all(&directory).unwrap();
        let setting = root.join("config/last-directory");
        assert!(read_last_directory(&setting).is_none());
        save_last_directory(&setting, &directory).unwrap();
        assert_eq!(read_last_directory(&setting), Some(directory));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preloaded_block_serves_detail_and_profile_without_file() {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
            encoder
                .write_image::<colortype::Gray16>(4, 2, &[0, 1024, 32768, 65535, 10, 20, 30, 40])
                .unwrap();
        }
        let source = Source::Bytes(Arc::from(bytes.into_inner()));
        let info = Arc::new(tiff::inspect(&source, "test".into()).unwrap());
        let cache: BlockCache = Arc::new(Mutex::new(HashMap::new()));
        let generation = AtomicU64::new(1);
        let message = run_job(
            Job::Overview(1, source, info.clone(), 0, 1),
            &cache,
            &generation,
        );
        assert!(matches!(message, Message::Overview(1, _)));
        assert_eq!(cache.lock().unwrap().len(), 1);
        let detail = run_job(
            Job::Detail(1, info.clone(), vec![(0, 0, 0)], 0, 65535),
            &cache,
            &generation,
        );
        match detail {
            Message::Detail(1, tiles) => {
                assert_eq!(tiles.len(), 1);
                assert_eq!((tiles[0].w, tiles[0].h), (4, 2));
                assert_eq!((tiles[0].pixel_w, tiles[0].pixel_h), (4, 2));
            }
            _ => panic!("detail was not loaded from RAM"),
        }
        let profile = run_job(
            Job::Profile(1, info.clone(), (0.0, 0.0), (3.0, 0.0)),
            &cache,
            &generation,
        );
        match profile {
            Message::Profile(1, Ok(values)) => assert_eq!(values[2].1, [32768; 3]),
            _ => panic!("profile was not loaded from RAM"),
        }
        let area = Selection::from_points((1.0, 0.0), (3.0, 2.0), &info).unwrap();
        let mut output = Vec::new();
        write_selection_png(&mut output, &info, &cache, area).unwrap();
        let decoder = png::Decoder::new(Cursor::new(output));
        let mut reader = decoder.read_info().unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frame.bit_depth, png::BitDepth::Sixteen);
        assert_eq!(frame.color_type, png::ColorType::Grayscale);
        assert_eq!(
            &decoded[..frame.buffer_size()],
            &[4, 0, 128, 0, 0, 20, 0, 30]
        );
    }

    #[test]
    fn zoom_tile_reads_across_chunk_boundaries() {
        let info = Info {
            name: "test".into(),
            width: 4,
            height: 2,
            bits: 8,
            channels: 1,
            white_is_zero: false,
            chunk_width: 2,
            chunk_height: 2,
            chunks_x: 2,
            chunks_y: 1,
        };
        let cache: BlockCache = Arc::new(Mutex::new(HashMap::from([
            (
                0,
                Arc::new(Chunk {
                    rect: info.rect(0),
                    pixels: tiff::Pixels::U8(vec![1, 2, 5, 6]),
                }),
            ),
            (
                1,
                Arc::new(Chunk {
                    rect: info.rect(1),
                    pixels: tiff::Pixels::U8(vec![3, 4, 7, 8]),
                }),
            ),
        ])));
        let full = make_tile(&info, &cache, (0, 0, 0), 0, 65535).unwrap();
        assert_eq!((full.pixel_w, full.pixel_h), (4, 2));
        assert_eq!(
            full.rgba.chunks_exact(4).map(|p| p[0]).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
        let reduced = make_tile(&info, &cache, (1, 0, 0), 0, 65535).unwrap();
        assert_eq!((reduced.pixel_w, reduced.pixel_h), (2, 1));
        assert_eq!(
            reduced
                .rgba
                .chunks_exact(4)
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            vec![6, 8]
        );
        assert_eq!(tile_level(0.02), 5);
        let area = Selection::from_points((1.0, 0.0), (4.0, 2.0), &info).unwrap();
        let selected = selection_blocks(&cache, &info, area).unwrap();
        let mut output = Vec::new();
        write_selection_png(&mut output, &info, &selected, area).unwrap();
        let decoder = png::Decoder::new(Cursor::new(output));
        let mut reader = decoder.read_info().unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((frame.width, frame.height), (3, 2));
        assert_eq!(frame.bit_depth, png::BitDepth::Eight);
        assert_eq!(&decoded[..frame.buffer_size()], &[2, 3, 4, 6, 7, 8]);
    }

    #[test]
    #[ignore = "uses about 1.3 GiB of RAM"]
    fn overview_preloads_provided_tiff() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stitched_bf.tiff");
        if !path.exists() {
            return;
        }
        let source = Source::Path(path);
        let info = Arc::new(tiff::inspect(&source, "stitched_bf.tiff".into()).unwrap());
        let cache: BlockCache = Arc::new(Mutex::new(HashMap::new()));
        let generation = AtomicU64::new(1);
        for index in 0..info.count() {
            let message = run_job(
                Job::Overview(1, source.clone(), info.clone(), index, 14),
                &cache,
                &generation,
            );
            assert!(matches!(message, Message::Overview(1, _)));
        }
        let blocks = cache.lock().unwrap();
        assert_eq!(blocks.len(), info.count() as usize);
        let bytes: usize = blocks.values().map(|block| block.pixels.bytes_len()).sum();
        assert_eq!(
            bytes,
            info.width as usize * info.height as usize * info.channels
        );
        drop(blocks);
        let detail = run_job(
            Job::Detail(
                1,
                info.clone(),
                vec![(0, (info.width - 1) / TILE, (info.height - 1) / TILE)],
                0,
                65535,
            ),
            &cache,
            &generation,
        );
        assert!(matches!(detail, Message::Detail(1, _)));
    }
}
