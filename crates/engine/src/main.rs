//! `loudautod`, LOUDAUTO's engine. Logs to stderr (the journal, under systemd) at `info`
//! unless `RUST_LOG` says otherwise; see `engine::run` for what it does.

use std::{process::ExitCode, time::Duration};

/// How long the exit waits for blocking work still out (a key lookup that
/// waits on a prompt, a notification) before leaving it behind.
const BLOCKING_GRACE: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("loudautod: no async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(engine::run::run(engine_ipc::socket_path()));
    runtime.shutdown_timeout(BLOCKING_GRACE);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("loudautod: {e}");
            ExitCode::FAILURE
        }
    }
}
