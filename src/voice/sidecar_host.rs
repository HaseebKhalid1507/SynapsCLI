use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use std::path::PathBuf;

use crate::{Result, RuntimeError};

use super::sidecar_protocol::{SidecarCommand, SidecarEvent};

#[derive(Debug)]
pub struct VoiceSidecarHost {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl VoiceSidecarHost {
    pub async fn spawn(command: &str, args: &[String]) -> Result<Self> {
        let command_path = resolve_sidecar_command(command);
        tracing::debug!(command = %command, resolved = %command_path.display(), args = ?args, "spawning voice sidecar");
        let mut child = Command::new(&command_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| {
                RuntimeError::Tool(format!("failed to spawn voice sidecar '{}': {err}", command_path.display()))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            RuntimeError::Tool(format!("voice sidecar '{command}' did not provide stdin"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            RuntimeError::Tool(format!("voice sidecar '{command}' did not provide stdout"))
        })?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    pub async fn send(&mut self, command: SidecarCommand) -> Result<()> {
        let mut json = serde_json::to_vec(&command)
            .map_err(|err| RuntimeError::Tool(format!("failed to encode voice sidecar command: {err}")))?;
        json.push(b'\n');
        self.stdin
            .write_all(&json)
            .await
            .map_err(|err| RuntimeError::Tool(format!("failed to write voice sidecar command: {err}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| RuntimeError::Tool(format!("failed to flush voice sidecar command: {err}")))
    }

    pub async fn recv(&mut self) -> Result<SidecarEvent> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|err| RuntimeError::Tool(format!("failed to read voice sidecar event: {err}")))?;
        if bytes == 0 {
            return Err(RuntimeError::Tool("voice sidecar closed stdout".to_string()));
        }
        serde_json::from_str(line.trim_end()).map_err(|err| {
            RuntimeError::Tool(format!("failed to decode voice sidecar event: {err}"))
        })
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.send(SidecarCommand::Shutdown).await;
        match tokio::time::timeout(std::time::Duration::from_millis(500), self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(RuntimeError::Tool(format!("failed to wait for voice sidecar: {err}"))),
            Err(_) => {
                self.child
                    .kill()
                    .await
                    .map_err(|err| RuntimeError::Tool(format!("failed to kill voice sidecar: {err}")))
            }
        }
    }
}

pub fn resolve_sidecar_command(command: &str) -> PathBuf {
    let path = PathBuf::from(command);
    if path.components().count() > 1 || path.is_absolute() {
        return path;
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let sibling = dir.join(command);
            if sibling.exists() {
                return sibling;
            }
        }
    }

    path
}

impl Drop for VoiceSidecarHost {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_unqualified_sidecar_command_next_to_current_exe() {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap();
        let sidecar_name = "synaps-test-sidecar-resolution";
        let sidecar_path = dir.join(sidecar_name);
        std::fs::write(&sidecar_path, "#!/bin/sh\n").unwrap();

        let resolved = resolve_sidecar_command(sidecar_name);

        let _ = std::fs::remove_file(&sidecar_path);
        assert_eq!(resolved, sidecar_path);
    }
}
