use std::process::ExitCode;

fn main() -> ExitCode {
    let args = std::env::args_os().skip(1).collect();
    let deps = hub::cli::real_deps();
    ExitCode::from(hub::cli::run(args, &deps) as u8)
}
