use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use crate::config::Machine;
use crate::coordinator::reach::SSH_FAILED;
use crate::coordinator::ssh_args;

fn machine(ssh: &[&str]) -> Machine {
    Machine { name: "box".into(), ssh: ssh.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

// Intent (README.md, "When a machine is down"): ssh to a machine never asks a
// person for anything, notices a dropped connection within a minute, and gives
// up after ten seconds without an answer (ConnectTimeout=10) unless the
// machine's own ssh options set ConnectTimeout. This test owns ssh's option
// words; tests that see a command line read them from ssh_args.
#[test]
fn ssh_args_are_batch_kept_alive_and_timed() {
    let head = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4"];
    let cases: [(&str, &[&str], &[&str]); 2] = [
        ("the default timeout", &["ci@box"], &["-o", "ConnectTimeout=10", "ci@box"]),
        ("the machine's own timeout", &["-o", "connecttimeout=3", "ci@box"], &["-o", "connecttimeout=3", "ci@box"]),
    ];
    for (why, ssh, tail) in cases {
        let want: Vec<&str> = head.iter().chain(tail).copied().collect();
        assert_eq!(ssh_args(&machine(ssh)), want, "with {why}");
    }
}

// Intent (README, "When a machine is down"). A machine that takes SSH's
// connection but never answers held its place runner for as long as ssh waited,
// which is for good; one that drops the connection silently held it for the
// system's own connect timeout. ssh to a machine now gives up after ten
// seconds, and says so by exiting 255, which rests the machine (reach.rs). A
// machine's own ssh options may set another timeout.
#[cfg(unix)]
#[test]
fn ssh_gives_up_on_a_machine_that_never_answers() {
    let _spawns = super::spawns();
    if Command::new("ssh").arg("-V").output().is_err() {
        eprintln!("needs ssh");
        return;
    }
    // The machine takes each connection, says it took it, and never says a
    // word to ssh.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port().to_string();
    let (accepted_tx, accepted) = mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for c in l.incoming() {
            held.push(c);
            let _ = accepted_tx.send(());
        }
    });
    // The default ssh connects first, then the one with the machine's own
    // shorter timeout. That one ends first, while the default one still
    // waits; then the default one ends. Both end before the 30s allowance.
    let (order_tx, order) = mpsc::channel();
    let cases: [(&str, Vec<&str>); 2] = [
        ("by default", vec!["-F", "/dev/null", "-p", &port, "far@127.0.0.1"]),
        ("with its own timeout", vec!["-o", "ConnectTimeout=1", "-F", "/dev/null", "-p", &port, "far@127.0.0.1"]),
    ];
    for (name, ssh) in cases {
        let mut argv = ssh_args(&machine(&ssh));
        argv.push("true".into());
        let tx = order_tx.clone();
        std::thread::spawn(move || {
            let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(std::process::Stdio::null()).spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let code = loop {
                if let Some(s) = child.try_wait().unwrap() {
                    break s.code().unwrap_or(-1);
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    break -1;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let _ = tx.send((name, code));
        });
        if let Err(_) = accepted.recv_timeout(Duration::from_secs(30)) {
            panic!("ssh {name} never connected");
        }
        if let Ok((got, code)) = order.try_recv() {
            panic!("ssh {got} exited {code} before it connected");
        }
    }
    for want in ["with its own timeout", "by default"] {
        let (got, code) = order.recv_timeout(Duration::from_secs(40)).unwrap();
        assert!(got == want && code == SSH_FAILED, "ssh {got} exited {code}, want ssh {want} to exit {SSH_FAILED} next");
    }
}
