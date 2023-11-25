use std::{
    future::Future,
    net::SocketAddr,
    path::Path,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
    vec,
};
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use axum::{
    extract::{
        ws::{Message, WebSocketUpgrade},
        ConnectInfo, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_macros::debug_handler;
use color_eyre::Result;
use futures::{SinkExt, StreamExt};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Event, EventKind, RecommendedWatcher, Watcher,
};
use tokio::process::{Child, Command};
use tracing::{debug, info};

/// The dev server is implemented in a client-server model between Tokio tasks, where many producer
/// threads wait on various events (signals, files changed, etc), and signal the "manager" thread to
/// do something in response, such as restarting the dev server, or simply terminating the process.
#[derive(Clone, Debug)]
enum DevServerMessage {
    Shutdown { signal: &'static str },
    Restart,
    ReloadBrowser,
}

/// The state that we are in makes a difference to how we handle certain kinds of signals.
#[derive(Clone)]
enum DevServerState {
    Idle,
    Restarting,
}

/// Future that waits until port ${PORT:-3000} is able to be connected to.
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

    // TODO: decouple the concept of "create the child process" and "wait for the child process to
    // start a server" so that we can do other things (like listen for signals) while we're waiting
    // for the child server to start.
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
    self_tx: tokio::sync::mpsc::Sender<DevServerMessage>,
    ws_tx: async_channel::Sender<()>,
    mut cmd: Child,
    ws_cancel_token: CancellationToken,
) -> Result<()> {
    while let Some(msg) = rx.recv().await {
        match msg {
            DevServerMessage::Shutdown { signal } => {
                debug!(
                    "shutting down child server in response to signal: {:?}",
                    signal
                );
                cmd.kill().await?;
                ws_cancel_token.cancel();
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
                self_tx.send(DevServerMessage::ReloadBrowser).await?;
            }
            DevServerMessage::ReloadBrowser => {
                debug!("reloading browser...");
                ws_tx.send(()).await?;
            }
        }
    }

    Ok::<_, color_eyre::Report>(())
}

fn create_file_watcher(
    tx: tokio::sync::mpsc::Sender<DevServerMessage>,
    message: DevServerMessage,
) -> Result<RecommendedWatcher> {
    let watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let send_change_signal = || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("could not create Tokio runtime to send message on");

                rt.block_on(async {
                    tx.send(message.clone())
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
                        send_change_signal();
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

#[debug_handler]
async fn websocket_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(rx): State<Arc<async_channel::Receiver<()>>>,
) -> impl IntoResponse {
    debug!("received websocket connection from {addr}");
    ws.on_upgrade(|mut socket| async move {
        if socket
            .send(Message::Ping((vec![1, 2, 3]).into()))
            .await
            .is_ok()
        {
            debug!("websocket connection established");
        } else {
            debug!("websocket connection failed");
            return;
        }

        let (mut sender, mut receiver) = socket.split();

        let mut recv_task = tokio::spawn(async move {
            // When this breaks, that means the the client has hung up the connection.
            while let Some(Ok(msg)) = receiver.next().await {
                debug!("received message from browser: {:?}", msg);
            }
        });

        let mut send_task = tokio::spawn(async move {
            // When this breaks, it means that the server has closed the reload channel.
            while let Ok(_) = rx.recv().await {
                debug!("received reload signal from manager process");
                sender.send(Message::Text("reload".into())).await.unwrap();
            }
        });

        // When either tasks exits (either because the client closed the connection or because the
        // server is exiting), abort the other one.
        tokio::select! {
            _ = (&mut recv_task) => {
                debug!("websocket connection closed");
                send_task.abort();
            },
            _ = (&mut send_task) => {
                debug!("reload channel closed");
                recv_task.abort();
            },
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();

    let state = Arc::new(RwLock::new(DevServerState::Idle));
    let (tx, rx) = tokio::sync::mpsc::channel::<DevServerMessage>(1);

    // Signal listener that sends message to our manager process:
    let signal_listener = tokio::spawn(create_signal_listener(state.clone(), tx.clone()));

    // Watcher for the Rust server:
    let mut server_watcher = create_file_watcher(tx.clone(), DevServerMessage::Restart)?;
    let path = Path::new("./crates/elf");
    info!("watching rust server at {:?}", path.canonicalize().unwrap());
    server_watcher.watch(path, notify::RecursiveMode::Recursive)?;

    // Watcher for the static files:
    let mut static_watcher = create_file_watcher(tx.clone(), DevServerMessage::ReloadBrowser)?;
    let path = Path::new("./static");
    info!(
        "watching static files at {:?}",
        path.canonicalize().unwrap()
    );
    static_watcher.watch(path, notify::RecursiveMode::Recursive)?;

    // Websocket server for triggering browser reloads:
    let (ws_tx, ws_rx) = async_channel::unbounded();
    let ws_rx_ptr = Arc::new(ws_rx);

    let live_reload_server = Router::new()
        .route(
            "/",
            get(|| async { "Elf dev server: connect to websocket at /ws for reload events" }),
        )
        .route("/ws", get(websocket_handler))
        .with_state(ws_rx_ptr)
        .layer(TraceLayer::new_for_http());

    let addr = "127.0.0.1:6969";
    let ws_cancel_token = CancellationToken::new();
    let ws_server_cancel_token = ws_cancel_token.clone();

    debug!("websocket server listening on {addr}");
    let server = axum::Server::bind(&addr.parse().unwrap())
        .serve(live_reload_server.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            ws_server_cancel_token.cancelled().await;
            debug!("websocket server shutting down");
        });

    // Child server process:
    let cmd = start_child_server().await?;

    // Manager process:
    let ws_manager_cancel_token = ws_cancel_token.clone();
    let ws_manager_tx = ws_tx.clone();
    let manager_self_tx = tx.clone();
    let manager_proc = tokio::spawn(create_manager_process(
        state,
        rx,
        manager_self_tx,
        ws_manager_tx,
        cmd,
        ws_manager_cancel_token,
    ));

    server.await?;
    manager_proc.await??;
    signal_listener.await??;

    Ok(())
}
