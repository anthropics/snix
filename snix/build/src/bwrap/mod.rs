use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use futures::stream::BoxStream;
use tokio::process::Command;

use crate::sandbox::{
    InputsProvider, SandboxOutputs, SandboxSpec,
    event::{SandboxEvent, stream_process},
};

const COMMON_BWRAP_ARGS: &[&str] = &[
    "--unshare-uts",
    "--unshare-ipc",
    "--unshare-pid",
    "--die-with-parent",
    "--as-pid-1",
    "--unshare-user",
    "--uid",
    "1000",
    "--gid",
    "100",
    "--clearenv",
    "--tmpfs",
    "/",
    "--dev",
    "/dev",
    "--proc",
    "/proc",
    "--tmpfs",
    "/tmp",
];

const ETC_PASSWD: &[u8] = b"
root:x:0:0:Nix build user:/build:/noshell
nixbld:x:1000:100:Nix build user:/build:/noshell
nobody:x:65534:65534:Nobody:/:/noshell
";

const ETC_GROUP: &[u8] = b"
root:x:0:
nixbld:!:100:
nogroup:x:65534:
";

const ETC_HOSTS: &[u8] = b"
127.0.0.1 localhost
::1 localhost
";

const ETC_NSSWITCH: &[u8] = b"
hosts: files dns
services: files
";

/// Bubblewrap based sandbox executor.
///
/// It executes the sandbox command in separate uts, ipc, pid and user namespaces,
/// always runs as uid=1000(nixbld) and gid=100(nixbld) inside the namespace. Provides sane
/// defaults for various `/etc` files.
///
/// Network is optionally disabled with a separate network namespace based on the value of
/// [SandboxSpec::allow_network].
///
/// The root filesystem is tmpfs, has /dev and /proc.
///
/// The rest of the filesystem is based on the [SandboxSpec::scratches], [SandboxSpec::additional_files]
/// and [SandboxSpec::inputs_provider].
///
/// # Scratches
///
/// A list of read-write directories available inside the sandbox, these directories are also left
/// available on the host after the sandbox has finished.
///
/// # Additional files
///
/// A list of read-write files whose path currently *must* resolve into one of the Scratches.
///
/// # Build Inputs([SandboxSpec::inputs_provider])
///
/// A read-only directory that contains any files required by the sandboxed command, e.g
/// `/nix/store`.
/// Before the sandbox starts, the [SandboxSpec::inputs_provider] will have a chance to populate
/// this directory and clean up after the sandbox is stopped.
///
/// **Note**: If the build inputs directory overlaps with any of the scratches, an overlayfs mount
/// will be created for that scratch so it remains writable, i.e. the sandboxed command can create
/// new files/directories.
pub struct Bwrap {
    host_workdir: PathBuf,
    args: Vec<OsString>,
    inputs_provider: InputsProvider,
}

/// Allows finding outputs produced by the sandboxed command.
#[derive(Debug, Clone)]
pub struct BwrapOutputs {
    scratch_dir: PathBuf,
}

impl SandboxOutputs for BwrapOutputs {
    /// Allows finding outputs produced by the sandboxed command.
    ///
    /// The command must write into one of the scratches.
    async fn find_path(&self, path: impl AsRef<Path>) -> Option<PathBuf> {
        let path = self.scratch_dir.join(path);
        // Exists follows symlinks so may return false incorrectly, as nix builds are apparently
        // allowed to produce broken symlinks as their $out...
        // i.e. `runCommand "test" {} "ln -s IdontExist $out"` is a valid nix build.
        //
        // Additionally, by the time find_path is called, the build has already unmonted the
        // fuse store, which means that even valid symlinks can be "broken" during ingestion.
        if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await {
            metadata.is_symlink() || metadata.is_dir() || metadata.is_file()
        } else {
            false
        }
        .then_some(path)
    }
}

impl Bwrap {
    /// Run the sandbox and return a stream of events alongside an outputs handle.
    pub fn run(self) -> std::io::Result<(BoxStream<'static, SandboxEvent>, BwrapOutputs)> {
        let outputs = BwrapOutputs {
            scratch_dir: self.host_workdir.join("scratches"),
        };

        let guard = self
            .inputs_provider
            .provide_inputs(self.host_workdir.join("host_inputs_dir"))?;

        let child = Command::new("bwrap")
            .args(self.args)
            // Make sure we've closed stdin otherwise builds can hang forever blocked on std io.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stream = stream_process(child, guard)?;

        Ok((stream, outputs))
    }

    /// Constructor.
    pub fn initialize(spec: SandboxSpec) -> std::io::Result<Bwrap> {
        let scratch_dir = spec.host_workdir().join("scratches");
        fs::create_dir_all(&scratch_dir)?;
        let mut args: Vec<OsString> = COMMON_BWRAP_ARGS.iter().map(|s| s.into()).collect();
        if !spec.allow_network() {
            args.push("--unshare-net".into());
        }
        for env in spec.env_vars() {
            args.extend([
                "--setenv".into(),
                env.key.clone().into(),
                str::from_utf8(&env.value)
                    .expect("invalid string in env")
                    .into(),
            ]);
        }

        let host_inputs_dir = spec.host_workdir().join("host_inputs_dir");
        fs::create_dir_all(&host_inputs_dir)?;
        args.extend([
            "--ro-bind".into(),
            Path::new("/").join(&host_inputs_dir).into(),
            Path::new("/")
                .join(spec.inputs_provider().inputs_dir())
                .into(),
        ]);
        for scratch in spec.scratches() {
            let scratch_path = scratch_dir.join(scratch);
            fs::create_dir_all(&scratch_path)?;
            if scratch == spec.inputs_provider().inputs_dir() {
                let overlay_workdir = spec.host_workdir().join("overlay_workdir");
                fs::create_dir_all(&overlay_workdir)?;
                args.extend([
                    "--overlay-src".into(),
                    OsString::from(&host_inputs_dir),
                    "--overlay".into(),
                    scratch_path.into(),
                    overlay_workdir.into(),
                    Path::new("/")
                        .join(spec.inputs_provider().inputs_dir())
                        .into(),
                ]);
            } else {
                args.extend([
                    "--bind".into(),
                    scratch_path.into(),
                    Path::new("/").join(scratch).into(),
                ]);
            }
        }
        args.extend([
            "--chdir".into(),
            Path::new("/").join(spec.sandbox_workdir()).into(),
        ]);

        if let Some(shell) = spec.provide_shell() {
            args.extend_from_slice(&["--ro-bind".into(), shell.into(), "/bin/sh".into()]);
        }

        for file in spec.additional_files() {
            let mut found = false;
            for scratch in spec.scratches() {
                if file.path.starts_with(scratch) {
                    found = true;
                }
            }
            if !found {
                return Err(std::io::Error::other(format!(
                    "Additional file does not belong to any scratch: {:?}",
                    file.path
                )));
            }
            // TODO: prevent files from escaping the sandbox, i.e. don't allow additional files
            // of this form: build/../../hello.
            let file_path = scratch_dir.join(&file.path);
            fs::create_dir_all(file_path.parent().expect("parent"))?;
            fs::write(&file_path, &file.contents)?;
        }
        let etc = &spec.host_workdir().join("etc");
        fs::create_dir_all(etc)?;
        fs::write(etc.join("passwd"), ETC_PASSWD)?;
        fs::write(etc.join("group"), ETC_GROUP)?;
        fs::write(etc.join("hosts"), ETC_HOSTS)?;
        fs::write(etc.join("nsswitch.conf"), ETC_NSSWITCH)?;

        args.extend([
            "--ro-bind".into(),
            etc.join("passwd").into(),
            "/etc/passwd".into(),
            "--ro-bind".into(),
            etc.join("group").into(),
            "/etc/group".into(),
        ]);
        if spec.allow_network() {
            args.extend([
                "--ro-bind".into(),
                "/etc/hosts".into(),
                "/etc/hosts".into(),
                "--ro-bind".into(),
                "/etc/resolv.conf".into(),
                "/etc/resolv.conf".into(),
                "--ro-bind".into(),
                "/etc/services".into(),
                "/etc/services".into(),
                "--ro-bind".into(),
                etc.join("nsswitch.conf").into(),
                "/etc/nsswitch.conf".into(),
            ]);
            //TODO: Create /etc/nsswitch.conf with: "hosts: files dns\nservices: files\n"
        } else {
            // Use predefined /etc/hosts like nix does.
            // Among other things it is required for libuv getaddrinfo() tests to pass.
            args.extend([
                "--ro-bind".into(),
                etc.join("hosts").into(),
                "/etc/hosts".into(),
            ]);
        }
        args.extend(spec.command().into_iter().map(|s| s.into()));

        Ok(Self {
            host_workdir: spec.host_workdir().into(),
            args,
            inputs_provider: spec.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_find_path() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let scratch_dir = temp_dir.path();

        let file_path = scratch_dir.join("file.txt");
        std::fs::write(&file_path, b"hello").expect("failed to write test file");

        let symlink_path = scratch_dir.join("link");
        std::os::unix::fs::symlink("nonexistent", &symlink_path)
            .expect("failed to create test symlink");

        let outputs = BwrapOutputs {
            scratch_dir: scratch_dir.to_path_buf(),
        };

        assert_eq!(outputs.find_path("file.txt").await, Some(file_path));
        assert_eq!(outputs.find_path("link").await, Some(symlink_path));
        assert_eq!(outputs.find_path("missing").await, None);
    }
}
