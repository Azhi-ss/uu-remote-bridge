//! Shell-specific terminal framing helpers.
//!
//! This module deliberately contains no desktop permissions or process injection.
//! It only turns a command into bytes for an already-authorized remote terminal.

pub fn command_for_shell(shell: &str, command: &str, marker: &str) -> Vec<u8> {
    if shell.to_ascii_lowercase().contains("powershell") {
        let codes = marker
            .bytes()
            .map(|byte| format!("0x{byte:02x}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("{command}; $m=(-join ({codes} | %% {{ [char]$_ }})); Write-Output $m\r\n")
            .into_bytes()
    } else {
        let escaped = marker
            .bytes()
            .map(|byte| format!("\\{byte:03o}"))
            .collect::<String>();
        format!("{command}; printf '{escaped}\\n'\n").into_bytes()
    }
}

pub fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_shell_markers() {
        let bash = String::from_utf8(command_for_shell("bash", "whoami", "MARK")).unwrap();
        assert!(bash.contains("printf '\\115\\101\\122\\113\\n'"));
        let powershell =
            String::from_utf8(command_for_shell("powershell", "whoami", "MARK")).unwrap();
        assert!(powershell.contains("0x4d,0x41,0x52,0x4b"));
    }
}
