//! Persistent agent process for the parity rig.
//!
//! Commands arrive through a FIFO, so the agent can keep its process identity
//! while a composer owns the terminal. Every Cyclops command runs below this
//! process and therefore exercises the same ancestry check as a real agent.

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::thread;

const COMPOSER_WRAPPER: &str = r#"
import os, signal, sys
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
os.setpgrp()
try:
    tty = os.open('/dev/tty', os.O_RDWR)
    os.tcsetpgrp(tty, os.getpgrp())
    os.close(tty)
except OSError:
    pass
os.execvp('python3', ['python3', sys.argv[1]])
"#;

const RECLAIM_TERMINAL: &str = r#"
import os, signal
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
try:
    tty = os.open('/dev/tty', os.O_RDWR)
    os.tcsetpgrp(tty, os.getpgrp())
    os.close(tty)
except OSError:
    pass
"#;

fn main() {
    let fifo = env::args().nth(1).expect("command FIFO");
    let ready = env::args().nth(2).expect("ready file");
    fs::write(&ready, std::process::id().to_string()).expect("write ready file");

    loop {
        let input = File::open(&fifo).expect("open command FIFO");
        for line in BufReader::new(input).lines() {
            let line = line.expect("read fixture command");
            let (command, argument) = line
                .split_once('\t')
                .map_or((line.as_str(), ""), |parts| parts);
            match command {
                "run" => run(argument),
                "composer" => start_composer(argument),
                "exit" => return,
                other => panic!("unknown parity agent command {other:?}"),
            }
        }
    }
}

fn run(script: &str) {
    let status = Command::new("/bin/sh")
        .args(["-c", script])
        .status()
        .expect("run parity command");
    assert!(status.success(), "parity command wrapper failed");
}

fn start_composer(path: &str) {
    let mut child = Command::new("python3")
        .args(["-c", COMPOSER_WRAPPER, path])
        .spawn()
        .expect("start fixture composer");
    thread::spawn(move || {
        let _ = child.wait();
        let _ = Command::new("python3")
            .args(["-c", RECLAIM_TERMINAL])
            .status();
    });
}
