mod agent;
mod cli;
mod cmd;
mod config;
mod coordinator;
mod ctx;
mod go;
mod http;
mod lock;
mod plan;
mod proc;
mod quote;
mod random;
mod release;
mod scaleset;
mod signals;
mod testprog;
#[cfg(test)]
mod testserver;
mod version;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cmd::main(&args));
}
