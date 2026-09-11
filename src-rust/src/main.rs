//! Entry point: argument parsing and mode dispatch, and nothing else.
//!
//! The modes are in `cli/`; everything they call is a service. Keeping this
//! file thin is what makes "one binary, three modes" a statement about the
//! command line rather than about the architecture.

fn main() -> std::process::ExitCode {
    eprintln!("lightview: not yet built");
    std::process::ExitCode::FAILURE
}
