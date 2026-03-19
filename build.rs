#[cfg(windows)]
fn main() {
    use std::path::Path;

    let icon_path = Path::new("assets/icon.ico");
    println!("cargo:rerun-if-changed={}", icon_path.display());

    if icon_path.exists() {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon(icon_path.to_string_lossy().as_ref());
        resource
            .compile()
            .expect("failed to embed Windows icon from assets/icon.ico");
    } else {
        println!("cargo:warning=icon not found at assets/icon.ico; building without exe icon");
    }
}

#[cfg(not(windows))]
fn main() {}
