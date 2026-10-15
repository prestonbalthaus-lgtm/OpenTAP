//! `otapctl`: the OpenTAP command-line client.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let code = otap_ctl::run(&args, &mut stdout.lock(), &mut stderr.lock());
    std::process::exit(code);
}
