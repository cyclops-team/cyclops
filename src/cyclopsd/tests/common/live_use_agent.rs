use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("watch") {
        assert_eq!(args.get(2).map(String::as_str), Some("--from"));
        assert_eq!(args.get(3).map(String::as_str), Some("gemini"));
        watch_tool(&args[4]);
        return;
    }

    let fifo = args.get(1).expect("command FIFO");
    print!("❯ ");
    std::io::stdout().flush().expect("paint clean composer");
    let mut children: Vec<Child> = Vec::new();
    for line in BufReader::new(File::open(fifo).expect("open command FIFO")).lines() {
        let line = line.expect("read fixture command");
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.first().copied() {
            Some("watch") => {
                assert_eq!(fields.len(), 3, "watch command fields");
                children.push(
                    Command::new(fields[1])
                        .args(["watch", "--from", "gemini", fields[2]])
                        .spawn()
                        .expect("spawn cyclops watch fixture"),
                );
            }
            Some("request") => {
                assert_eq!(fields.len(), 6, "request command fields");
                let status = Command::new("python3")
                    .args([fields[1], fields[2], fields[3], fields[4], fields[5]])
                    .status()
                    .expect("spawn socket client");
                assert!(status.success(), "socket client failed");
            }
            Some("exit") => break,
            Some(command) => panic!("unknown fixture command {command:?}"),
            None => {}
        }
    }

    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn watch_tool(ready: &str) {
    print!("\x1b]2;CYCLOPS-WATCH-ACTIVE\x07");
    std::io::stdout().flush().expect("paint working title");
    fs::write(ready, std::process::id().to_string()).expect("write watch ready file");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
