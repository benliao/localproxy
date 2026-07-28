#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--cli") {
        let mut rest = args.iter().skip_while(|a| *a != "--cli").skip(1);
        let bind = rest
            .next()
            .cloned()
            .unwrap_or_else(|| "127.0.0.1:8899".into());
        let key = rest.next().cloned();
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        if let Err(e) = rt.block_on(localproxy_lib::run_cli(&bind, key.as_deref())) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    localproxy_lib::run()
}
