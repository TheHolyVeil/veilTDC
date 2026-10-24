//! Entry point for veil-bar.

fn main() {
    tracing_subscriber::fmt::init();
    if let Err(e) = veil_bar::run_bar() {
        eprintln!("[veil-bar] Error: {e}");
        std::process::exit(1);
    }
}
