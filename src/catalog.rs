//! Public Betsson games catalogue, and the games of the provider that is tested.
//!
//! Listing reads the Betsson games API. It does not need a login.

use std::collections::HashSet;
use std::io::Read;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{Map, Value};

use anyhow::{Result, anyhow, bail};

/// Casino lobby. A test searches it for the game by name, and its embedded config identifies
/// the market, brand, and jurisdiction.
pub const DEFAULT_CASINO_URL: &str = "https://www.betsson.es/casino";

/// Public games catalog.
pub const GAMES_URL: &str = "https://www.betsson.es/api/v2/games";

/// The provider whose games are tested, as the catalogue names it.
pub const PROVIDER: &str = "PragmaticPlay";

/// Browser user agent sent when reading the public catalog.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const BODY_LIMIT: u64 = 32 * 1024 * 1024;
const ERROR_BODY_BYTES: usize = 300;

/// One game of the catalogue, as the `games` table stores it. The day it was added on is the day
/// the database first stored it, since the API has no release date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    /// Identifier the catalogue gives the game, such as `nyxAviator`.
    pub id: String,
    /// Display name, such as `Eye Of Horus`.
    pub name: String,
    /// Provider, such as `Blueprint`. Empty when the API omits it.
    pub provider: String,
    /// Primary game type, such as `Slots`. Empty when the API omits it.
    pub primary_type: String,
    /// Whether the catalog marks the game as available for fun play. Stored as 0 or 1.
    pub is_available_for_fun: bool,
    /// The 512×512 square tile, or the regular thumbnail. Empty when the API has neither.
    pub image_url: String,
}

/// Brand and market taken from the casino lobby HTML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageConfig {
    /// Market code, such as `es`.
    pub market: String,
    /// Brand UUID.
    pub brand: String,
    /// Country code, such as `ES`.
    pub country: String,
    /// Jurisdiction code sent to the games API.
    pub jurisdiction: String,
}

/// HTTP GET used to read the public site.
pub trait Fetcher {
    /// Fetch `url` with the given request headers.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails or the status is not successful.
    fn fetch(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>>;
}

/// Blocking HTTPS client.
pub struct Http;

impl Fetcher for Http {
    fn fetch(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut request = shared_agent().get(url);
        for &(name, value) in headers {
            request = request.header(name, value);
        }
        let mut response = request
            .call()
            .map_err(|error| anyhow!("GET {url} failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_prefix(response.body_mut(), ERROR_BODY_BYTES);
            bail!(
                "GET {url} returned HTTP {} ({})",
                status.as_u16(),
                body.trim()
            );
        }
        response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map_err(|error| anyhow!("GET {url} failed: {error}"))
    }
}

/// Reads brand, market, and jurisdiction from the casino lobby HTML document.
///
/// # Errors
///
/// Returns an error when any of the three values is missing.
pub fn parse_page_config(html: &str) -> Result<PageConfig> {
    let market = capture(market_pattern(), html, 1);
    let brand = capture(brand_pattern(), html, 1);
    let country = capture(jurisdiction_pattern(), html, 1);
    let jurisdiction = capture(jurisdiction_pattern(), html, 2);
    match (market, brand, country, jurisdiction) {
        (Some(market), Some(brand), Some(country), Some(jurisdiction)) => Ok(PageConfig {
            market: market.to_owned(),
            brand: brand.to_owned(),
            country: country.to_owned(),
            jurisdiction: jurisdiction.to_owned(),
        }),
        _ => bail!("could not read brand, market, or jurisdiction from {DEFAULT_CASINO_URL}"),
    }
}

/// Reads every game of a games API payload.
///
/// # Errors
///
/// Returns an error when the payload is not a JSON list.
fn parse_catalog(body: &str) -> Result<Vec<Game>> {
    let payload: Value = serde_json::from_str(body)
        .map_err(|error| anyhow!("games API did not return JSON ({error})"))?;
    let Some(list) = payload.as_array() else {
        bail!("games API did not return a list");
    };
    Ok(list
        .iter()
        .filter_map(|value| game_from_object(value.as_object()?))
        .collect())
}

/// Downloads the page config.
///
/// # Errors
///
/// Returns an error when the page cannot be read or parsed.
fn fetch_page_config(fetcher: &impl Fetcher) -> Result<PageConfig> {
    let bytes = fetcher.fetch(
        DEFAULT_CASINO_URL,
        &[("user-agent", USER_AGENT), ("accept", "text/html")],
    )?;
    let html = String::from_utf8_lossy(&bytes);
    parse_page_config(&html)
}

/// Downloads every game of the catalogue.
///
/// # Errors
///
/// Returns an error when the page or the games API cannot be read.
fn fetch_catalog(fetcher: &impl Fetcher) -> Result<Vec<Game>> {
    let config = fetch_page_config(fetcher)?;
    let headers = [
        ("user-agent", USER_AGENT),
        ("accept", "application/json"),
        ("brandid", config.brand.as_str()),
        ("marketcode", config.market.as_str()),
        ("x-obg-channel", "Web"),
        ("x-obg-device", "Desktop"),
        ("x-obg-country-code", config.country.as_str()),
        ("x-obg-jurisdiction", config.jurisdiction.as_str()),
        ("referer", DEFAULT_CASINO_URL),
    ];
    let bytes = fetcher.fetch(GAMES_URL, &headers)?;
    let body = String::from_utf8_lossy(&bytes);
    parse_catalog(&body)
}

/// Downloads the games of [`PROVIDER`], sorted by name.
///
/// # Errors
///
/// Returns an error when the page or the games API cannot be read, or the provider has no games.
pub fn fetch_provider_games(fetcher: &impl Fetcher) -> Result<Vec<Game>> {
    select_provider_games(fetch_catalog(fetcher)?)
}

/// Finds a game of the whole catalogue by its name, ignoring case but preferring a game spelt
/// exactly as given.
///
/// # Errors
///
/// Returns an error when the page or the games API cannot be read.
pub fn find_game(fetcher: &impl Fetcher, name: &str) -> Result<Option<Game>> {
    let name = name.trim();
    let games: Vec<Game> = fetch_catalog(fetcher)?
        .into_iter()
        .filter(|game| game.name.to_lowercase() == name.to_lowercase())
        .collect();
    let exact = games.iter().position(|game| game.name == name);
    Ok(games.into_iter().nth(exact.unwrap_or(0)))
}

fn select_provider_games(catalog: Vec<Game>) -> Result<Vec<Game>> {
    let mut games: Vec<Game> = catalog
        .into_iter()
        .filter(|game| game.provider == PROVIDER)
        .collect();
    if games.is_empty() {
        bail!("the catalogue has no games of {PROVIDER}");
    }
    games.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut seen = HashSet::new();
    games.retain(|game| seen.insert(game.id.clone()));
    Ok(games)
}

/// A game needs an identifier and a name. Whatever else is missing is left empty.
fn game_from_object(object: &Map<String, Value>) -> Option<Game> {
    let content = object.get("content").and_then(Value::as_object);
    Some(Game {
        id: trimmed_string(object, "id")?.to_owned(),
        name: trimmed_string(object, "name")?.to_owned(),
        provider: optional_string(object, "provider"),
        primary_type: optional_string(object, "primaryType"),
        is_available_for_fun: object
            .get("isAvailableForFun")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        image_url: content.map(image_url).unwrap_or_default(),
    })
}

fn trimmed_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    let text = object.get(key)?.as_str()?.trim();
    if text.is_empty() { None } else { Some(text) }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn image_url(content: &Map<String, Value>) -> String {
    ["thumbnailSquare", "thumbnail"]
        .iter()
        .filter_map(|key| content.get(*key)?.get("url")?.as_str())
        .map(str::trim)
        .find(|url| !url.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn capture<'a>(pattern: &Regex, text: &'a str, group: usize) -> Option<&'a str> {
    pattern.captures(text)?.get(group).map(|item| item.as_str())
}

fn market_pattern() -> &'static Regex {
    static PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""marketCode":"([^"]+)""#).expect("market pattern"));
    &PATTERN
}

fn brand_pattern() -> &'static Regex {
    static PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""brandId":"([0-9a-fA-F-]{36})""#).expect("brand pattern"));
    &PATTERN
}

fn jurisdiction_pattern() -> &'static Regex {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""countryCode":"([A-Z]{2})","jurisdiction":"([^"]+)""#)
            .expect("jurisdiction pattern")
    });
    &PATTERN
}

fn shared_agent() -> &'static ureq::Agent {
    static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
        let config: ureq::config::Config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .https_only(true)
            .build();
        config.into()
    });
    &AGENT
}

fn read_prefix(body: &mut ureq::Body, max: usize) -> String {
    let mut buffer = vec![0_u8; max];
    let Ok(count) = body.as_reader().read(&mut buffer) else {
        return String::new();
    };
    String::from_utf8_lossy(&buffer[..count]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = r#"{"marketCode":"es","brandId":"11111111-2222-3333-4444-555555555555","countryCode":"ES","jurisdiction":"DGOJ"}"#;

    #[test]
    fn reads_page_config() {
        let config = parse_page_config(HTML).unwrap();
        assert_eq!(config.market, "es");
        assert_eq!(config.brand, "11111111-2222-3333-4444-555555555555");
        assert_eq!(config.country, "ES");
        assert_eq!(config.jurisdiction, "DGOJ");
    }

    #[test]
    fn rejects_incomplete_html() {
        let error = parse_page_config("<html></html>").unwrap_err();
        assert!(error.to_string().contains(DEFAULT_CASINO_URL));
    }

    fn provider_games(body: &str) -> Result<Vec<Game>> {
        select_provider_games(parse_catalog(body)?)
    }

    #[test]
    fn selects_sorts_and_dedups_the_games_of_the_provider() {
        let body = r#"[
            {"id":"b","name":"Beta","provider":"PragmaticPlay","primaryType":"Slots","isAvailableForFun":false,"content":{"thumbnail":{"url":"https://example.test/b.png"}}},
            {"id":"a","name":"Alpha","provider":"PragmaticPlay","primaryType":"Slots","isAvailableForFun":true,"content":{"thumbnailSquare":{"url":" https://example.test/a.png "},"thumbnail":{"url":"https://example.test/no.png"}}},
            {"id":"c","name":"Skip","provider":"Other"},
            {"id":"a","name":"Alpha again","provider":"PragmaticPlay"},
            {"name":"No id","provider":"PragmaticPlay"},
            "not-an-object",
            {"id":"d","name":"  ","provider":"PragmaticPlay"}
        ]"#;
        assert_eq!(
            provider_games(body).unwrap(),
            [
                Game {
                    id: "a".into(),
                    name: "Alpha".into(),
                    provider: "PragmaticPlay".into(),
                    primary_type: "Slots".into(),
                    is_available_for_fun: true,
                    image_url: "https://example.test/a.png".into(),
                },
                Game {
                    id: "b".into(),
                    name: "Beta".into(),
                    provider: "PragmaticPlay".into(),
                    primary_type: "Slots".into(),
                    is_available_for_fun: false,
                    image_url: "https://example.test/b.png".into(),
                },
            ]
        );
    }

    #[test]
    fn missing_optional_fields_become_empty_strings() {
        let game = &parse_catalog(r#"[{"id":"a","name":"Alpha"}]"#).unwrap()[0];
        assert_eq!(game.provider, "");
        assert_eq!(game.primary_type, "");
        assert_eq!(game.image_url, "");
        assert!(!game.is_available_for_fun);
    }

    #[test]
    fn rejects_a_provider_without_games_and_a_non_list() {
        let empty = provider_games(r#"[{"id":"c","name":"Skip","provider":"Other"}]"#).unwrap_err();
        assert!(empty.to_string().contains(PROVIDER));
        let object = provider_games(r#"{"games":[]}"#).unwrap_err();
        assert!(object.to_string().contains("list"));
        let broken = provider_games("not-json").unwrap_err();
        assert!(broken.to_string().contains("JSON"));
    }

    struct MapFetcher {
        html: &'static str,
        games: &'static str,
    }

    impl Fetcher for MapFetcher {
        fn fetch(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
            assert!(headers.iter().any(|(name, _)| *name == "user-agent"));
            if url == DEFAULT_CASINO_URL {
                return Ok(self.html.as_bytes().to_vec());
            }
            if url == GAMES_URL {
                assert!(headers.iter().any(|(name, value)| *name == "brandid"
                    && *value == "11111111-2222-3333-4444-555555555555"));
                return Ok(self.games.as_bytes().to_vec());
            }
            Err(anyhow!("unexpected url {url}"))
        }
    }

    #[test]
    fn fetch_provider_games_uses_page_config_headers() {
        let fetcher = MapFetcher {
            html: HTML,
            games: r#"[{"id":"alpha","name":"Alpha","provider":"PragmaticPlay"}]"#,
        };
        let games = fetch_provider_games(&fetcher).unwrap();
        assert_eq!(games[0].id, "alpha");
    }

    #[test]
    fn finds_any_game_of_the_catalogue_by_name() {
        let fetcher = MapFetcher {
            html: HTML,
            games: r#"[{"id":"old","name":"Los Piratas Golfetes","provider":"Other"},
                        {"id":"new","name":"LOS PIRATAS GOLFETES","provider":"Other"}]"#,
        };
        let found = |name| find_game(&fetcher, name).unwrap().unwrap().id;
        assert_eq!(found(" los piratas golfetes "), "old");
        assert_eq!(found("LOS PIRATAS GOLFETES"), "new");
        assert!(find_game(&fetcher, "Missing").unwrap().is_none());
    }
}
