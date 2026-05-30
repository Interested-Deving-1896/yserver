use std::{env, process::ExitCode};

const DEFAULT_DISPLAY: u16 = 7;

fn parse_display<I: IntoIterator<Item = String>>(args: I) -> Result<u16, String> {
    let mut out = DEFAULT_DISPLAY;
    for arg in args {
        match arg.parse::<u16>() {
            Ok(n) => out = n,
            Err(_) => return Err(format!("unrecognized argument: {arg}")),
        }
    }
    Ok(out)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let display = match parse_display(env::args().skip(1)) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("yserver: {err}");
            eprintln!("usage: yserver [<display>]");
            return ExitCode::FAILURE;
        }
    };

    match yserver::run(display) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("yserver: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<u16, String> {
        parse_display(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn no_args_is_default() {
        assert_eq!(parse(&[]).unwrap(), DEFAULT_DISPLAY);
    }

    #[test]
    fn positional_display() {
        assert_eq!(parse(&["0"]).unwrap(), 0);
        assert_eq!(parse(&["42"]).unwrap(), 42);
    }

    #[test]
    fn non_numeric_errors() {
        assert!(parse(&["foo"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
