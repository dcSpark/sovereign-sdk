use axum::debug_handler;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tee::maa::attestation::ResponseStruct;
use std::io::Write;
use std::io;
use std::net::SocketAddr;
use tempfile::NamedTempFile;

#[derive(Debug)]
pub enum ServerError {
    InternalServerError(String),
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let body = format!("Internal server error. Please try again. Error: {}", match self {
            ServerError::InternalServerError(msg) => msg,
        });

        // its often easiest to implement `IntoResponse` by calling other implementations
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}


impl From<io::Error> for ServerError {
    fn from(err: io::Error) -> Self {
        ServerError::InternalServerError(err.to_string())
    }
}

impl From<ServerError> for anyhow::Error {
    fn from(err: ServerError) -> Self {
        match err {
            ServerError::InternalServerError(msg) => anyhow::anyhow!(msg),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AttestationRequest {
    nonce: Option<String>,
    payload: Option<String>,
}

#[debug_handler]
async fn get_maa_token(
    Query(request): Query<AttestationRequest>,
) -> Result<Json<ResponseStruct>, ServerError> {
    let _ = request.nonce.as_deref();
    let _ = request.payload.as_deref();
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(include_str!("../attestation.json").as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let json = std::fs::read_to_string(temp_file.path()).unwrap();
    let json = serde_json::from_str(&json).unwrap();
    Ok(Json(json))
}

#[debug_handler]
async fn get_att_root(
    Query(request): Query<AttestationRequest>,
) -> Result<Json<ResponseStruct>, ServerError> {
    let _ = request.nonce.as_deref();
    let _ = request.payload.as_deref();
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(include_str!("../attestation_att.json").as_bytes()).unwrap();
    temp_file.flush().unwrap();

    let json = std::fs::read_to_string(temp_file.path()).unwrap();
    let json = serde_json::from_str(&json).unwrap();
    Ok(Json(json))
}

pub fn app() -> Router {
    Router::new()
        .route("/", get(get_maa_token))
        .route("/attestation_att", get(get_att_root))
}

pub async fn serve(addr: SocketAddr) -> Result<(), ServerError> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Fake Attestation Server listening on {}", addr);
    axum::serve(listener, app())
        .await
        .map_err(|error| ServerError::InternalServerError(error.to_string()))
}