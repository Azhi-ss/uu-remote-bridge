mod api;
mod auth;
mod config;
mod control;
mod file_host;
mod host;
mod peer;
mod signal;
mod terminal;
mod terminal_host;
mod wire;

use std::env;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = run(&args).await;
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(2);
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    match args {
        [] => {
            println!("{}", help());
            Ok(())
        }
        [command] if command == "help" || command == "--help" => {
            println!("{}", help());
            Ok(())
        }
        [command, device] if command == "sessions" => {
            let mut client = peer::Client::connect(device).await?;
            for session in client.sessions().await? {
                println!(
                    "{}\t{}\t{}\t{}",
                    session.id, session.name, session.shell, session.state
                );
            }
            Ok(())
        }
        [command] if command == "host" => {
            let credentials = config::load()?;
            host::Host::new(credentials).run().await
        }
        [command, device, selector, rest @ ..] if command == "exec" => {
            if rest.is_empty() {
                return Err("usage: exec DEVICE SESSION_OR_NAME COMMAND...".into());
            }
            let mut client = peer::Client::connect(device).await?;
            let output = client.exec(selector, &rest.join(" ")).await?;
            print!("{output}");
            Ok(())
        }
        _ => Err(help().into()),
    }
}

fn help() -> &'static str {
    "uu-remote-clean\n  sessions DEVICE\n  exec DEVICE SESSION_OR_NAME COMMAND...\n  host\n\nThe default build is a headless terminal/WebRTC clean-room client and host."
}
