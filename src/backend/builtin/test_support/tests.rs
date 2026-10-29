use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use super::{Reply, serve_until};

#[test]
fn no_request_and_missing_script_turn_join_without_waiting_for_watchdog() {
    for replies in [Vec::new(), vec![Reply::http("200 OK", String::new())]] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let expected_success = replies.is_empty();
        let started = Instant::now();
        let server = serve_until(listener, replies, Duration::from_millis(200), None);
        assert_eq!(server.join().is_ok(), expected_success);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn partial_headers_and_bodies_close_with_bounded_cleanup() {
    for bytes in [
        b"POST / HTTP/1.1\r\n".as_slice(),
        b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nx".as_slice(),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let address = listener.local_addr().expect("address");
        let server = serve_until(
            listener,
            vec![Reply::http("200 OK", String::new())],
            Duration::from_millis(200),
            None,
        );
        let started = Instant::now();
        let mut stream = TcpStream::connect(address).expect("connect fixture");
        stream.write_all(bytes).expect("partial request");
        drop(stream);
        assert!(server.join().is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn partial_request_cannot_extend_absolute_watchdog() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let address = listener.local_addr().expect("address");
    let server = serve_until(
        listener,
        vec![Reply::http("200 OK", String::new())],
        Duration::from_millis(60),
        None,
    );
    let started = Instant::now();
    let mut stream = TcpStream::connect(address).expect("connect fixture");
    stream
        .write_all(b"POST / HTTP/1.1\r\n")
        .expect("partial header");
    assert!(server.join().is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
}
