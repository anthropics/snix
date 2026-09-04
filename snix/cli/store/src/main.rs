use clap::Parser;
use clap::Subcommand;

use futures::StreamExt;
use futures::TryStreamExt;
use nix_compat::nixhash::{CAHash, NixHash};
use nix_compat::store_path;
use nix_compat::wire::de::Error;
use snix_castore::directoryservice;
use snix_castore::import::fs::ingest_path;
use snix_cli::shutdown_signal;
use snix_cli_store::path_metadata;
use snix_cli_store::upload_closure;
use snix_cli_store::upload_closure::upload_closure;
use snix_store::decompression::DecompressedReader;
use snix_store::nar::NarCalculationService;
use snix_store::utils::ServiceUrls;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio_stream::wrappers::LinesStream;
use tonic::transport::Server;
use tracing::{Instrument, Span, debug, info, info_span, warn};
use tracing_indicatif::span_ext::IndicatifSpanExt;

use snix_castore::proto::GRPCBlobServiceWrapper;
use snix_castore::proto::GRPCDirectoryServiceWrapper;
use snix_castore::proto::blob_service_server::BlobServiceServer;
use snix_castore::proto::directory_service_server::DirectoryServiceServer;
use snix_store::pathinfoservice::{self, PathInfo, PathInfoService};
use snix_store::proto::GRPCPathInfoServiceWrapper;
use snix_store::proto::path_info_service_server::PathInfoServiceServer;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(flatten)]
    tracing_args: snix_tracing::TracingArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, PartialEq, Eq, clap::ValueEnum)]
enum ImportNarOutputFormats {
    /// Print the node itself in urlsafe base64. This can be used in NAR URLs served by nar-bridge
    UrlsafeBase64,

    /// Print the node itself in base64.
    Base64,

    /// Print the node fields as JSON.
    Json,
}

impl std::fmt::Display for ImportNarOutputFormats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ImportNarOutputFormats::UrlsafeBase64 => "urlsafe-base64",
                ImportNarOutputFormats::Base64 => "base64",
                ImportNarOutputFormats::Json => "json",
            }
        )
    }
}

#[derive(Subcommand)]
enum Command {
    /// Runs the snix-store daemon.
    Daemon {
        /// The address to listen on.
        #[clap(flatten)]
        listen_args: tokio_listener::ListenerAddressLFlag,

        #[clap(flatten)]
        service_addrs: ServiceUrls,
    },
    /// Imports a list of paths into the store, print the store path for each of them.
    ImportPath {
        #[clap(value_name = "PATH")]
        paths: Vec<PathBuf>,

        #[clap(flatten)]
        service_addrs: snix_store::utils::ServiceUrlsGrpc,
    },

    /// Imports a list of NAR files into castore, without persisting any NARInfos.
    ImportNar {
        #[clap(value_name = "PATH")]
        paths: Vec<PathBuf>,

        #[clap(flatten)]
        service_addrs: snix_castore::utils::ServiceUrlsGrpc,

        /// The format to use when printing the outputs.
        /// Outputs preserve input path ordering, one per line.
        #[arg(long, default_value_t = ImportNarOutputFormats::Json)]
        format: ImportNarOutputFormats,

        /// The number of paths to import concurrently.
        #[arg(long, default_value_t = 10)]
        concurrency: usize,
    },

    /// Uploads path into snix-[ca]store by retrieving metadata and ingesting from the filesystem.
    Upload {
        #[clap(flatten)]
        service_addrs: snix_store::utils::ServiceUrlsGrpc,

        #[command(subcommand)]
        source: UploadSource,
    },
    /// Mounts a snix-store at the given mountpoint
    #[cfg(feature = "fuse")]
    Mount {
        #[clap(value_name = "PATH")]
        dest: PathBuf,

        #[clap(flatten)]
        service_addrs: snix_store::utils::ServiceUrlsGrpc,

        /// Number of FUSE threads to spawn.
        #[arg(long, env, default_value_t = default_threads())]
        threads: usize,

        #[arg(long, env, default_value_t = false)]
        /// Whether to configure the mountpoint with allow_other.
        /// Requires /etc/fuse.conf to contain the `user_allow_other`
        /// option, configured via `programs.fuse.userAllowOther` on NixOS.
        allow_other: bool,

        /// Whether to list elements at the root of the mount point.
        /// This is useful if your PathInfoService doesn't provide an
        /// (exhaustive) listing.
        #[clap(long, short, action)]
        list_root: bool,

        /// uid:gid to use, instead of 0:0
        #[clap(long, value_parser = parse_uid_gid)]
        uid_gid: Option<(u32, u32)>,

        #[arg(long, default_value_t = true)]
        /// Whether to expose blob and directory digests as extended attributes.
        show_xattr: bool,
    },
    /// Starts a snix-store virtiofs daemon at the given socket path.
    #[cfg(feature = "virtiofs")]
    #[command(name = "virtiofs")]
    VirtioFs {
        #[clap(value_name = "PATH")]
        socket: PathBuf,

        #[clap(flatten)]
        service_addrs: snix_store::utils::ServiceUrlsGrpc,

        /// Whether to list elements at the root of the mount point.
        /// This is useful if your PathInfoService doesn't provide an
        /// (exhaustive) listing.
        #[clap(long, short, action)]
        list_root: bool,

        /// uid:gid to use, instead of 0:0
        #[clap(long, value_parser = parse_uid_gid)]
        uid_gid: Option<(u32, u32)>,

        #[arg(long, default_value_t = true)]
        /// Whether to expose blob and directory digests as extended attributes.
        show_xattr: bool,
    },
    #[cfg(feature = "compat-migrate-directory-sizes")]
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
}

#[derive(Subcommand)]
enum UploadSource {
    /// Uploads a closure of store paths.
    ///
    /// PathInfos are only persisted once all references have been uploaded.
    /// Contents are uploaded eagerly.
    ReferenceGraph {
        /// A path pointing to a JSON file/unix domain socket (or '-' for stdin)
        /// containing store path metadata.
        ///
        /// Needs to provide `narHash`, `narSize`, `references` and optionally
        /// `deriver`, `signatures` per store path, either as a list of objects
        /// with an additional `path` field or as an attrset keyed by store path.
        ///
        /// Produced by the following command, depending on the implementation
        /// and version:
        ///
        /// ```notrust
        /// nix path-info --json --recursive <some-path>
        /// ```
        ///
        /// The former format is provided by Nix < 2.19 and Lix), the latter by
        /// CppNix >= 2.19.
        reference_graph_path: PathBuf,

        /// The number of paths to ingest concurrently.
        #[arg(long, default_value_t = 10)]
        concurrency: usize,
    },

    /// Uploads individual store paths.
    ///
    /// Elements are processed in a streaming fashion, without waiting/checking for
    /// references to be uploaded.
    Jsonl {
        /// A path pointing to a JSONL file/unix domain socket (or '-' for stdin)
        /// containing store path metadata.
        ///
        /// Each line needs to be a JSON object with `narHash`, `narSize`, `references`, 'path'
        /// and optionally `deriver`, `signatures`.
        metadata_lines_path: PathBuf,

        /// The number of items to process concurrently.
        #[arg(long, default_value_t = 10)]
        concurrency: usize,
    },
}

#[cfg(feature = "compat-migrate-directory-sizes")]
#[derive(Subcommand)]
enum ToolsCommand {
    /// Lists all PathInfos, for each one describing a directory, recalculates the directory closure, then puts it back.
    /// This can be used to migrate old Directory closures to use the correct size fields.
    RewriteDirectories {
        #[arg(long, env, default_value = "grpc+http://[::1]:8000")]
        directory_service_addr: String,

        #[arg(long, env, default_value = "grpc+http://[::1]:8000")]
        path_info_service_addr: String,

        /// Don't write new PathInfos with minimized directory sizes
        #[clap(long, short, action)]
        dry_run: bool,

        /// Number of PathInfos to process concurrently
        #[arg(long, env, default_value = "10")]
        concurrency: usize,
    },
}

#[cfg(feature = "fuse")]
fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|threads| threads.into())
        .unwrap_or(4)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();

    let mut tracing_handle = snix_tracing::TracingBuilder::default()
        .handle_tracing_args(&args.tracing_args)
        .build()?;

    let mut stdout_writer = tracing_handle.get_stdout_writer();

    match args.command {
        Command::Daemon {
            listen_args,
            service_addrs,
        } => {
            // initialize stores
            let (blob_service, directory_service, path_info_service, nar_calculation_service) =
                snix_store::utils::construct_services(service_addrs).await?;

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
                )
                .add_service(PathInfoServiceServer::new(GRPCPathInfoServiceWrapper::new(
                    path_info_service,
                    nar_calculation_service,
                )));

            #[cfg(feature = "tonic-reflection")]
            {
                use snix_castore::proto::FILE_DESCRIPTOR_SET as CASTORE_FILE_DESCRIPTOR_SET;
                use snix_store::proto::FILE_DESCRIPTOR_SET;

                router = router.add_service(
                    tonic_reflection::server::Builder::configure()
                        .register_encoded_file_descriptor_set(CASTORE_FILE_DESCRIPTOR_SET)
                        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
                        .build_v1alpha()?,
                );
                router = router.add_service(
                    tonic_reflection::server::Builder::configure()
                        .register_encoded_file_descriptor_set(CASTORE_FILE_DESCRIPTOR_SET)
                        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
                        .build_v1()?,
                );
            }

            let listen_address = &listen_args.listen_address.unwrap_or_else(|| {
                "[::]:8000"
                    .parse()
                    .expect("invalid fallback listen address")
            });

            let listener =
                snix_cli::make_listener(listen_address, &listen_args.listener_options).await?;

            info!(listen_address=%listen_address, "starting daemon");

            router
                .serve_with_incoming_shutdown(listener, shutdown_signal())
                .await?;
        }
        Command::ImportPath {
            paths,
            service_addrs,
        } => {
            // FUTUREWORK: allow flat for single files?
            let (blob_service, directory_service, path_info_service, nar_calculation_service) =
                snix_store::utils::construct_services(service_addrs).await?;

            // For each path passed, construct the name, or bail out if it's invalid.
            let paths_and_names = paths
                .into_iter()
                .map(|p| {
                    let basename = p
                        .file_name()
                        .ok_or_else(|| std::io::Error::invalid_data("path has no basename"))?;

                    let name = store_path::validate_name_from_os_str(basename)
                        .map_err(|_| std::io::Error::invalid_data("basename invalid"))?
                        .to_owned();
                    Ok::<_, std::io::Error>((p, name))
                })
                .collect::<Result<Vec<_>, _>>()?;

            let imports_span =
                info_span!("import paths", "indicatif.pb_show" = tracing::field::Empty);
            imports_span.pb_set_message("Importing");
            imports_span.pb_set_length(paths_and_names.len() as u64);
            imports_span.pb_set_style(&snix_tracing::PB_PROGRESS_STYLE);
            imports_span.pb_start();

            futures::stream::iter(paths_and_names)
                .map(|(path, name)| {
                    let blob_service = blob_service.clone();
                    let directory_service = directory_service.clone();
                    let path_info_service = path_info_service.clone();
                    let nar_calculation_service = nar_calculation_service.clone();
                    let imports_span = imports_span.clone();
                    let mut stdout_writer = stdout_writer.clone();

                    async move {
                        let span = Span::current();
                        span.pb_set_style(&snix_tracing::PB_SPINNER_STYLE);
                        span.pb_set_message(&format!("Ingesting {path:?}"));
                        span.pb_start();

                        // Ingest the contents at the given path into castore.
                        let root_node = ingest_path::<_, _, _, &[u8]>(
                            blob_service,
                            directory_service,
                            &path,
                            None,
                        )
                        .await
                        .map_err(std::io::Error::custom)?;

                        span.pb_set_message(&format!("NAR Calculation for {path:?}"));

                        // Ask for the NAR size and sha256
                        let (nar_size, nar_sha256) =
                            nar_calculation_service.calculate_nar(&root_node).await?;

                        // Calculate the output path. This might still fail, as some names are illegal.
                        // FUTUREWORK: express the `name` at the type level to be valid and check for this earlier.
                        let hash = NixHash::Sha256(nar_sha256);
                        let output_path =
                            store_path::build_ca_path(
                                &name,
                                true,
                                &hash,
                                [],
                                false,
                            )
                            .map_err(|e| {
                                warn!(err=%e, "unable to build CA path");
                                std::io::Error::custom(e)
                            })?;

                        // Construct and insert PathInfo
                        match path_info_service
                            .as_ref()
                            .put(PathInfo {
                                store_path: output_path.to_owned(),
                                node: root_node,
                                // There's no reference scanning on imported paths
                                references: vec![],
                                nar_size,
                                nar_sha256,
                                signatures: vec![],
                                deriver: None,
                                ca: Some(CAHash::Nar(hash)),
                            })
                            .await
                        {
                            // If the import was successful, print the path to stdout.
                            Ok(path_info) => {
                                use std::io::Write;
                                debug!(store_path=%path_info.store_path.as_absolute_path_fmt(), "imported path");
                                writeln!(&mut stdout_writer, "{}", path_info.store_path.as_absolute_path_fmt())?;
                                imports_span.pb_inc(1);
                                Ok(())
                            }
                            Err(e) => {
                                warn!(?path, err=%e, "failed to import");
                                Err(e)
                            }
                        }
                    }.instrument(info_span!("import path", "indicatif.pb_show" = tracing::field::Empty))
                })
                .buffer_unordered(50)
                .try_collect::<()>()
                .await?;
        }

        Command::ImportNar {
            paths,
            service_addrs,
            ref format,
            concurrency,
        } => {
            let (blob_service, directory_service) =
                snix_castore::utils::construct_services(service_addrs).await?;

            // Ingest each NAR concurrently, producing the formatted output line.
            let mut imports = futures::stream::iter(paths)
                .map(|path| {
                    let blob_service = blob_service.clone();
                    let directory_service = &directory_service;
                    async move {
                        let reader = snix_cli::reader_for_path(path).await?;

                        // Transparently decompress the NAR if it is compressed.
                        let mut reader = DecompressedReader::new(reader).await?;

                        let root_node = if let Some(reader) = reader.as_unknown_inner() {
                            snix_store::nar::ingest_nar(blob_service, directory_service, reader)
                                .await?
                        } else {
                            snix_store::nar::ingest_nar(
                                blob_service,
                                directory_service,
                                &mut tokio::io::BufReader::new(reader),
                            )
                            .await?
                        };

                        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(match format {
                            ImportNarOutputFormats::UrlsafeBase64
                            | ImportNarOutputFormats::Base64 => {
                                use prost::Message;
                                let proto_node = snix_castore::proto::Entry::from_name_and_node(
                                    "".into(),
                                    root_node,
                                )
                                .encode_to_vec();
                                if *format == ImportNarOutputFormats::UrlsafeBase64 {
                                    data_encoding::BASE64URL_NOPAD.encode(&proto_node)
                                } else {
                                    data_encoding::BASE64.encode(&proto_node)
                                }
                            }
                            ImportNarOutputFormats::Json => {
                                serde_json::to_string(&root_node).expect("serialize")
                            }
                        })
                    }
                })
                .buffered(concurrency);

            // `buffered` yields in input order, so printing here preserves it.
            use std::io::Write;
            while let Some(output) = imports.try_next().await? {
                writeln!(&mut stdout_writer, "{output}")?;
            }
        }

        Command::Upload {
            service_addrs,
            source,
        } => {
            let (blob_service, directory_service, path_info_service, _nar_calculation_service) =
                snix_store::utils::construct_services(service_addrs).await?;

            match source {
                UploadSource::ReferenceGraph {
                    reference_graph_path,
                    concurrency,
                } => {
                    let reference_graph = {
                        let mut source = snix_cli::reader_for_path(reference_graph_path).await?;
                        // Read the entire file.
                        let mut json_bytes: Vec<u8> = vec![];
                        source.read_to_end(&mut json_bytes).await?;

                        path_metadata::parse_all(&json_bytes).map_err(std::io::Error::other)?
                    };

                    upload_closure(
                        reference_graph,
                        blob_service,
                        directory_service,
                        path_info_service,
                        concurrency,
                    )
                    .await?;
                }
                UploadSource::Jsonl {
                    metadata_lines_path,
                    concurrency,
                } => {
                    let span = tracing::info_span!(
                        "upload_jsonl",
                        "indicatif.pb_show" = tracing::field::Empty
                    );
                    span.pb_set_style(&snix_tracing::PB_SPINNER_LONG_STYLE);
                    span.pb_set_message("Uploading paths");

                    let source = snix_cli::reader_for_path(metadata_lines_path).await?;
                    let s = LinesStream::new(source.lines()).map(|line| {
                        async {
                        let (
                            store_path, path_metadata
                        ) = path_metadata::parse_one(line?.as_bytes())?;

                        if path_info_service
                            .has(*store_path.digest())
                            .await
                            .map_err(std::io::Error::other)?
                        {
                            debug!(path_info.store_path = %store_path, "skipped, already exists");
                            return Ok::<_, std::io::Error>(None);
                        }

                        let path_info = upload_closure::ingest(store_path, path_metadata, &blob_service, &directory_service).await.map_err(std::io::Error::other)?;

                        Ok(Some(path_info_service
                                .put(path_info)
                                .await
                                .map_err(std::io::Error::other)?))
                    }.in_current_span()
                    });
                    s.buffer_unordered(concurrency)
                        // filter out Ok(None).
                        .filter_map(|result| async { result.transpose() })
                        .try_collect::<Vec<PathInfo>>()
                        .await?;
                }
            }
        }
        #[cfg(feature = "fuse")]
        Command::Mount {
            dest,
            service_addrs,
            list_root,
            uid_gid,
            threads,
            allow_other,
            show_xattr,
        } => {
            let (blob_service, directory_service, path_info_service, _nar_calculation_service) =
                snix_store::utils::construct_services(service_addrs).await?;

            use snix_castore::fs::{FSSettings, SnixStoreFs, fuse::FuseDaemon};

            let fs = SnixStoreFs::new(
                blob_service,
                directory_service,
                pathinfoservice::RootNodesWrapper::from(path_info_service),
                FSSettings {
                    list_root,
                    uid_gid_override: uid_gid,
                    show_xattr,
                },
                tokio::runtime::Handle::current(),
            );

            let fuse_daemon = tokio::task::spawn_blocking(move || {
                info!(mount_path=?dest, "mounting");

                FuseDaemon::new(fs, &dest, threads, allow_other)
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
            tokio::task::spawn_blocking(move || fuse_daemon.wait()).await?
        }
        #[cfg(feature = "virtiofs")]
        Command::VirtioFs {
            socket,
            service_addrs,
            list_root,
            uid_gid,
            show_xattr,
        } => {
            let (blob_service, directory_service, path_info_service, _nar_calculation_service) =
                snix_store::utils::construct_services(service_addrs).await?;

            use snix_castore::fs::{FSSettings, SnixStoreFs, virtiofs::start_virtiofs_daemon};

            let fs = SnixStoreFs::new(
                blob_service,
                directory_service,
                pathinfoservice::RootNodesWrapper::from(path_info_service),
                FSSettings {
                    list_root,
                    uid_gid_override: uid_gid,
                    show_xattr,
                },
                tokio::runtime::Handle::current(),
            );

            tokio::task::spawn_blocking(move || {
                info!(socket_path=?socket, "starting virtiofs-daemon");

                start_virtiofs_daemon(fs, socket)
            })
            .await??;
        }

        #[cfg(feature = "compat-migrate-directory-sizes")]
        Command::Tools { command } => {
            let ToolsCommand::RewriteDirectories {
                directory_service_addr,
                path_info_service_addr,
                dry_run,
                concurrency,
            } = command;

            let directory_service =
                snix_castore::directoryservice::from_addr(&directory_service_addr).await?;
            let path_info_service =
                snix_store::pathinfoservice::from_addr(&path_info_service_addr, None).await?;

            let rewrite_directories_span = info_span!(
                "rewrite_directories",
                "indicatif.pb_show" = tracing::field::Empty
            );
            rewrite_directories_span.pb_set_style(&snix_tracing::PB_SPINNER_LONG_STYLE);
            rewrite_directories_span.pb_set_message("Rewriting directories");
            rewrite_directories_span.pb_start();

            let work = path_info_service
                .list()
                .map(move |e| {
                    let path_info_service = path_info_service.clone();
                    let directory_service = directory_service.clone();
                    async move {
                        match e {
                            Ok(path_info) => {
                                use snix_cli_store::rewrite_directories;

                                Ok(rewrite_directories::rewrite_pathinfo(
                                    path_info,
                                    dry_run,
                                    path_info_service,
                                    directory_service,
                                )
                                .await?)
                            }
                            Err(err) => {
                                warn!(%err, "failed to get next PathInfo");
                                Err(err)
                            }
                        }
                    }
                    .in_current_span()
                })
                .buffer_unordered(concurrency)
                .inspect_ok(|_| rewrite_directories_span.pb_inc(1))
                .try_fold((0_u64, 0_u64), |(rewritten, total), x| async move {
                    Ok((if x { rewritten + 1 } else { rewritten }, total + 1))
                });

            tokio::select! {
                _ = snix_cli::shutdown_signal() => {},
                res = work => {
                    match res {
                        Ok((rewritten, total)) => {
                            info!("Done, rewrote {rewritten} of {total} PathInfo");
                        },
                        Err(err) => {
                            if let Err(e) = tracing_handle.shutdown().await {
                                eprintln!("failed to shutdown tracing: {e}");
                            }

                            return Err(err)
                        },
                    }
                }
            }
        }
    };

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
