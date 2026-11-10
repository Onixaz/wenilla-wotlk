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

use std::path::{Path, PathBuf};
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
    /// The locale folder plain `/data/*` serves (`esES`, …), which must be one under `--data`.
    /// Without it, the folder the 3.3.5a client mounts with no `locale` CVar: `enUS`, else the
    /// next one in the client's table. Every locale folder is served under `/data/__l/{locale}/`.
    #[arg(long)]
    locale: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    let app = if cli.data.is_dir() {
        data_router(&cli.data, cli.locale.as_deref())?
    } else {
        // A single `.MPQ`: one chain, no locale, as before.
        anyhow::ensure!(cli.locale.is_none(), "--locale needs a Data folder");
        let chain = Arc::new(
            Chain::open(&cli.data).with_context(|| format!("opening {}", cli.data.display()))?,
        );
        let version = wenilla_host::data::data_version(&cli.data);
        tracing::info!(data = %cli.data.display(), version = ?version, "archive open");
        wenilla_host::data::router_with(chain, None, version)
    };
    let app = app
        .merge(wenilla_host::ws::router(
            cli.upstream.clone(),
            wenilla_host::ALLOWED_PORTS,
        ))
        .merge(wenilla_host::static_site::router(&cli.www));

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

/// The `/data` router over every locale folder of the `Data` directory `data`, plain `/data/*`
/// serving `locale` (or the folder `enUS` resolves to). A locale whose chain fails to open is
/// left out with a warning, unless it is the default.
fn data_router(data: &Path, locale: Option<&str>) -> Result<axum::Router> {
    let folders = benilla_formats::data_locales(data)
        .with_context(|| format!("listing {}", data.display()))?;
    let default = match locale {
        Some(wanted) => folders
            .iter()
            .copied()
            .find(|f| *f == wanted)
            .with_context(|| {
                format!(
                    "--locale {wanted}: the locales under {} are {folders:?}",
                    data.display()
                )
            })?,
        None => benilla_formats::resolve_data_locale(data, "enUS")
            .with_context(|| format!("{} holds no locale folder", data.display()))?,
    };
    let mut served = Vec::new();
    for (folder, chain) in folders.iter().zip(Chain::open_locales(data, &folders)) {
        let chain = match chain {
            Ok(chain) => chain,
            Err(e) if *folder != default => {
                tracing::warn!(locale = folder, error = %format!("{e:#}"), "locale left out");
                continue;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("opening {} as {folder}", data.display()))
            }
        };
        let version = wenilla_host::data::data_version_for(data, folder);
        tracing::info!(locale = folder, version = ?version, "patch chain open");
        served.push(wenilla_host::data::LocaleData {
            locale: folder.to_string(),
            chain: Arc::new(chain),
            version,
        });
    }
    tracing::info!(data = %data.display(), default, "serving every locale");
    Ok(wenilla_host::data::router_locales(served, default))
}

/// Is the listener bound to a loopback address (the only place `/data` is safe unauthenticated)?
fn binds_loopback(listener: &tokio::net::TcpListener) -> bool {
    listener
        .local_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false)
}
