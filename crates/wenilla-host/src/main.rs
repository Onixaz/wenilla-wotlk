//! `wenilla-host` — the piece of the browser build that can't be static hosting: it serves the
//! wasm bundle, answers game-data reads out of the real MPQ chain, and proxies the two TCP ports
//! the client needs (login 3724, world 8085) over WebSocket, since a browser tab cannot open a
//! raw socket. See `web/README.md` for
//! the exact URL/encoding rules the client lanes code against.
//!
//! This host is a **local development tool**. `/data` hands out the operator's game files to
//! anyone who can reach the socket, with no login, so it binds to loopback by default and must
//! never be exposed on the open internet. Multi-user hosting is `wenilla-realm`, which mounts the
//! same routers behind a session cookie.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use benilla_formats::Chain;
use clap::Parser;

#[derive(Parser)]
#[command(
    about = "Serve the benilla browser build, its game data, and its net proxy (local testing only)",
    after_help = "wenilla-host is for local testing. /data serves the game files under --data to \
                  anyone who can reach the socket, without any login. Keep it on loopback or a \
                  private network; never expose it on the open internet. For hosting players, \
                  use wenilla-realm."
)]
struct Cli {
    /// Address to listen on. Loopback by default; anything else exposes `/data` to that
    /// network unauthenticated (see `--help`).
    #[arg(long, default_value = "127.0.0.1:8090")]
    bind: String,
    /// Directory holding the wasm-bindgen output (`index.html`, `wenilla.js`, `*.wasm`) —
    /// `scripts/web-build.sh`'s `web/dist/`.
    #[arg(long)]
    www: PathBuf,
    /// The 3.3.5a `Data` directory (or a single `.MPQ`) the chain opens — the same one `benilla`
    /// itself reads on the desktop.
    #[arg(long)]
    data: PathBuf,
    /// Host the `/ws/{port}` proxy dials for the allowed ports — the mangos boxes this host
    /// itself runs against, so it defaults to loopback.
    #[arg(long, default_value = "127.0.0.1")]
    upstream: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    let version = wenilla_host::data::data_version(&cli.data);
    tracing::info!(version = ?version, "data version");
    let site = |chain: Chain, locale: Option<String>| {
        wenilla_host::data::router_with(Arc::new(chain), locale, version.clone())
            .merge(wenilla_host::static_site::router(&cli.www))
    };

    let locales = if cli.data.is_dir() {
        benilla_formats::data_locales(&cli.data)
            .with_context(|| format!("listing {}", cli.data.display()))?
    } else {
        Vec::new()
    };
    let pages = if locales.is_empty() {
        let chain =
            Chain::open(&cli.data).with_context(|| format!("opening {}", cli.data.display()))?;
        site(chain, benilla_formats::data_locale(&cli.data).ok())
    } else {
        let mut sites = Vec::with_capacity(locales.len());
        for folder in locales {
            let chain = Chain::open_locale(&cli.data, folder)
                .with_context(|| format!("opening {} as {folder}", cli.data.display()))?;
            tracing::info!(locale = folder, "patch chain open");
            sites.push((folder.to_string(), site(chain, Some(folder.to_string()))));
        }
        let default = benilla_formats::data_locale(&cli.data)
            .with_context(|| format!("choosing a locale in {}", cli.data.display()))?;
        wenilla_host::locale_site::router(sites, &default)
    };
    let app = pages.merge(wenilla_host::ws::router(
        cli.upstream.clone(),
        wenilla_host::ALLOWED_PORTS,
    ));

    let listener = tokio::net::TcpListener::bind(&cli.bind)
        .await
        .with_context(|| format!("binding {}", cli.bind))?;
    if !binds_loopback(&listener) {
        tracing::warn!(
            bind = %cli.bind,
            "wenilla-host is a local testing tool: /data serves your game files to anyone who \
             can reach this address, with no login. Do not expose it on the open internet — use \
             wenilla-realm to host players."
        );
    }
    tracing::info!(bind = %cli.bind, www = %cli.www.display(), upstream = %cli.upstream, "wenilla-host listening");
    axum::serve(listener, app).await.context("serving")
}

/// Is the listener bound to a loopback address (the only place `/data` is safe unauthenticated)?
fn binds_loopback(listener: &tokio::net::TcpListener) -> bool {
    listener
        .local_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false)
}
