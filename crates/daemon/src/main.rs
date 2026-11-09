// kvmd: background daemon.
//   kvmd listen [addr]   Windows client: receive input and inject it (default 0.0.0.0:24800)
//   kvmd test <addr>     send a small mouse-square test to a listener
// TODO: Linux server backend (XDG InputCapture portal + libei), TLS + pairing, edge logic.
use kvm_protocol::codec::{read_msg, write_msg};
use kvm_protocol::{InputEvent, Message};
use std::net::{TcpListener, TcpStream};

#[cfg(windows)]
mod inject_win;

#[cfg(windows)]
fn inject(ev: &InputEvent) {
    inject_win::inject(ev)
}
#[cfg(target_os = "linux")]
mod inject_linux;

#[cfg(target_os = "linux")]
fn inject(ev: &InputEvent) {
    inject_linux::inject(ev)
}
#[cfg(not(any(windows, target_os = "linux")))]
fn inject(_ev: &InputEvent) {
    eprintln!("injection not implemented on this OS yet");
}

fn serve(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("kvmd listening on {addr}");
    for stream in listener.incoming() {
        let mut s = stream?;
        s.set_nodelay(true)?;
        println!("connection from {:?}", s.peer_addr());
        loop {
            match read_msg(&mut s) {
                Ok(Message::Input(ev)) => inject(&ev),
                Ok(Message::Ping) => write_msg(&mut s, &Message::Pong)?,
                Ok(m) => println!("{m:?}"),
                Err(e) => {
                    println!("disconnected: {e}");
                    break;
                }
            }
        }
    }
    Ok(())
}

fn test(addr: &str) -> std::io::Result<()> {
    let mut s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    write_msg(&mut s, &Message::Hello { name: "test".into(), width: 0, height: 0, version: 1 })?;
    for (dx, dy) in [(8, 0), (0, 8), (-8, 0), (0, -8)] {
        for _ in 0..20 {
            write_msg(&mut s, &Message::Input(InputEvent::MouseMoveRel { dx, dy }))?;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("listen") => serve(args.get(2).map(String::as_str).unwrap_or("0.0.0.0:24800")),
        Some("test") => test(args.get(2).map(String::as_str).unwrap_or("127.0.0.1:24800")),
        _ => {
            eprintln!("usage: kvmd listen [addr] | kvmd test <addr>");
            Ok(())
        }
    }
}
