//! The proxy is deliberately dumb: it copies bytes in both directions and knows
//! nothing about HTTP, TLS or any other protocol.
//!
//! There is no buffering to tune and no timeout to argue about. Each direction
//! gets its own thread, because both halves of a connection can be blocked at the
//! same time (a websocket is the easy example) and a progress in one direction
//! must not depend on the other.

use std::io;
use std::net::{Shutdown, TcpStream};
use std::thread;

/// Connects a client and an upstream socket until both directions are finished,
/// then closes both.
pub fn splice(client: TcpStream, upstream: TcpStream) {
    thread::scope(|scope| {
        scope.spawn(|| copy_then_half_close(&upstream, &client));
        copy_then_half_close(&client, &upstream);
    });

    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
}

/// Copies `src` into `dst`. When `src` reaches EOF we half-close `dst` (a TCP
/// FIN) so the peer sees the end of the stream but can still answer.
///
/// A failing copy is not worth reporting: the usual reason is that the peer went
/// away, and the connection is closed either way.
fn copy_then_half_close(dst: &TcpStream, src: &TcpStream) {
    let _ = io::copy(&mut &*src, &mut &*dst);
    let _ = dst.shutdown(Shutdown::Write);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn proxies_bytes_in_both_directions() {
        // A tiny echo server stands in for the app.
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut buffer = [0u8; 16];
            let read = stream.read(&mut buffer).unwrap();
            stream.write_all(&buffer[..read]).unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy = thread::spawn(move || {
            let (client, _) = listener.accept().unwrap();
            let upstream = TcpStream::connect(upstream_addr).unwrap();
            splice(client, upstream);
        });

        let mut client = TcpStream::connect(proxy_addr).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client.write_all(b"ping").unwrap();

        let mut buffer = [0u8; 16];
        let read = client.read(&mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"ping");

        drop(client);
        proxy.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn a_half_close_is_forwarded() {
        // The client sends, then half-closes; the app must see EOF and answer.
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            stream.write_all(b"done").unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy = thread::spawn(move || {
            let (client, _) = listener.accept().unwrap();
            let upstream = TcpStream::connect(upstream_addr).unwrap();
            splice(client, upstream);
        });

        let mut client = TcpStream::connect(proxy_addr).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client.write_all(b"bye").unwrap();
        client.shutdown(Shutdown::Write).unwrap();

        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, b"done");

        proxy.join().unwrap();
        server.join().unwrap();
    }
}
