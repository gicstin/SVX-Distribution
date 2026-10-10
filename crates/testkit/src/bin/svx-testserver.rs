use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = PathBuf::from(".");
    let mut port = 0u16;
    let mut port_file = None;
    let mut cuts = Vec::new();
    let mut i = 0;
    while i + 1 < args.len() {
        match args[i].as_str() {
            "--root" => root = PathBuf::from(&args[i + 1]),
            "--port" => port = args[i + 1].parse().expect("port"),
            "--port-file" => port_file = Some(PathBuf::from(&args[i + 1])),
            "--cut" => cuts.push(args[i + 1].clone()),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    let server = testkit::Server::start(&root, port);
    for c in cuts {
        let (p, n) = c.split_once('=').expect("--cut PATH=BYTES");
        server.cut_once(p, n.parse().expect("bytes"));
    }
    if let Some(f) = port_file {
        std::fs::write(f, server.port.to_string()).expect("write port file");
    }
    println!("serving {} on {}", root.display(), server.base());
    loop {
        std::thread::park();
    }
}
