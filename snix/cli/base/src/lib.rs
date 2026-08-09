use std::env::{join_paths, split_paths, var_os};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use tracing::debug;
use which::which_in;

pub type SnixCliResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const DEFAULT_LIBEXEC_PATH_VAR: &str = "SNIX_LIBEXEC_PATH";

/// Builds a listener. We want to always set sleep_on_errors = true
/// to avoid server errors when running low on file handles.
/// sleep_on_errors became the default in hyper, but is not yet flipped in
/// tokio-listener.
pub async fn make_listener(
    address: &tokio_listener::ListenerAddress,
    user_options: &tokio_listener::UserOptions,
) -> std::io::Result<tokio_listener::Listener> {
    tokio_listener::Listener::bind(
        address,
        &{
            let mut system_options = tokio_listener::SystemOptions::default();
            system_options.sleep_on_errors = true;
            system_options
        },
        user_options,
    )
    .await
}

/// Make an os-specific search path.
///
/// This concatenates `SNIX_LIBEXEC_PATH` environment variable, `default_libexec_path`
/// argument, `PATH` environment variable and the directory of the current executable
/// into one string separated by the os-specific path separator.
///
/// It does this to crate one giant search path of places to look for a sub-command
/// binary.
pub fn make_search_path(default_libexec_path: Option<&str>) -> Option<OsString> {
    let libexec_path = var_os(DEFAULT_LIBEXEC_PATH_VAR);
    let libexec_paths = libexec_path.iter().flat_map(split_paths);

    let default_libexec_paths = default_libexec_path.iter().flat_map(split_paths);

    let path = var_os("PATH");
    let paths = path.iter().flat_map(split_paths);

    let current_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(ToOwned::to_owned));
    let paths = libexec_paths
        .chain(default_libexec_paths)
        .chain(paths)
        .chain(current_exe);
    join_paths(paths).ok()
}

/// Search for a snix sub-command.
///
/// This searches the paths in the `SNIX_LIBEXEC_PATH` environment variable,
/// the `default_libexec_path` argument, the `PATH` environment variable and
/// the directory of the current executable in that order for a binary called
/// `snix-{sub_cmd}` and will return the absolute path to it if found.
pub fn find_command(sub_cmd: &str, default_libexec_path: Option<&str>) -> SnixCliResult<PathBuf> {
    let cwd = std::env::current_exe()
        .ok()
        .and_then(|c| c.parent().map(ToOwned::to_owned))
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| io::Error::other("Could not resolve current directory"))?;
    let binary_name = format!("snix-{sub_cmd}");
    let search_path: Option<OsString> = make_search_path(default_libexec_path);
    debug!(?search_path, binary_name, "Searching for {binary_name}");
    Ok(which_in(binary_name, search_path, cwd)?)
}

/// future that listens to both ctrl-c and sigterm.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    debug!("signal received, shutting down…");
}

/// Opens a given path, with special-casing for `-` as stdin.
pub async fn reader_for_path(
    path: impl AsRef<std::path::Path>,
) -> std::io::Result<Box<dyn tokio::io::AsyncBufRead + Unpin + Send>> {
    use std::os::unix::fs::FileTypeExt;
    use tokio::io::BufReader;

    let path = path.as_ref();
    if path == "-" {
        Ok(Box::new(BufReader::new(tokio::io::stdin())) as Box<_>)
    } else {
        let metadata = tokio::fs::metadata(path).await?;

        if metadata.file_type().is_socket() {
            let stream = tokio::net::UnixStream::connect(path).await?;
            Ok(Box::new(BufReader::new(stream)))
        } else {
            let file = tokio::fs::File::open(path).await?;
            Ok(Box::new(BufReader::new(file)))
        }
    }
}
