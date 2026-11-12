//! A stand-in for `ssh -W 127.0.0.1:<port> <host>` in paseo-client's tests,
//! so they never touch a real SSH server. It connects to the port on this
//! machine and pipes standard input and output through it, as ssh's stdio
//! forwarding does. A host with "denied" in it fails as a refused key does;
//! one with "closed" fails as a forward to a closed port does. Each run adds
//! its arguments as a line to [`log`] for its host.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

/// Where the runs for `host` are written down, in the temporary directory.
fn log(host: &str) -> PathBuf {
    let name: String = (host.chars())
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    std::env::temp_dir().join(format!("fake-ssh-{name}.log"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = args.last().cloned().unwrap_or_default();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log(&host))
        .expect("the log");
    writeln!(file, "{}", args.join(" ")).expect("logged");
    drop(file);
    let forward = args
        .iter()
        .position(|arg| arg == "-W")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_default();
    if host.contains("denied") {
        eprintln!("Warning: Permanently added '{host}' (ED25519) to the list of known hosts.");
        eprintln!("{host}: Permission denied (publickey).");
        std::process::exit(255);
    }
    let Ok(stream) = TcpStream::connect(&forward)
        .map_err(|_| ())
        .and_then(|stream| {
            if host.contains("closed") {
                Err(())
            } else {
                Ok(stream)
            }
        })
    else {
        eprintln!("channel 0: open failed: connect failed: Connection refused");
        eprintln!("stdio forwarding failed");
        std::process::exit(255);
    };
    let mut upstream = stream.try_clone().expect("a second handle");
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if upstream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = upstream.shutdown(std::net::Shutdown::Write);
    });
    let mut downstream = stream;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match downstream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout
                    .write_all(&buf[..n])
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}
