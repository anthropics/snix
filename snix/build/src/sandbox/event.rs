use std::os::unix::process::ExitStatusExt;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio_util::io::ReaderStream;
use tracing::warn;

/// A common set of events emitted by sandboxed builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    ExitCode(i32),
}

/// Stream stdout, stderr, and exit code from a spawned child process.
///
/// Keeps `guard` alive for the duration of the stream (e.g. FUSE mount, inputs guard).
pub fn stream_process<G: 'static + Send>(
    mut child: tokio::process::Child,
    guard: G,
) -> std::io::Result<BoxStream<'static, SandboxEvent>> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout pipe was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("stderr pipe was not captured"))?;

    let stream = async_stream::stream! {
        let _guard = guard;
        let stdout_stream = ReaderStream::new(stdout)
            .map(|res| res.map(|b| SandboxEvent::Stdout(b.to_vec())));
        let stderr_stream = ReaderStream::new(stderr)
            .map(|res| res.map(|b| SandboxEvent::Stderr(b.to_vec())));

        let output_stream = futures::stream::select(stdout_stream, stderr_stream);
        tokio::pin!(output_stream);
        while let Some(res) = output_stream.next().await {
            match res {
                Ok(event) => yield event,
                Err(e) => {
                    warn!("error reading sandbox output: {e}");
                    break;
                }
            }
        }

        let exit_code = match child.wait().await {
            Ok(status) => status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            Err(_) => 1,
        };

        yield SandboxEvent::ExitCode(exit_code);
    }
    .boxed();

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::process::Command;

    #[tokio::test]
    async fn test_stream_process() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo hello; echo err >&2; exit 42"]);
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().expect("failed to spawn /bin/sh");

        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();
        struct DropGuard(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let mut stream = stream_process(child, DropGuard(Some(drop_tx)))
            .expect("failed to start stream_process");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        while let Some(event) = stream.next().await {
            match event {
                SandboxEvent::Stdout(bytes) => stdout.extend(bytes),
                SandboxEvent::Stderr(bytes) => stderr.extend(bytes),
                SandboxEvent::ExitCode(code) => exit_code = Some(code),
            }
        }

        assert_eq!(stdout, b"hello\n");
        assert_eq!(stderr, b"err\n");
        assert_eq!(exit_code, Some(42));
        assert_eq!(
            drop_rx.await,
            Ok(()),
            "guard was dropped after stream ended"
        );
    }
}
