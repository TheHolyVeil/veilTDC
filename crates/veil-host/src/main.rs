//! Standalone runner for veil-host.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::io::Result<()> {
    veil_host::cli::run_cli()
}
