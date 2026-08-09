#[cfg(feature = "otlp")]
use axum_tracing_opentelemetry::middleware::OtelAxumLayer;
use clap::Parser;
use mimalloc::MiMalloc;
use nar_bridge::AppState;
use snix_cli::shutdown_signal;
use snix_store::utils::ServiceUrlsGrpc;
use std::num::NonZeroUsize;
use tracing::info;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Expose the Nix HTTP Binary Cache protocol for a snix-store.
/// Depending on the store backend(s) this is points at, it can also support
/// uploads.
/// While it doesn't support "Compression" as Nix understands it, it does use
/// the HTTP Content-Encoding feature to zstd-compress over the wire, if the
/// client negotiates it (which libcurl and by this Nix also does).
/// This means uploads need to happen with compression=none,
/// and NARInfo will say "Compression: none", but transfers are still
/// compressed.
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(flatten)]
    service_addrs: ServiceUrlsGrpc,

    /// The priority to announce at the `nix-cache-info` endpoint.
    /// A lower number means it's *more preferred.
    #[arg(long, env, default_value_t = 39)]
    priority: u64,

    /// The address to listen on.
    #[clap(flatten)]
    listen_args: tokio_listener::ListenerAddressLFlag,

    /// The capacity of the lookup table from NarHash to [Node].
    /// Should be bigger than the number of concurrent NAR uploads.
    #[arg(long, env, default_value_t = NonZeroUsize::new(1000).unwrap())]
    root_nodes_cache_capacity: NonZeroUsize,

    #[clap(flatten)]
    pub tracing_args: snix_tracing::TracingArgs,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();

    let mut tracing_handle = snix_tracing::TracingBuilder::default()
        .handle_tracing_args(&args.tracing_args)
        .build()?;

    // initialize stores
    let (blob_service, directory_service, path_info_service, _nar_calculation_service) =
        snix_store::utils::construct_services(args.service_addrs).await?;

    let state = AppState::new(
        blob_service,
        directory_service,
        path_info_service,
        args.root_nodes_cache_capacity,
    );

    let app = nar_bridge::gen_router(args.priority);

    #[cfg(feature = "otlp")]
    let app = app.layer(
        if args
            .tracing_args
            .tracers()
            .contains(snix_tracing::Tracer::Otlp)
        {
            OtelAxumLayer::default()
        } else {
            OtelAxumLayer::default().filter(|_| false)
        },
    );

    let app = app.with_state(state);

    let listen_address = &args.listen_args.listen_address.unwrap_or_else(|| {
        "[::]:9000"
            .parse()
            .expect("invalid fallback listen address")
    });

    let listener =
        snix_cli::make_listener(listen_address, &args.listen_args.listener_options).await?;

    info!(listen_address=%listen_address, "starting daemon");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(tracing_handle.shutdown().await.inspect_err(|err| {
        eprintln!("failed to shutdown tracing: {err}");
    })?)
}
