use std::net::SocketAddr;

use axum::{
    http::{HeaderValue, Method},
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::{api, jito};

pub async fn start_service(addr: &String, app_state: api::AppState) {
    jito::init_tip_accounts().await.unwrap();
    tokio::spawn(async {
        jito::ws::tip_stream()
            .await
            .expect("Failed to get tip percentiles data");
    });

    let app = Router::new()
        .nest(
            "/api",
            Router::new()
                .route("/swap", post(api::swap))
                .route("/pool/:pool_id", get(api::get_pool))
                .route("/coins/:mint", get(api::coins))
                .route("/token_accounts", get(api::token_accounts))
                .route("/token_accounts/:mint", get(api::token_account))
                .route(
                    "/pool_info/:token_address",
                    get(api::get_pool_by_token_address),
                )
                .nest(
                    "/price",
                    Router::new()
                        .route("/raydium/:token_address", get(api::get_raydium_token_price))
                        .route("/pump/:token_address", get(api::get_pump_token_price)),
                )
                .with_state(app_state),
        )
        .layer(
            CorsLayer::new()
                .allow_origin("*".parse::<HeaderValue>().unwrap())
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::OPTIONS,
                    Method::DELETE,
                ]),
        );

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}
