# StitchViewer

A Rust viewer for large TIFF images and stitched PNG grids. The `eframe/egui` interface runs on desktop and in a browser, with `wgpu` rendering textures on the GPU. The `tiff` decoder reads independently compressed strips or tiles.

## Desktop

```sh
cargo run --release -- stitched_bf.tiff
cargo run --release -- experiment/bf
cargo run --release -- experiment
```

Without an argument, the app opens `stitched_bf.tiff` from the current directory if it exists. Use **Open TIFF** to select another file or **Open PNG directory** to select a directory of `Scan_X_Y_*.png` files. You can also select `experiment` when only one of its `bf` and `df` subdirectories contains matching files. If both contain files, select the desired subdirectory. On desktop, file and folder dialogs reopen in the last successfully opened image directory, including after restarting the app.

PNG tiles are assembled in memory at their original resolution, without writing a TIFF or reducing image size. The viewer preserves 8-bit or 16-bit grayscale values. Grid positions and rotations follow `ScanStitcher-cpp`; rotation settings are read from `raport/stitch_summary.json` when present. The sample `experiment/bf` grid measures 61600 × 55000 pixels.

Every decoded chunk stays in RAM after creating the overview. This avoids decompressing chunks again when panning. View textures are generated on demand at the resolution needed for the current zoom. The sample TIFF needs about 1.27 GB of pixel memory; the full-resolution BF PNG grid needs about 3.39 GB at 8-bit grayscale. GPU textures have a separate limit of 96 tiles.

## Arch Linux / AUR

Build and install a package from local sources with `./scripts/install-arch.sh`. The [AUR recipe](aur/README.md) also includes `stitchviewer-git`, which builds from the [GitHub repository](https://github.com/RafalVonau/StitchViewer).
The package installs an application menu shortcut, a scalable icon, and TIFF file association.

## Windows

On Linux with MinGW-w64 installed, run `bash scripts/build-windows.sh`. The script installs the `x86_64-pc-windows-gnu` Rust target if needed and creates `target/x86_64-pc-windows-gnu/release/stitchviewer.exe`. On Debian or Ubuntu, install the compiler with `sudo apt install gcc-mingw-w64-x86-64`.

The workflows in `.github/workflows/windows.yml` and `.github/workflows/linux.yml` run only when a new tag is pushed. They build native Windows and Linux executables, upload GitHub Actions artifacts, and create or update the corresponding GitHub Release with `stitchviewer.exe` and `stitchviewer-linux-x86_64.tar.gz`.

## WebAssembly

Install `wasm-bindgen-cli` at the version used by `Cargo.lock` and install Caddy. Then run the build script. The script installs the `wasm32-unknown-unknown` Rust target if needed.

```sh
cargo install wasm-bindgen-cli --version 0.2.129 --locked
./scripts/build-web.sh
./scripts/build-web.sh --serve
```

The script writes a static site to `dist/`. The `--serve` option starts Caddy using `Caddyfile`; open `http://localhost:8080`. You can also run `caddy run --config Caddyfile` after building. In the browser, use **Open TIFF** to select a file. Opening PNG directories is currently available only in the desktop app. The browser holds both the selected TIFF bytes and decoded pixels in memory, so available browser memory limits file size. Web decoding runs one chunk at a time on the main WASM thread. The desktop app uses one decoder thread per available CPU core.

### TIFF from an API

In the web app, enter an address in **TIFF URL** or pass it with the `tiff` query parameter:

```text
http://localhost:8080/?tiff=%2Fapi%2Fscans%2F42%2Ftiff
```

When the page opens, the browser requests `GET /api/scans/42/tiff`. The Axum endpoint should return the TIFF bytes with status `200` and `Content-Type: image/tiff`. The included `Caddyfile` proxies `/api/*` to Axum at `127.0.0.1:3000`; change that address if your server uses another port. You can serve `dist/` directly from Axum instead. An API on another origin needs CORS configured. HTTP and download errors appear in the status bar. The entire TIFF is downloaded before decoding starts, so very large files need enough browser memory.

When `?tiff=...` is present, the viewer hides the file picker and URL input. The linked image opens directly; region selection and PNG download remain available.

## Controls

- Left drag: pan the image.
- Mouse wheel: zoom around the cursor.
- Right drag: select a measurement line and show its intensity profile. Hover over the chart to see raw 8-bit or 16-bit values.
- Alt + left drag: select a rectangular region. You can also enable **Select area** and drag without Alt. Choose **Save PNG** or press `Ctrl+S` to export the selected original pixels. The export keeps the source bit depth and does not apply display contrast. `Esc` clears the selection.
- Arrow keys: pan. `F`: fit to window. `1`: 1:1 scale. `Ctrl+O`: open a file.

TIFF supports interleaved grayscale, RGB, and RGBA images with 8 or 16 bits per channel. Contrast for 16-bit grayscale is estimated automatically from the overview. The overview is at most 2048 pixels on its longer side. The viewer generates 512 × 512 textures for visible regions at a resolution matched to the zoom. The texture cache holds up to 96 tiles.

## License

MIT. See [LICENSE](LICENSE).
