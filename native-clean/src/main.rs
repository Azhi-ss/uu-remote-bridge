mod api;
mod auth;
mod config;
mod control;
mod desktop;
mod file_client;
mod file_host;
mod host;
mod media;
mod peer;
mod signal;
mod terminal;
mod terminal_host;
#[cfg(feature = "desktop")]
mod viewer_terminal;
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
            let result = client.sessions().await;
            for session in client.finish(result).await? {
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
            let result = client.exec(selector, &rest.join(" ")).await;
            let output = client.finish(result).await?;
            print!("{output}");
            Ok(())
        }
        [command, device, path] if command == "files-list" => {
            let mut client = peer::Client::connect_files(device).await?;
            let result = client.files_list(path).await;
            for entry in client.finish(result).await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    entry.kind, entry.size, entry.modified, entry.name, entry.path
                );
            }
            Ok(())
        }
        [command, device, local, remote] if command == "files-upload" => {
            let mut client = peer::Client::connect_files(device).await?;
            let result = client.files_upload(local, remote).await;
            client.finish(result).await
        }
        [command, device, remote, local] if command == "files-download" => {
            let mut client = peer::Client::connect_files(device).await?;
            let result = client.files_download(remote, local).await;
            client.finish(result).await
        }
        [command, device, path] if command == "files-remove" => {
            let mut client = peer::Client::connect_files(device).await?;
            let result = client.files_remove(path).await;
            client.finish(result).await
        }
        [command, device, raw] if command == "input" => {
            let value: serde_json::Value =
                serde_json::from_str(raw).map_err(|_| "input expects a JSON object")?;
            if !value.is_object() && !value.is_array() {
                return Err("input expects an object or array of objects".into());
            }
            if !cfg!(feature = "desktop") {
                return Err("input requires rebuilding with --features desktop".into());
            }
            let client = peer::Client::connect_with_options(device, true).await?;
            let events = value
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(std::slice::from_ref(&value));
            for event in events {
                client.send_input(event).await?;
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            client.finish(Ok(())).await
        }
        [command, device, path] if command == "desktop-snapshot" => {
            desktop::snapshot(device, path).await
        }
        [command, device] if command == "desktop-view" => desktop::view_cli(device, 100).await,
        [command, device, columns] if command == "desktop-view" => {
            let columns = columns
                .parse::<usize>()
                .map_err(|_| "desktop-view columns must be a number")?;
            desktop::view_cli(device, columns).await
        }
        _ => Err(help().into()),
    }
}

fn help() -> &'static str {
    "uu-remote-clean\n  sessions DEVICE\n  exec DEVICE SESSION_OR_NAME COMMAND...\n  files-list DEVICE PATH\n  files-upload DEVICE LOCAL REMOTE\n  files-download DEVICE REMOTE LOCAL\n  files-remove DEVICE PATH\n  input DEVICE JSON_OR_ARRAY\n  desktop-view DEVICE [COLUMNS]\n  desktop-snapshot DEVICE FILE.ppm\n  host [--desktop]\n\ndesktop-view forwards keyboard/mouse; Ctrl+] exits, Ctrl+G then a key sends Win+key.\nThe default build is a headless terminal/WebRTC clean-room client and host. Desktop capture/input/view requires --features desktop and host --desktop."
}
