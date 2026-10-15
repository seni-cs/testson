use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use axum::{Json, Router};
use tower_http::services::ServeFile;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::db::{self, Game, GameSummary};

/// Directory of the data directory whose files are served, at [`ASSETS_URL`].
pub const ASSETS_DIR: &str = "assets";
/// URL path the files in [`ASSETS_DIR`] are served at.
pub const ASSETS_URL: &str = "/data/assets";

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JAVASCRIPT: &str = "text/javascript; charset=utf-8";

const INDEX_PAGE: &str = include_str!("../web/index.html");
const GAME_PAGE: &str = include_str!("../web/game.html");
const NOT_FOUND_PAGE: &str = include_str!("../web/404.html");

/// The files in `web/`, compiled into the binary so that the server finds them wherever it is
/// started from: the URL path each is served at, its content type and its contents.
const PAGES: &[(&str, &str, &str)] = &[
    ("/", HTML, INDEX_PAGE),
    ("/index.html", HTML, INDEX_PAGE),
    ("/game.html", HTML, GAME_PAGE),
    ("/404.html", HTML, NOT_FOUND_PAGE),
    ("/common.css", CSS, include_str!("../web/common.css")),
    ("/common.js", JAVASCRIPT, include_str!("../web/common.js")),
    ("/index.css", CSS, include_str!("../web/index.css")),
    ("/index.js", JAVASCRIPT, include_str!("../web/index.js")),
    ("/game.css", CSS, include_str!("../web/game.css")),
    ("/game.js", JAVASCRIPT, include_str!("../web/game.js")),
];

/// Tells browsers to ask whether a file of the pages changed before reusing their copy of it,
/// so that a page never runs against the script or the styles of an older version.
const REVALIDATE: HeaderValue = HeaderValue::from_static("no-cache");

/// Tells browsers to take every response for the type it says it is, so that no file is read as
/// a page or a script because of what it holds.
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");

/// Lets the pages run their own scripts and styles and call the API, show the images of the games
/// from any `https` host, such as the catalogue's, and play the assets. Nothing else is loaded,
/// and no other site may frame them. `form-action` is left out: the pages submit no form, and
/// some browsers apply it to the `method="dialog"` form that closes the viewer.
const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                           img-src 'self' https:; media-src 'self'; connect-src 'self'; \
                           object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Keeps an asset opened on its own from running anything or reaching the dashboard: the agent
/// writes the assets, so they are not trusted as pages. A browser shows an image or a video
/// opened on its own in a document of its own, which still needs to load that one file from its
/// own origin and to lay it out with the inline styles the browser adds, so the sandbox keeps the
/// origin and blocks the scripts.
const ASSET_POLICY: &str = "default-src 'none'; img-src 'self'; media-src 'self'; \
                            style-src 'unsafe-inline'; sandbox allow-same-origin";

/// The extensions of the files the assets are served from, with the type each is served as: the
/// video and the screenshots the dashboard shows. Any other file, such as a trace, which holds
/// the session, is not served.
const ASSET_TYPES: [(&str, &str); 6] = [
    ("gif", "image/gif"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("png", "image/png"),
    ("webm", "video/webm"),
    ("webp", "image/webp"),
];

/// Whether the assets serve files with `extension`, in any case: see [`ASSET_TYPES`].
pub fn serves_extension(extension: &str) -> bool {
    ASSET_TYPES
        .iter()
        .any(|(known, _)| extension.eq_ignore_ascii_case(known))
}

/// Build the routes, reading the database and the assets from `data_dir`.
pub fn router(data_dir: &std::path::Path) -> Router {
    let pages = PAGES
        .iter()
        .fold(Router::new(), |router, &(path, content_type, contents)| {
            router.route(path, page(content_type, contents))
        });

    pages
        .route("/api/games", get(list_games))
        .route("/api/game/{game_id}", get(get_game))
        .route("/api/test/{test_id}", get(get_test))
        .route("/game/{game_id}", get(get_game_page))
        .route(&format!("{ASSETS_URL}/{{*path}}"), get(get_asset))
        .fallback(not_found)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(PAGE_POLICY),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            NOSNIFF,
        ))
        .with_state(data_dir.to_path_buf())
}

/// Respond to a GET with one of the files in `web/`. Its entity tag is worked out once, here.
fn page(content_type: &'static str, contents: &'static str) -> MethodRouter<PathBuf> {
    let tag = entity_tag(contents);
    get(move |request: HeaderMap| async move { embedded(&request, content_type, &tag, contents) })
}

/// The entity tag of a file of the pages: a hash of its contents, so that it changes when the
/// file does and stays the same from one start of the server to the next.
fn entity_tag(contents: &str) -> HeaderValue {
    // 64-bit FNV-1a, fixed by its definition rather than by the version of the standard library.
    let hash = contents
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    HeaderValue::try_from(format!("\"{hash:016x}\""))
        .expect("a quoted hex number is a header value")
}

/// Respond with a file of the pages, or with 304 Not Modified when the browser's copy of it,
/// named in `If-None-Match`, is still current.
fn embedded(
    request: &HeaderMap,
    content_type: &'static str,
    tag: &HeaderValue,
    contents: &'static str,
) -> Response {
    let validators = [
        (header::CACHE_CONTROL, REVALIDATE),
        (header::ETAG, tag.clone()),
    ];
    if is_current(request, tag) {
        return (StatusCode::NOT_MODIFIED, validators).into_response();
    }
    let content_type = [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))];
    (validators, content_type, contents).into_response()
}

/// Whether one of the entity tags in the `If-None-Match` headers of a request matches `tag`.
/// The comparison is the weak one, which ignores a `W/` prefix, as that header asks for.
fn is_current(request: &HeaderMap, tag: &HeaderValue) -> bool {
    let Ok(tag) = tag.to_str() else {
        return false;
    };
    request
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.trim_start_matches("W/") == tag)
}

/// Respond with the page for what does not exist.
async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        [(header::CACHE_CONTROL, REVALIDATE)],
        Html(NOT_FOUND_PAGE),
    )
}

enum Error {
    NotFound,
    Database(rusqlite::Error),
    Blocking(tokio::task::JoinError),
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::Blocking(error)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Database(error) => {
                esay!("Database access failed: {error}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            Self::Blocking(error) => {
                esay!("Reading the database stopped: {error}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Read the database on the threads Tokio keeps for blocking work, so that a slow read does not
/// hold up the threads that serve every other request.
async fn read<T, F>(data_dir: PathBuf, read: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&std::path::Path) -> rusqlite::Result<T> + Send + 'static,
{
    Ok(tokio::task::spawn_blocking(move || read(&data_dir)).await??)
}

/// Respond with the page of a game, or with the page for what does not exist if no game has
/// the id in the address.
async fn get_game_page(
    State(data_dir): State<PathBuf>,
    Path(game_id): Path<String>,
    request: HeaderMap,
) -> Result<Response, Error> {
    static TAG: LazyLock<HeaderValue> = LazyLock::new(|| entity_tag(GAME_PAGE));

    if read(data_dir, move |dir| db::game_exists(dir, &game_id)).await? {
        Ok(embedded(&request, HTML, &TAG, GAME_PAGE))
    } else {
        Ok(not_found().await.into_response())
    }
}

async fn list_games(State(data_dir): State<PathBuf>) -> Result<Json<Vec<GameSummary>>, Error> {
    Ok(Json(read(data_dir, db::select_games).await?))
}

async fn get_game(
    State(data_dir): State<PathBuf>,
    Path(game_id): Path<String>,
) -> Result<Json<Game>, Error> {
    let game = read(data_dir, move |dir| db::select_game(dir, &game_id)).await?;
    Ok(Json(game.ok_or(Error::NotFound)?))
}

/// Respond with the stored JSON document of a test, untouched.
async fn get_test(
    State(data_dir): State<PathBuf>,
    Path(test_id): Path<i64>,
) -> Result<impl IntoResponse, Error> {
    let test = read(data_dir, move |dir| db::select_test_document(dir, test_id))
        .await?
        .ok_or(Error::NotFound)?;
    Ok(([(header::CONTENT_TYPE, "application/json")], test))
}

/// Respond with a file of the assets, ranges included so that a video can be sought, or with Not
/// Found for anything that is not a regular file of a known type reached without a link.
async fn get_asset(
    State(data_dir): State<PathBuf>,
    Path(path): Path<String>,
    request: Request,
) -> Response {
    let found = asset_type(&path).and_then(|content_type| {
        let file = resolve_asset(&data_dir.join(ASSETS_DIR), &path)?;
        Some((file, content_type))
    });
    let mut response = match found {
        Some((file, content_type)) => serve_asset(file, content_type, request).await,
        None => StatusCode::NOT_FOUND.into_response(),
    };
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(ASSET_POLICY),
    );
    response
}

async fn serve_asset(file: PathBuf, content_type: &'static str, request: Request) -> Response {
    match ServeFile::new(file).try_call(request).await {
        Ok(mut response) => {
            // Only a response with the file says what type it is.
            if response.headers().contains_key(header::CONTENT_TYPE) {
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            }
            response.map(Body::new)
        }
        Err(error) => {
            esay!("Reading an asset failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The type an asset is served as, or `None` if `path` may not name one: it must be a relative
/// path of plain names, see [`is_plain_name`], that ends in one of [`ASSET_TYPES`].
fn asset_type(path: &str) -> Option<&'static str> {
    if !path.split('/').all(is_plain_name) {
        return None;
    }
    let (_, extension) = path.rsplit('/').next()?.rsplit_once('.')?;
    ASSET_TYPES
        .iter()
        .find(|(known, _)| extension.eq_ignore_ascii_case(known))
        .map(|&(_, content_type)| content_type)
}

/// Whether `name` names an entry of a directory and nothing else: it is not empty, `.` or `..`,
/// and has no separator, drive or stream (`name:stream` on Windows) and no control character.
/// Windows drops the dots and spaces a name ends with, so such a name is refused as well.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
}

/// The file `path`, already checked by [`asset_type`], names under `root`, if it is a regular file
/// reached without following a link: no component may be a symbolic link, a junction or another
/// reparse point, the file may have no other name, and it must still be inside `root` once every
/// link is resolved.
fn resolve_asset(root: &std::path::Path, path: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let mut file = root.clone();
    let mut metadata = None;
    for name in path.split('/') {
        file.push(name);
        let found = fs::symlink_metadata(&file).ok()?;
        if is_link(&found) {
            return None;
        }
        metadata = Some(found);
    }
    let metadata = metadata?;
    if !metadata.is_file() || has_other_names(&metadata) {
        return None;
    }
    inside(&root, &file)
}

/// `file` with every link resolved, if it is inside `root`, which must already be resolved.
fn inside(root: &std::path::Path, file: &std::path::Path) -> Option<PathBuf> {
    let file = file.canonicalize().ok()?;
    file.starts_with(root).then_some(file)
}

/// Whether `metadata` is of a symbolic link, a junction, or any other reparse point on Windows:
/// any of them could lead out of the assets.
fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || is_reparse_point(metadata)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

/// Whether the file of `metadata` has other names, hard links that may be anywhere on its volume,
/// such as the login. Only Unix tells from the standard library, so on Windows a hard link is not
/// caught.
#[cfg(unix)]
fn has_other_names(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn has_other_names(_: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::fs;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    /// A data directory with one run in its assets and a session file beside them.
    fn data_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(ASSETS_DIR).join("run");
        fs::create_dir_all(run.join("screenshots")).unwrap();
        fs::write(run.join("screenshots").join("game.png"), b"\x89PNG").unwrap();
        fs::write(run.join("session.webm"), b"0123456789").unwrap();
        fs::write(
            run.join("page.html"),
            "<script>fetch('/api/games')</script>",
        )
        .unwrap();
        fs::write(run.join("page.svg"), "<svg/>").unwrap();
        fs::write(run.join("trace.zip"), "PK session").unwrap();
        fs::write(dir.path().join("session-block.txt"), "secret").unwrap();
        dir
    }

    async fn get(data_dir: &std::path::Path, uri: &str, range: Option<&str>) -> Response {
        let mut request = Request::get(uri);
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        router(data_dir)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn header_of<'a>(response: &'a Response, name: &header::HeaderName) -> &'a str {
        response.headers()[name].to_str().unwrap()
    }

    #[test]
    fn only_relative_paths_of_plain_names_to_known_files_name_an_asset() {
        for (path, expected) in [
            ("run/session.webm", Some("video/webm")),
            ("run/trace.zip", None),
            ("run/trace.trace", None),
            ("run/trace.network", None),
            ("run/screenshots/Ronda 1.PNG", Some("image/png")),
            ("run/screenshots/a.b.jpeg", Some("image/jpeg")),
            ("run/page.html", None),
            ("run/image.svg", None),
            ("run/notes.txt", None),
            ("run/screenshots", None),
            ("run.png/file", None),
            ("", None),
            ("/etc/passwd.png", None),
            ("C:/secret.png", None),
            ("run/../../session.png", None),
            ("run/./session.webm", None),
            ("run//session.webm", None),
            ("run/session.webm/", None),
            ("run\\..\\x.png", None),
            ("run/x.png.", None),
            ("run/x.png ", None),
            ("run/x:y.png", None),
            ("run/x\0.png", None),
            ("run/.../x.png", None),
        ] {
            assert_eq!(asset_type(path), expected, "{path:?}");
        }
    }

    #[test]
    fn a_file_resolved_out_of_the_assets_is_refused() {
        let dir = data_dir();
        let root = dir.path().join(ASSETS_DIR).canonicalize().unwrap();
        let file = root.join("run").join("session.webm");
        assert_eq!(inside(&root, &file), Some(file.canonicalize().unwrap()));

        let outside = dir.path().join("leak.png");
        fs::write(&outside, "secret").unwrap();
        let escaping = root.join("run").join("..").join("..").join("leak.png");
        assert!(escaping.is_file());
        assert_eq!(inside(&root, &escaping), None);
    }

    #[test]
    fn only_regular_files_resolve() {
        let dir = data_dir();
        let root = dir.path().join(ASSETS_DIR);
        assert!(resolve_asset(&root, "run/screenshots/game.png").is_some());
        assert_eq!(resolve_asset(&root, "run/screenshots"), None);
        assert_eq!(resolve_asset(&root, "run/missing.png"), None);
    }

    #[tokio::test]
    async fn assets_are_served_with_a_fixed_type_and_a_sandbox() {
        let dir = data_dir();
        let response = get(dir.path(), "/data/assets/run/screenshots/game.png", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, &header::CONTENT_TYPE), "image/png");
        assert_eq!(
            header_of(&response, &header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert_eq!(
            header_of(&response, &header::CONTENT_SECURITY_POLICY),
            ASSET_POLICY
        );
    }

    #[tokio::test]
    async fn a_video_can_be_sought() {
        let dir = data_dir();
        let response = get(
            dir.path(),
            "/data/assets/run/session.webm",
            Some("bytes=2-5"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header_of(&response, &header::CONTENT_TYPE), "video/webm");
        assert_eq!(header_of(&response, &header::CONTENT_RANGE), "bytes 2-5/10");
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], b"2345");
    }

    #[tokio::test]
    async fn active_content_and_paths_out_of_the_assets_are_not_served() {
        let dir = data_dir();
        for uri in [
            "/data/assets/run/page.html",
            "/data/assets/run/page.svg",
            "/data/assets/run/trace.zip",
            "/data/assets/run",
            "/data/assets/run/",
            "/data/assets/run/screenshots",
            "/data/assets/../session-block.txt",
            "/data/assets/run/%2e%2e/%2e%2e/session-block.txt",
            "/data/assets/run/..%5C..%5Csession-block.txt",
            "/data/assets/run//session.webm",
        ] {
            let response = get(dir.path(), uri, None).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            assert!(!response.headers().contains_key(header::LOCATION), "{uri}");
            assert_eq!(
                header_of(&response, &header::X_CONTENT_TYPE_OPTIONS),
                "nosniff",
                "{uri}"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_in_the_assets_is_not_followed() {
        let dir = data_dir();
        let outside = dir.path().join("runs");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("leak.png"), "secret").unwrap();
        let junction = dir.path().join(ASSETS_DIR).join("junction");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");
        assert!(junction.join("leak.png").is_file());

        let response = get(dir.path(), "/data/assets/junction/leak.png", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_or_a_hard_link_in_the_assets_is_not_followed() {
        let dir = data_dir();
        let assets = dir.path().join(ASSETS_DIR);
        std::os::unix::fs::symlink(
            dir.path().join("session-block.txt"),
            assets.join("leak.png"),
        )
        .unwrap();
        std::os::unix::fs::symlink(dir.path(), assets.join("data")).unwrap();
        fs::hard_link(
            dir.path().join("session-block.txt"),
            assets.join("hard.png"),
        )
        .unwrap();

        for uri in [
            "/data/assets/leak.png",
            "/data/assets/data/assets/run/session.webm",
            "/data/assets/hard.png",
        ] {
            let response = get(dir.path(), uri, None).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn pages_get_a_policy_that_still_lets_them_work() {
        let dir = data_dir();
        let response = get(dir.path(), "/", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, &header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert_eq!(
            header_of(&response, &header::CONTENT_SECURITY_POLICY),
            PAGE_POLICY
        );
    }
}

/// The pages compiled into the binary, and the routes that read the database.
#[cfg(test)]
mod page_tests {
    use super::*;
    use crate::catalog;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn data_dir_with_a_game() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        db::setup(dir.path()).unwrap();
        let game = catalog::Game {
            id: "nyxAviator".to_owned(),
            name: "Aviator".to_owned(),
            provider: String::new(),
            primary_type: String::new(),
            is_available_for_fun: true,
            image_url: String::new(),
        };
        db::upsert_game(dir.path(), &game).unwrap();
        dir
    }

    /// Request `path` and return the status, the content type and the body.
    async fn get(data_dir: &std::path::Path, path: &str) -> (StatusCode, String, String) {
        let request = Request::get(path).body(Body::empty()).unwrap();
        let response = router(data_dir).oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map_or_else(String::new, |value| value.to_str().unwrap().to_owned());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    /// Request `path` with an `If-None-Match` header naming `tag`, and return the status, the
    /// entity tag and the length of the body.
    async fn revalidate(
        data_dir: &std::path::Path,
        path: &str,
        tag: &str,
    ) -> (StatusCode, Option<HeaderValue>, usize) {
        let request = Request::get(path)
            .header(header::IF_NONE_MATCH, tag)
            .body(Body::empty())
            .unwrap();
        let response = router(data_dir).oneshot(request).await.unwrap();
        let status = response.status();
        let tag = response.headers().get(header::ETAG).cloned();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, tag, body.len())
    }

    // The expected contents below are the ones `include_str!` compiled in, and nothing in the
    // router reads `web/` at run time, so the pages are served whatever directory the server
    // starts in.

    #[tokio::test]
    async fn the_pages_are_served_from_the_binary() {
        let dir = data_dir_with_a_game();

        for &(path, content_type, contents) in PAGES {
            let response = get(dir.path(), path).await;
            assert_eq!(
                response,
                (StatusCode::OK, content_type.to_owned(), contents.to_owned()),
                "{path}"
            );
        }
        assert_eq!(
            get(dir.path(), "/index.js").await.1,
            "text/javascript; charset=utf-8"
        );
    }

    #[test]
    fn every_file_in_web_is_compiled_in() {
        let web = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
        for entry in std::fs::read_dir(web).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let path = format!("/{name}");
            assert!(
                PAGES.iter().any(|&(served, _, _)| served == path),
                "web/{name} is not in PAGES"
            );
        }
    }

    #[tokio::test]
    async fn the_page_of_a_game_is_served_from_the_binary() {
        let dir = data_dir_with_a_game();

        let (status, content_type, body) = get(dir.path(), "/game/nyxAviator").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, HTML);
        assert_eq!(body, GAME_PAGE);

        let (status, content_type, body) = get(dir.path(), "/game/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(content_type, HTML);
        assert_eq!(body, NOT_FOUND_PAGE);
    }

    #[tokio::test]
    async fn an_unknown_path_gets_the_page_for_what_does_not_exist() {
        let dir = data_dir_with_a_game();

        for path in ["/nope", "/web/index.html", "/index.js/x"] {
            let (status, content_type, body) = get(dir.path(), path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(content_type, HTML, "{path}");
            assert_eq!(body, NOT_FOUND_PAGE, "{path}");
        }
    }

    #[test]
    fn the_entity_tag_follows_the_contents() {
        assert_eq!(entity_tag(""), "\"cbf29ce484222325\"");
        assert_eq!(entity_tag("a"), "\"af63dc4c8601ec8c\"");
        assert_ne!(entity_tag(INDEX_PAGE), entity_tag(GAME_PAGE));
    }

    #[tokio::test]
    async fn every_page_response_carries_the_security_headers() {
        let dir = data_dir_with_a_game();
        let tag = entity_tag(INDEX_PAGE);
        let requests = [
            ("/", None, StatusCode::OK),
            ("/", Some(tag.to_str().unwrap()), StatusCode::NOT_MODIFIED),
            ("/game/nyxAviator", None, StatusCode::OK),
            ("/game/nope", None, StatusCode::NOT_FOUND),
            ("/nope", None, StatusCode::NOT_FOUND),
            ("/api/games", None, StatusCode::OK),
        ];
        for (path, current, expected) in requests {
            let mut request = Request::get(path);
            if let Some(current) = current {
                request = request.header(header::IF_NONE_MATCH, current);
            }
            let response = router(dir.path())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
            let headers = response.headers();
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
            assert_eq!(
                headers[header::CONTENT_SECURITY_POLICY],
                PAGE_POLICY,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn a_current_copy_of_a_page_is_not_sent_again() {
        let dir = data_dir_with_a_game();

        for (path, contents) in [
            ("/", INDEX_PAGE),
            ("/game.js", include_str!("../web/game.js")),
            ("/game/nyxAviator", GAME_PAGE),
        ] {
            let tag = entity_tag(contents);
            let tag = tag.to_str().unwrap();

            let stale = revalidate(dir.path(), path, "\"0000000000000000\"").await;
            assert_eq!(stale.0, StatusCode::OK, "{path}");
            assert_eq!(stale.1.as_ref().map(|v| v.to_str().unwrap()), Some(tag));
            assert_eq!(stale.2, contents.len(), "{path}");

            for current in [tag.to_owned(), format!("W/{tag}"), format!("\"x\", {tag}")] {
                let fresh = revalidate(dir.path(), path, &current).await;
                assert_eq!(fresh.0, StatusCode::NOT_MODIFIED, "{path} {current}");
                assert_eq!(fresh.1.as_ref().map(|v| v.to_str().unwrap()), Some(tag));
                assert_eq!(fresh.2, 0, "{path} {current}");
            }
        }

        let tag = entity_tag(GAME_PAGE);
        let missing = revalidate(dir.path(), "/game/nope", tag.to_str().unwrap()).await;
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        assert_eq!(missing.1, None);
    }

    #[tokio::test]
    async fn every_page_and_the_404_page_must_be_revalidated() {
        let dir = data_dir_with_a_game();

        for path in ["/", "/common.css", "/game/nyxAviator", "/nope"] {
            let request = Request::get(path).body(Body::empty()).unwrap();
            let response = router(dir.path()).oneshot(request).await.unwrap();
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL),
                Some(&REVALIDATE),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn the_api_reads_the_database() {
        let dir = data_dir_with_a_game();

        let (status, content_type, body) = get(dir.path(), "/api/games").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("\"id\":\"nyxAviator\""), "{body}");

        assert_eq!(
            get(dir.path(), "/api/game/nyxAviator").await.0,
            StatusCode::OK
        );
        assert_eq!(
            get(dir.path(), "/api/game/nope").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(dir.path(), "/api/test/1").await.0,
            StatusCode::NOT_FOUND
        );
    }
}
