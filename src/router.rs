use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeader;

use crate::db::{self, Game, GameSummary};

/// Directory of the data directory whose files are served, at [`ASSETS_URL`].
pub const ASSETS_DIR: &str = "assets";
/// URL path the files in [`ASSETS_DIR`] are served at.
pub const ASSETS_URL: &str = "/data/assets";

const GAME_PAGE: &str = "web/game.html";
const NOT_FOUND_PAGE: &str = "web/404.html";

/// Tells browsers to ask whether a file of the pages changed before reusing their copy of it,
/// so that a page never runs against the script or the styles of an older version.
const REVALIDATE: HeaderValue = HeaderValue::from_static("no-cache");

/// Build the routes, reading the database and the assets from `data_dir`.
pub fn router(data_dir: &std::path::Path) -> Router {
    let pages = ServeDir::new("web").not_found_service(ServeFile::new(NOT_FOUND_PAGE));

    Router::new()
        .route("/api/games", get(list_games))
        .route("/api/game/{game_id}", get(get_game))
        .route("/api/test/{test_id}", get(get_test))
        .route("/game/{game_id}", get(get_game_page))
        .nest_service(ASSETS_URL, ServeDir::new(data_dir.join(ASSETS_DIR)))
        .fallback_service(SetResponseHeader::overriding(
            pages,
            header::CACHE_CONTROL,
            REVALIDATE,
        ))
        .with_state(data_dir.to_path_buf())
}

enum Error {
    NotFound,
    Database(rusqlite::Error),
    Page(std::io::Error),
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Page(error)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Database(error) => {
                eprintln!("Database access failed: {error}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            Self::Page(error) => {
                eprintln!("Reading a page failed: {error}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Respond with the page of a game, or with the page for what does not exist if no game has
/// the id in the address.
async fn get_game_page(
    State(data_dir): State<PathBuf>,
    Path(game_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let (status, page) = if db::game_exists(&data_dir, &game_id)? {
        (StatusCode::OK, GAME_PAGE)
    } else {
        (StatusCode::NOT_FOUND, NOT_FOUND_PAGE)
    };

    let page = Html(std::fs::read_to_string(page)?);
    Ok((status, [(header::CACHE_CONTROL, REVALIDATE)], page))
}

async fn list_games(State(data_dir): State<PathBuf>) -> Result<Json<Vec<GameSummary>>, Error> {
    Ok(Json(db::select_games(&data_dir)?))
}

async fn get_game(
    State(data_dir): State<PathBuf>,
    Path(game_id): Path<String>,
) -> Result<Json<Game>, Error> {
    let game = db::select_game(&data_dir, &game_id)?.ok_or(Error::NotFound)?;
    Ok(Json(game))
}

/// Respond with the stored JSON document of a test, untouched.
async fn get_test(
    State(data_dir): State<PathBuf>,
    Path(test_id): Path<i64>,
) -> Result<impl IntoResponse, Error> {
    let test = db::select_test_document(&data_dir, test_id)?.ok_or(Error::NotFound)?;
    Ok(([(header::CONTENT_TYPE, "application/json")], test))
}
