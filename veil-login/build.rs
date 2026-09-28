use std::path::PathBuf;

fn main() {
    let mut ui_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    ui_path.push("ui/velogin.slint");
    
    slint_build::compile(ui_path.to_str().unwrap()).unwrap();
}

