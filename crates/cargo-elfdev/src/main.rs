use std::{
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};

use color_eyre::Result;
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Event, EventKind, RecommendedWatcher, Watcher,
};
use tokio::process::{Child, Command};
use tracing::{debug, info};

/// The dev server is implemented in a client-server model between Tokio tasks, where many producer
/// threads wait on various events (signals, files changed, etc), and signal the "manager" thread to
/// do something in response, such as restarting the dev server, or simply terminating the process.
enum DevServerMessage {
    Shutdown { signal: &'static str },
    Restart,
}

/// The state that we are in makes a difference to how we handle certain kinds of signals.
#[derive(Clone)]
enum DevServerState {
    Idle,
    Restarting,
}

async fn wait_for_server_start() -> Result<()> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());

    loop {
        let socket = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await;
        match socket {
            Ok(_) => {
                debug!("child server is listening on port {}", port);
                break;
            }
            Err(e) => {
                debug!("server failed listen check: {:?}", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    Ok(())
}

async fn start_child_server() -> Result<Child> {
    let cmd = Command::new("cargo").args(&["run", "-p", "elf"]).spawn()?;
    wait_for_server_start().await?;

    return Ok(cmd);
}

async fn create_signal_listener(
    state: Arc<RwLock<DevServerState>>,
    tx: tokio::sync::mpsc::Sender<DevServerMessage>,
) -> Result<()> {
    'signal_loop: loop {
        let sigint = async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for SIGINT");
        };

        let sigterm = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to listen for SIGTERM")
                .recv()
                .await;
        };

        let sig_child = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
                .expect("failed to listen for SIGCHLD")
                .recv()
                .await;
        };

        tokio::select! {
            _ = sigint => {
                tx.send(DevServerMessage::Shutdown { signal: "SIGINT" }).await?;
                break 'signal_loop;
            },
            _ = sigterm => {
                tx.send(DevServerMessage::Shutdown { signal: "SIGTERM" }).await?;
                break 'signal_loop;
            },
            _ = sig_child => {
                debug!("received SIGCHLD in signal processor");
                // This is kinda gross, but somehow we have to drop the read lock before calling
                // send() below.
                let local_state = {
                    let locked_state = state.read().unwrap();
                    (*locked_state).clone()
                };

                match local_state {
                    DevServerState::Idle => {
                        // If the child server crashes, for now, just exit immediately: the user is
                        // responsible for restarting it when they fix the compilation issue or
                        // whatever caused the original crash.
                        debug!("child server crashed, sending exit signal to manager process");
                        tx.send(DevServerMessage::Shutdown { signal: "SIGCHLD" }).await?;
                    }
                    DevServerState::Restarting => {
                        debug!("we received SIGCHLD from ourselves, ignoring");
                    }
                };
            },
        }
    }

    Ok::<_, color_eyre::Report>(())
}

async fn create_manager_process(
    state: Arc<RwLock<DevServerState>>,
    mut rx: tokio::sync::mpsc::Receiver<DevServerMessage>,
    mut cmd: Child,
) -> Result<()> {
    while let Some(msg) = rx.recv().await {
        match msg {
            DevServerMessage::Shutdown { signal } => {
                debug!(
                    "shutting down child server in response to signal: {:?}",
                    signal
                );
                cmd.kill().await?;
                break;
            }
            DevServerMessage::Restart => {
                // This may look strange, but it's important that we don't hold a write guard across
                // the awaits below, or else we'll block other calls to read() (e.g. during our
                // signal handler) during the subsequent awaits while we're waiting for the server
                // to start, which defeats the purpose of sharing mutable memory between our
                // threads.
                {
                    let mut state = state.write().unwrap();
                    *state = DevServerState::Restarting;
                }
                debug!("restarting server...");
                cmd.kill().await?;
                cmd = start_child_server().await?;
                debug!("successfully restarted server");
                {
                    let mut state = state.write().unwrap();
                    *state = DevServerState::Idle;
                }
            }
        }
    }

    Ok::<_, color_eyre::Report>(())
}

fn create_file_watcher(
    tx: tokio::sync::mpsc::Sender<DevServerMessage>,
) -> Result<RecommendedWatcher> {
    let watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let restart_server = || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("could not create Tokio runtime to send message on");

                rt.block_on(async {
                    tx.send(DevServerMessage::Restart)
                        .await
                        .expect("could not send file change message to manager process");
                });
            };

            match res {
                Ok(event) => match event.kind {
                    EventKind::Modify(ModifyKind::Data(_))
                    | EventKind::Remove(RemoveKind::File)
                    | EventKind::Create(CreateKind::File) => {
                        info!("file changed: {:?}", event.paths);
                        restart_server();
                    }
                    _ => {}
                },
                Err(e) => {
                    debug!("error in file watcher: {:?}", e);
                }
            }
        },
        notify::Config::default(),
    )?;

    Ok(watcher)
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();

    let state = Arc::new(RwLock::new(DevServerState::Idle));
    let (tx, rx) = tokio::sync::mpsc::channel::<DevServerMessage>(1);

    let signal_listener = tokio::spawn(create_signal_listener(state.clone(), tx.clone()));

    let cmd = start_child_server().await?;
    let manager_proc = tokio::spawn(create_manager_process(state, rx, cmd));

    // Set up file watcher: note that it's really important that we call watcher.watch() here in the
    // main function or otherwise the magic of Drop will make the whole thing a noop:
    let mut watcher = create_file_watcher(tx.clone())?;
    let path = Path::new("./crates/elf");
    info!("watching {:?}", path.canonicalize().unwrap());
    watcher.watch(path, notify::RecursiveMode::Recursive)?;

    manager_proc.await??;
    signal_listener.await??;

    Ok(())
}
