#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    if std::env::args_os()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
    {
        println!(
            "StitchViewer\n\nUsage: stitchviewer [file.tif|png_directory]\n  -h, --help Show this help"
        );
        return Ok(());
    }
    let icon = image::load_from_memory(include_bytes!("../assets/stitchviewer.png"))
        .expect("embedded StitchViewer icon is a valid PNG")
        .into_rgba8();
    let (width, height) = icon.dimensions();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_icon(eframe::egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "StitchViewer",
        options,
        Box::new(|cc| Ok(Box::new(stitchviewer::Viewer::new(cc)))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    stitchviewer::start();
}
