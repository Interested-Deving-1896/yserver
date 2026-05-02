use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match yserver::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("yserver: {err}");
            ExitCode::FAILURE
        }
    }
}
