use askama::Template;
use axum::{debug_handler, extract::Query, response::IntoResponse, routing::get, Router};
use color_eyre::Result;
use serde::Deserialize;
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::info;

#[derive(Template)]
#[template(path = "layout.html")]
struct IndexTemplate<'a> {
    name: &'a str,
}

#[derive(Debug, Deserialize)]
struct IndexQuery {
    name: Option<String>,
}

#[debug_handler]
async fn get_handler(Query(query): Query<IndexQuery>) -> impl IntoResponse {
    IndexTemplate { name: "Michael" }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/", get(get_handler))
        .fallback_service(ServeDir::new("static").append_index_html_on_directories(true))
        .layer(
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
