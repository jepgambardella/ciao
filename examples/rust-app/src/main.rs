use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let listener = TcpListener::bind(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    for mut stream in listener.incoming().flatten() {
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request);
        let body = "ciao rust example\n";
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
    }
}
