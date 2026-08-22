use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn main() {
    let fifo = env::args().nth(1).expect("agent FIFO");
    let input = File::open(fifo).expect("open agent FIFO");
    for line in BufReader::new(input).lines() {
        let line = line.expect("read agent command");
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.first().copied() {
            Some("child") => direct_child(&fields),
            Some("sibling") => sibling_children(&fields),
            Some("hold-connection") => hold_connection(&fields),
            Some("exit") => return,
            Some(command) => panic!("unknown agent command {command:?}"),
            None => {}
        }
    }
}

fn direct_child(fields: &[&str]) {
    assert_eq!(fields.len(), 8, "child command fields");
    let mut hook = spawn_client(
        fields[1], fields[2], fields[3], fields[5], fields[6], fields[7],
    );
    wait_for(Path::new(fields[5]));
    fs::write(
        fields[4],
        format!("agent_pid={}\nhook_pid={}\n", std::process::id(), hook.id()),
    )
    .expect("write child topology");
    wait_success(&mut hook, "direct hook");
}

fn sibling_children(fields: &[&str]) {
    assert_eq!(fields.len(), 9, "sibling command fields");
    let mut tool = foreground_tool(fields[5]);
    wait_for(Path::new(fields[5]));
    let mut hook = spawn_client(
        fields[1], fields[2], fields[4], fields[6], fields[7], fields[8],
    );
    wait_for(Path::new(fields[6]));
    fs::write(
        fields[3],
        format!(
            "agent_pid={}\nforeground_tool_pid={}\nhook_pid={}\n",
            std::process::id(),
            tool.id(),
            hook.id()
        ),
    )
    .expect("write sibling topology");

    wait_success(&mut hook, "sibling hook");
    let _ = tool.kill();
    let _ = tool.wait();
}

fn hold_connection(fields: &[&str]) {
    assert_eq!(fields.len(), 7, "connection command fields");
    // Child handles do not kill on drop. This client outlives the supervisor.
    drop(spawn_client(
        fields[1], fields[2], fields[5], fields[3], fields[4], fields[6],
    ));
}

fn spawn_client(
    client: &str,
    socket: &str,
    result: &str,
    ready: &str,
    send: &str,
    params: &str,
) -> Child {
    Command::new("python3")
        .args([client, socket, result, ready, send, params])
        .spawn()
        .expect("spawn hook client")
}

fn foreground_tool(ready: &str) -> Child {
    const SCRIPT: &str = r#"
import os, signal, sys
os.setpgrp()
try:
    tty = os.open('/dev/tty', os.O_RDWR)
    os.tcsetpgrp(tty, os.getpgrp())
    os.close(tty)
except OSError:
    pass
with open(sys.argv[1], 'w') as output:
    output.write(str(os.getpid()))
signal.pause()
"#;
    Command::new("python3")
        .args(["-c", SCRIPT, ready])
        .spawn()
        .expect("spawn foreground tool")
}

fn wait_success(child: &mut Child, name: &str) {
    assert!(child.wait().expect("wait for child").success(), "{name} failed");
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {}", path.display());
}
