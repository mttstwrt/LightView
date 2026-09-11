//! Entry point: start a runtime and hand over to the CLI.
//!
//! Everything the modes do lives in `cli/`; everything they call is a service.
//! Keeping this file thin is what makes "one binary, three modes" a statement
//! about the command line rather than about the architecture.

fn main() -> std::process::ExitCode {
    // `RUST_LOG` if set, otherwise warnings only: a gallery open logs nothing
    // on a healthy run, so anything that does appear is worth reading.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lightview: could not start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    runtime.block_on(lightview::cli::run())
}
