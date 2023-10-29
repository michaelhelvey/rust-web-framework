use axum::{
    debug_handler,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use color_eyre::Result;
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::info;

#[debug_handler]
async fn get_handler() -> impl IntoResponse {
    // TODO: should probably write something for converting our custom error
    // typtes into 500 HTML...
    let file_contents = tokio::fs::read_to_string("static/index.html")
        .await
        .unwrap();
    Html(file_contents)
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();

    let app = Router::new().route("/", get(get_handler)).layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .into_inner(),
    );

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    info!("Starting server at http://127.0.0.1:{}", port);

    axum::Server::bind(&format!("127.0.0.1:{}", port).parse()?)
        .serve(app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let sigint = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = sigint => {},
        _ = terminate => {},
    }

    println!("signal received, starting graceful shutdown");
}
