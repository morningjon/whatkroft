//! whatkroftd: the WhatKroft background daemon.
//!
//! Owns the WhatsApp connection and the local chat store, and serves both to
//! UI clients over a Unix socket. Keeping this separate from the window means
//! messages keep arriving while the window is closed.

mod demo;
mod hub;
mod ipc;
mod store;
mod summary;
mod wa;

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio::sync::watch;

use crate::hub::Hub;
use crate::ipc::{Backend, Daemon};
use crate::store::Store;

const USAGE: &str = "\
whatkroftd: WhatKroft background daemon

Usage: whatkroftd [OPTIONS]

Options:
      --data-dir <DIR>  Where to keep the session and chat databases
                        [default: $XDG_DATA_HOME/whatkroft]
      --demo            Serve a made-up account from memory; no network
      --demo-pairing    Like --demo, but stay on the pairing screen
  -h, --help            Print help
  -V, --version         Print version

Environment:
  WHATKROFT_SOCKET      Socket path [default: $XDG_RUNTIME_DIR/whatkroft/daemon.sock]
  RUST_LOG              Log filter, e.g. debug [default: info]";

#[derive(Default)]
struct Args {
    data_dir: Option<PathBuf>,
    demo: bool,
    demo_pairing: bool,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("whatkroftd {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--demo" => args.demo = true,
            "--demo-pairing" => {
                args.demo = true;
                args.demo_pairing = true;
            }
            "--data-dir" => {
                let dir = iter.next().context("--data-dir needs a directory")?;
                args.data_dir = Some(PathBuf::from(dir));
            }
            other => anyhow::bail!("unknown argument '{other}' (try --help)"),
        }
    }
    Ok(Some(args))
}

fn default_data_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("whatkroft"));
    }
    let home = std::env::var_os("HOME").context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".local/share/whatkroft"))
}

/// Create a directory only this user can enter. It holds either the session
/// keys or the control socket, so nobody else should get in.
fn private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))
}

fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    if path.exists() {
        // A live daemon answers; a socket file left by a crash does not.
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!(
                "another whatkroftd is already running on {}",
                path.display()
            );
        }
        std::fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))?;
    }
    UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let socket = whatkroft_ipc::socket_path();
    let listener = bind_socket(&socket)?;
    let (stop, stopping) = watch::channel(false);

    let (hub, backend, session) = if args.demo {
        let hub = Arc::new(Hub::new(Store::open_in_memory()?));
        demo::start(&hub, args.demo_pairing)?;
        log::info!("demo mode: serving a made-up account, nothing is sent anywhere");
        (hub, Backend::Demo, None)
    } else {
        let data_dir = match args.data_dir {
            Some(dir) => dir,
            None => default_data_dir()?,
        };
        private_dir(&data_dir)?;
        let store = Store::open(&data_dir.join("chats.db")).context("opening chat database")?;
        let hub = Arc::new(Hub::new(store));
        let slot = Arc::new(wa::ClientSlot::default());
        let session = tokio::spawn(wa::run(
            Arc::clone(&hub),
            data_dir.join("session.db"),
            Arc::clone(&slot),
            stopping.clone(),
        ));
        (hub, Backend::WhatsApp(slot), Some(session))
    };

    log::info!("listening on {}", socket.display());
    let daemon = Arc::new(Daemon { hub, backend });
    let server = tokio::spawn(ipc::serve(listener, daemon, stopping));

    let outcome = match session {
        Some(mut session) => tokio::select! {
            _ = shutdown_signal() => {
                let _ = stop.send(true);
                // Let the client flush its state before the process exits.
                session.await.context("WhatsApp task panicked")?
            }
            finished = &mut session => {
                let _ = stop.send(true);
                finished.context("WhatsApp task panicked")?
            }
        },
        None => {
            shutdown_signal().await;
            let _ = stop.send(true);
            Ok(())
        }
    };

    let _ = server.await;
    let _ = std::fs::remove_file(&socket);
    outcome
}
