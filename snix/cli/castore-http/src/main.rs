use clap::Parser;
use snix_castore::Node;
use snix_castore::{B3Digest, utils::ServiceUrlsGrpc};
use snix_castore_http::app_state::AppConfig;
use snix_cli::shutdown_signal;
use std::sync::Arc;
use tracing::info;

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// The address to listen on.
    #[clap(flatten)]
    listen_args: tokio_listener::ListenerAddressLFlag,
    #[clap(flatten)]
    service_addrs: ServiceUrlsGrpc,
    /// The root directory digest to serve.
    #[arg(short, long)]
    root_directory: B3Digest,
    /// The name of the file to serve if a client requests a directory, separated by ','. e.g. "index.html,index.htm"
    #[arg(long, value_delimiter = ',')]
    index_names: Vec<String>,
    /// Whether a directory listing should be returned if a client requests a directory but none of the `index_names` matched
    #[arg(short, long)]
    auto_index: bool,

    #[clap(flatten)]
    tracing_args: snix_tracing::TracingArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let mut tracing_handle = snix_tracing::TracingBuilder::default()
        .handle_tracing_args(&args.tracing_args)
        .build()?;

    let (blob_service, directory_service) =
        snix_castore::utils::construct_services(args.service_addrs)
            .await
            .expect("failed to construct services");

    let state = Arc::new(AppConfig {
        blob_service,
        directory_service,
        root_node: Node::Directory {
            digest: args.root_directory,
            // size doesn't really matter here, we're not doing inode allocation.
            size: 0,
        },
        index_names: args.index_names.to_vec(),
        auto_index: args.auto_index,
    });

    let app = snix_castore_http::router::gen_router(state);

    let listen_address = &args.listen_args.listen_address.unwrap_or_else(|| {
        "[::]:9000"
            .parse()
            .expect("invalid fallback listen address")
    });

    let listener =
        snix_cli::make_listener(listen_address, &args.listen_args.listener_options).await?;

    info!(%listen_address, "starting daemon");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(tracing_handle.shutdown().await.inspect_err(|err| {
        eprintln!("failed to shutdown tracing: {err}");
    })?)
}
