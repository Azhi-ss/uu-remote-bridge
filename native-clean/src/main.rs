mod api;
mod auth;
mod config;
mod control;
mod desktop;
mod file_client;
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
        [command, flag] if command == "host" && flag == "--desktop" => {
            let credentials = config::load()?;
            host::Host::new_with_options(credentials, true).run().await
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
        [command, device, path] if command == "files-list" => {
            let mut client = peer::Client::connect(device).await?;
            for entry in client.files_list(path).await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    entry.kind, entry.size, entry.modified, entry.name, entry.path
                );
            }
            Ok(())
        }
        [command, device, local, remote] if command == "files-upload" => {
            let mut client = peer::Client::connect(device).await?;
            client.files_upload(local, remote).await
        }
        [command, device, remote, local] if command == "files-download" => {
            let mut client = peer::Client::connect(device).await?;
            client.files_download(remote, local).await
        }
        [command, device, raw] if command == "input" => {
            let value: serde_json::Value =
                serde_json::from_str(raw).map_err(|_| "input expects a JSON object")?;
            let client = peer::Client::connect(device).await?;
            client.send_input(&value).await
        }
        _ => Err(help().into()),
    }
}

fn help() -> &'static str {
    "uu-remote-clean\n  sessions DEVICE\n  exec DEVICE SESSION_OR_NAME COMMAND...\n  files-list DEVICE PATH\n  files-upload DEVICE LOCAL REMOTE\n  files-download DEVICE REMOTE LOCAL\n  input DEVICE JSON\n  host [--desktop]\n\nThe default build is a headless terminal/WebRTC clean-room client and host. Desktop capture/input requires --features desktop and host --desktop."
}
