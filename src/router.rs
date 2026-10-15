use std::path::PathBuf;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tower_http::services::ServeDir;

use crate::db::{self, Game, GameSummary, StatsTest};

/// Directory of the data directory whose files are served, at [`ASSETS_URL`].
pub const ASSETS_DIR: &str = "assets";
/// URL path the files in [`ASSETS_DIR`] are served at.
pub const ASSETS_URL: &str = "/data/assets";

/// The only files of the assets that are served: the recordings and the screenshots. Anything
/// else there, such as the trace an older version saved, which records the login, is not.
const ASSET_EXTENSIONS: &[&str] = &["webm", "png", "jpg", "jpeg", "webp", "gif"];

/// The files of `web/`, compiled in so the server works from any directory: the path each is
/// served at, its type and its contents.
const PAGES: &[(&str, &str, &str)] = &[
    ("/", HTML, INDEX_PAGE),
    ("/index.html", HTML, INDEX_PAGE),
    ("/game.html", HTML, GAME_PAGE),
    ("/stats", HTML, STATS_PAGE),
    ("/404.html", HTML, NOT_FOUND_PAGE),
    ("/common.css", CSS, include_str!("../web/common.css")),
    ("/common.js", JS, include_str!("../web/common.js")),
    ("/theme.js", JS, include_str!("../web/theme.js")),
    ("/index.css", CSS, include_str!("../web/index.css")),
    ("/index.js", JS, include_str!("../web/index.js")),
    ("/game.css", CSS, include_str!("../web/game.css")),
    ("/game.js", JS, include_str!("../web/game.js")),
    ("/stats.html", HTML, STATS_PAGE),
    ("/stats.css", CSS, include_str!("../web/stats.css")),
    ("/stats.js", JS, include_str!("../web/stats.js")),
];

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const INDEX_PAGE: &str = include_str!("../web/index.html");
const GAME_PAGE: &str = include_str!("../web/game.html");
const STATS_PAGE: &str = include_str!("../web/stats.html");
const NOT_FOUND_PAGE: &str = include_str!("../web/404.html");

/// Tells browsers to ask whether a file of the pages changed before reusing their copy of it,
/// so that a page never runs against the script or the styles of an older version.
const REVALIDATE: HeaderValue = HeaderValue::from_static("no-cache");

/// Build the routes, reading the database and the assets from `data_dir`.
pub fn router(data_dir: &std::path::Path) -> Router {
    let assets = Router::new()
        .fallback_service(ServeDir::new(data_dir.join(ASSETS_DIR)))
        .layer(middleware::from_fn(only_media));

    Router::new()
        .route("/api/games", get(list_games))
        .route("/api/game/{game_id}", get(get_game))
        .route("/api/test/{test_id}", get(get_test))
        .route("/api/stats", get(get_stats))
        .route("/game/{game_id}", get(get_game_page))
        .nest_service(ASSETS_URL, assets)
        .fallback(get_page)
        .with_state(data_dir.to_path_buf())
}

/// Lets through only the requests for a file with one of [`ASSET_EXTENSIONS`].
async fn only_media(request: Request, next: Next) -> Response {
    let media = std::path::Path::new(request.uri().path())
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| ASSET_EXTENSIONS.contains(&extension.to_lowercase().as_str()));
    if media {
        next.run(request).await
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Respond with a file of the pages, or with the page for what does not exist.
async fn get_page(uri: Uri) -> Response {
    match PAGES.iter().find(|(path, _, _)| *path == uri.path()) {
        Some((_, content_type, body)) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
                (header::CACHE_CONTROL, REVALIDATE),
            ],
            *body,
        )
            .into_response(),
        None => not_found_page(),
    }
}

fn not_found_page() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CACHE_CONTROL, REVALIDATE)],
        Html(NOT_FOUND_PAGE),
    )
        .into_response()
}

enum Error {
    NotFound,
    Database(rusqlite::Error),
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
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
        }
    }
}

/// Respond with the page of a game, or with the page for what does not exist if no game has
/// the id in the address.
async fn get_game_page(
    State(data_dir): State<PathBuf>,
    Path(game_id): Path<String>,
) -> Result<Response, Error> {
    if !db::game_exists(&data_dir, &game_id)? {
        return Ok(not_found_page());
    }
    Ok(([(header::CACHE_CONTROL, REVALIDATE)], Html(GAME_PAGE)).into_response())
}

async fn list_games(State(data_dir): State<PathBuf>) -> Result<Json<Vec<GameSummary>>, Error> {
    Ok(Json(db::select_games(&data_dir)?))
}

/// How many of the latest tests of each game the stats page shows.
const STATS_TESTS: u32 = 30;

async fn get_stats(State(data_dir): State<PathBuf>) -> Result<Json<Vec<StatsTest>>, Error> {
    Ok(Json(db::select_latest_tests(&data_dir, STATS_TESTS)?))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_of_the_pages_is_compiled_in() {
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/web")).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(
                PAGES.iter().any(|(path, _, _)| path[1..] == name),
                "web/{name} is missing from PAGES"
            );
        }
    }
}
