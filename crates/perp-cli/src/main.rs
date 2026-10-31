#![forbid(unsafe_code)]

fn main() {
    let args = match perp_cli::args_from_os(std::env::args_os().skip(1)) {
        Ok(a) => a,
        Err(n) => {
            eprintln!("error: argument {n} is not valid UTF-8");
            std::process::exit(perp_cli::EXIT_USAGE);
        }
    };
    let code = perp_cli::run(args, &mut std::io::stdout(), &mut std::io::stderr());
    std::process::exit(code);
}
