//! Process fixture for socket peer identity tests.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

extern "C" {
    fn fork() -> i32;
}

fn main() {
    let mut args = std::env::args().skip(1);
    let socket = args.next().expect("socket path");
    let mode = args.next().expect("mode");
    let mut stream = UnixStream::connect(socket).expect("connect fixture socket");
    stream.write_all(b"R").expect("announce connection");

    let mut command = [0u8; 1];
    stream.read_exact(&mut command).expect("read command");
    match mode.as_str() {
        "hold" => assert_eq!(command, *b"X"),
        "inherit" => {
            assert_eq!(command, *b"F");
            let forked = unsafe { fork() };
            assert!(forked >= 0, "fork failed");
            if forked > 0 {
                return;
            }
            stream.write_all(b"I").expect("announce inherited socket");
            stream.read_exact(&mut command).expect("read child exit");
            assert_eq!(command, *b"X");
        }
        other => panic!("unknown mode {other}"),
    }
}
