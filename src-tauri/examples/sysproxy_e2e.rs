//! Manual e2e helper: flips the real system HTTPS proxy on/off via the same
//! code path the Tauri commands use.
//!   sysproxy_e2e                 -> print current state
//!   sysproxy_e2e on  "Wi-Fi"     -> proxy only the named services
//!   sysproxy_e2e off "Wi-Fi"     -> restore only the named services
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, services) = match args.split_first() {
        Some((m, rest)) => (m.as_str(), rest.to_vec()),
        None => {
            println!("{:#?}", localproxy_lib::sysproxy_get());
            return;
        }
    };
    let r = match mode {
        "on" => localproxy_lib::sysproxy_set("127.0.0.1", 8899, &services),
        "off" => localproxy_lib::sysproxy_clear(&services),
        "list" => localproxy_lib::sysproxy_services(),
        other => Err(format!("unknown mode {other}")),
    };
    println!("{r:?}");
}
