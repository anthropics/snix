use clap::{Parser, Subcommand};
use snix_castore::blob_engine::BlobServiceEngine;
use snix_castore::directoryservice;
use snix_castore::import::{archive::ingest_archive, fs::ingest_path};
use snix_castore::proto::blob_service_server::BlobServiceServer;
use snix_castore::proto::directory_service_server::DirectoryServiceServer;
use snix_castore::proto::{GRPCBlobServiceWrapper, GRPCDirectoryServiceWrapper};
use snix_castore::{Node, utils::ServiceUrls};
use snix_cli::{make_listener, shutdown_signal};
use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use tokio::fs::{self, File};
use tonic::transport::Server;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(flatten)]
    tracing_args: snix_tracing::TracingArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the snix-castore daemon
    Daemon {
        #[clap(flatten)]
        listen_args: tokio_listener::ListenerAddressLFlag,

        #[clap(flatten)]
        service_addrs: ServiceUrls,
    },

    /// Ingest a directory or tar archive and return its B3Digest
    Ingest {
        /// Path of the directory or tar archive to import
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        #[clap(flatten)]
        service_addrs: ServiceUrls,
    },

    #[cfg(feature = "fuse")]
    /// Mount a directory by its B3Digest with FUSE
    Mount {
        /// B3Digest of the directory to mount (output of `snix-castore ingest`)
        #[arg(value_name = "DIGEST")]
        digest: String,

        /// Path to the mount point for FUSE
        #[arg(value_name = "PATH")]
        dest: PathBuf,

        #[clap(flatten)]
        service_addrs: ServiceUrls,

        /// uid:gid to use, instead of 0:0
        #[clap(long, value_parser = parse_uid_gid)]
        uid_gid: Option<(u32, u32)>,

        #[arg(long, default_value_t = true)]
        /// Whether to expose blob and directory digests as extended attributes.
        show_xattr: bool,
    },

    #[cfg(feature = "virtiofs")]
    /// Expose a directory by its B3Digest through a Virtiofs daemon
    Virtiofs {
        /// B3Digest of the directory to expose (output of `snix-castore ingest`)
        #[arg(value_name = "DIGEST")]
        digest: String,

        /// Path to the virtiofs socket
        #[arg(value_name = "PATH")]
        socket: PathBuf,

        #[clap(flatten)]
        service_addrs: ServiceUrls,

        /// uid:gid to use, instead of 0:0
        #[clap(long, value_parser = parse_uid_gid)]
        uid_gid: Option<(u32, u32)>,

        #[arg(long, default_value_t = true)]
        /// Whether to expose blob and directory digests as extended attributes.
        show_xattr: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = Args::parse();

    let mut tracing_handle = snix_tracing::TracingBuilder::default()
        .handle_tracing_args(&args.tracing_args)
        .build()?;

    match args.command {
        Command::Daemon {
            listen_args,
            service_addrs,
        } => {
            let (blob_service, directory_service) =
                snix_castore::utils::construct_services(service_addrs).await?;

            #[cfg_attr(feature = "otlp", allow(unused_mut))]
            let mut server = Server::builder();
            #[cfg(feature = "otlp")]
            let mut server = server.layer(
                if args
                    .tracing_args
                    .tracers()
                    .contains(snix_tracing::Tracer::Otlp)
                {
                    tonic_tracing_opentelemetry::middleware::server::OtelGrpcLayer::default()
                } else {
                    tonic_tracing_opentelemetry::middleware::server::OtelGrpcLayer::default()
                        .filter(|_| false)
                },
            );

            let (_health_reporter, health_service) = tonic_health::server::health_reporter();

            #[allow(unused_mut)]
            let mut router = server
                .add_service(health_service)
                .add_service(BlobServiceServer::new(GRPCBlobServiceWrapper::new(
                    blob_service,
                )))
                .add_service(
                    DirectoryServiceServer::new(GRPCDirectoryServiceWrapper::new(
                        directory_service,
                    ))
                    .max_decoding_message_size(directoryservice::GRPC_MAX_DECODING_MESSAGE_SIZE),
                );

            #[cfg(feature = "tonic-reflection")]
            {
                use snix_castore::proto::FILE_DESCRIPTOR_SET;

                router = router.add_service(
                    tonic_reflection::server::Builder::configure()
                        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
                        .build_v1alpha()?,
                );
                router = router.add_service(
                    tonic_reflection::server::Builder::configure()
                        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
                        .build_v1()?,
                );
            }

            let listen_address = &listen_args.listen_address.unwrap_or_else(|| {
                "[::]:8000"
                    .parse()
                    .expect("invalid fallback listen address")
            });

            let listener = make_listener(listen_address, &listen_args.listener_options).await?;
            tracing::info!(listen_address=%listen_address, "starting daemon");

            router
                .serve_with_incoming_shutdown(listener, shutdown_signal())
                .await?
        }
        Command::Ingest {
            input,
            service_addrs,
        } => {
            let (blob_service, directory_service) =
                snix_castore::utils::construct_services(service_addrs).await?;

            let metadata = fs::metadata(&input).await?;
            let node = if metadata.is_dir() {
                ingest_path::<_, _, _, &[u8]>(
                    BlobServiceEngine(&blob_service),
                    &directory_service,
                    &input,
                    None,
                )
                .await?
            } else {
                let mut file = File::open(&input).await?;
                ingest_archive(
                    BlobServiceEngine(blob_service.clone()),
                    &directory_service,
                    &mut file,
                )
                .await?
            };
            let digest = match node {
                Node::Directory { digest, .. } => digest,
                _ => return Err("Expected a directory node".into()),
            };
            let mut stdout = tracing_handle.get_stdout_writer();
            writeln!(stdout, "{digest}")?;
        }
        #[cfg(feature = "fuse")]
        Command::Mount {
            digest,
            dest,
            service_addrs,
            uid_gid,
            show_xattr,
        } => {
            let (blob_service, directory_service) =
                snix_castore::utils::construct_services(service_addrs).await?;

            use snix_castore::fs::{FSSettings, SnixStoreFs, fuse::FuseDaemon};

            let digest = digest.parse()?;
            let directory = directory_service
                .get(&digest)
                .await?
                .ok_or("Root directory not found")?;

            let fuse_daemon = tokio::task::spawn_blocking(move || {
                let fs = SnixStoreFs::new(
                    blob_service,
                    directory_service,
                    directory,
                    FSSettings {
                        list_root: true,
                        uid_gid_override: uid_gid,
                        show_xattr,
                    },
                    tokio::runtime::Handle::current(),
                );
                tracing::info!(mount_path=?dest, "mounting");

                FuseDaemon::new(fs, &dest, 4, true)
            })
            .await??;

            // Wait for a ctrl_c and then call fuse_daemon.unmount().
            tokio::spawn({
                let fuse_daemon = fuse_daemon.clone();
                async move {
                    shutdown_signal().await;
                    tokio::task::spawn_blocking(move || fuse_daemon.unmount()).await??;
                    Ok::<_, std::io::Error>(())
                }
            });

            // Wait for the server to finish, which can either happen through it
            // being unmounted externally, or receiving a signal invoking the
            // handler above.
            tokio::task::spawn_blocking(move || fuse_daemon.wait()).await?;
        }
        #[cfg(feature = "virtiofs")]
        Command::Virtiofs {
            digest,
            socket,
            service_addrs,
            uid_gid,
            show_xattr,
        } => {
            let (blob_service, directory_service) =
                snix_castore::utils::construct_services(service_addrs).await?;

            use snix_castore::fs::{FSSettings, SnixStoreFs, virtiofs::start_virtiofs_daemon};

            let digest = digest.parse()?;
            let directory = directory_service
                .get(&digest)
                .await?
                .ok_or("Root directory not found")?;

            tokio::task::spawn_blocking(move || {
                let fs = SnixStoreFs::new(
                    blob_service,
                    directory_service,
                    directory,
                    FSSettings {
                        list_root: true,
                        uid_gid_override: uid_gid,
                        show_xattr,
                    },
                    tokio::runtime::Handle::current(),
                );
                tracing::info!(socket_path=?socket, "starting virtiofs-daemon");

                start_virtiofs_daemon(fs, socket)
            })
            .await??;
        }
    }

    Ok(tracing_handle.shutdown().await.inspect_err(|err| {
        eprintln!("failed to shutdown tracing: {err}");
    })?)
}

#[cfg(any(feature = "fuse", feature = "virtiofs"))]
fn parse_uid_gid(s: &str) -> Result<(u32, u32), &'static str> {
    match s.split_once(":") {
        Some((left, right)) => {
            let uid: u32 = left.parse().map_err(|_| "invalid lhs")?;
            let gid: u32 = right.parse().map_err(|_| "invalid rhs")?;
            Ok((uid, gid))
        }
        None => Err("no delimiter found"),
    }
}
