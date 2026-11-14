//! TMDB API v3 endpoints used by VideoTag.
//!
//! The API key is supplied by the caller on every call — this crate
//! never stores it. Every request carries the caller's
//! `TmdbLanguage`; TMDB falls back to the original language per
//! field when no translation exists.

use crate::{Candidate, Episode, IdPlatform, MediaType, Result, SourceError, TmdbLanguage};
use serde_json::Value;

const BASE: &str = "https://api.themoviedb.org/3";

/// Percent-encode a query parameter value (unreserved characters pass
/// through, everything else becomes %XX of its UTF-8 bytes).
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn build_url_lang(key: &str, path: &str, params: &[(&str, String)], language: &str) -> String {
    let mut url = format!(
        "{BASE}{path}?api_key={}&language={language}",
        url_encode(key)
    );
    for (name, value) in params {
        url.push_str(&format!("&{name}={}", url_encode(value)));
    }
    url
}

/// GET JSON from TMDB in `language`. A 404 yields `Ok(None)` so
/// detail lookups can treat "not there" as a normal outcome; any
/// other non-2xx status is an [`SourceError::Api`] carrying TMDB's
/// own status message.
fn fetch_json_lang(
    key: &str,
    path: &str,
    params: &[(&str, String)],
    language: &str,
) -> Result<Option<Value>> {
    let url = build_url_lang(key, path, params, language);
    let (status, body) = crate::http::get_status_text(&url)?;
    if status == 404 {
        return Ok(None);
    }
    let parsed: std::result::Result<Value, _> = serde_json::from_str(&body);
    if !(200..300).contains(&status) {
        let message = parsed
            .as_ref()
            .ok()
            .and_then(|v| v.get("status_message"))
            .and_then(Value::as_str)
            .unwrap_or("request failed")
            .to_string();
        return Err(SourceError::Api { status, message });
    }
    let value = parsed
        .map_err(|e| SourceError::Parse(format!("invalid JSON from TMDB: {e}")))?;
    Ok(Some(value))
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Year from a TMDB date field (`first_air_date` / `release_date`,
/// shaped `YYYY-MM-DD`, possibly empty or missing).
fn year_of(v: &Value, key: &str) -> Option<i32> {
    let date = v.get(key)?.as_str()?;
    let year: i32 = date.get(..4)?.parse().ok()?;
    if year > 0 {
        Some(year)
    } else {
        None
    }
}

/// Parse one TMDB result/detail object into a [`Candidate`]. TV
/// objects carry `name` / `original_name` / `first_air_date`; movie
/// objects carry `title` / `original_title` / `release_date`.
pub fn parse_candidate(v: &Value, media_type: MediaType) -> Option<Candidate> {
    let id = v.get("id")?.as_u64()?;
    let (title, original_title, year) = match media_type {
        MediaType::Tv => (
            str_field(v, "name"),
            str_field(v, "original_name"),
            year_of(v, "first_air_date"),
        ),
        MediaType::Movie => (
            str_field(v, "title"),
            str_field(v, "original_title"),
            year_of(v, "release_date"),
        ),
    };
    Some(Candidate {
        id,
        source: IdPlatform::Tmdb,
        media_type,
        title,
        original_title,
        year,
        overview: str_field(v, "overview"),
        popularity: v
            .get("popularity")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        vote_average: v
            .get("vote_average")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        vote_count: v
            .get("vote_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        // Filled in by the detail lookups from a second, en-US fetch.
        english_title: String::new(),
    })
}

/// The title an en-US detail payload carries for this media type —
/// what `get_tv` / `get_movie` store into `Candidate::english_title`.
pub fn english_title_of(v: &Value, media_type: MediaType) -> String {
    parse_candidate(v, media_type)
        .map(|c| c.title)
        .unwrap_or_default()
}

/// Parse a `/search/*` response body into candidates.
pub fn parse_search_results(v: &Value, media_type: MediaType) -> Vec<Candidate> {
    v.get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| parse_candidate(item, media_type))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `/tv/{id}/season/{n}` response body into episodes, in the
/// order TMDB returns them (ascending episode number). Episodes with
/// no number are skipped; an empty `air_date` becomes `None`.
pub fn parse_episodes(v: &Value) -> Vec<Episode> {
    v.get("episodes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let number = item.get("episode_number")?.as_u64()? as u32;
                    let air_date = item
                        .get("air_date")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    Some(Episode {
                        number,
                        title: str_field(item, "name"),
                        air_date,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn search(
    key: &str,
    media_type: MediaType,
    query: &str,
    year: Option<i32>,
    lang: TmdbLanguage,
) -> Result<Vec<Candidate>> {
    let (path, year_param) = match media_type {
        MediaType::Tv => ("/search/tv", "first_air_date_year"),
        MediaType::Movie => ("/search/movie", "year"),
    };
    let mut params = vec![("query", query.to_string()), ("page", "1".to_string())];
    if let Some(y) = year {
        params.push((year_param, y.to_string()));
    }
    match fetch_json_lang(key, path, &params, lang.query_value())? {
        Some(v) => Ok(parse_search_results(&v, media_type)),
        None => Ok(Vec::new()),
    }
}

/// Search TV shows by title, optionally narrowed by first-air year.
pub fn search_tv(
    key: &str,
    query: &str,
    year: Option<i32>,
    lang: TmdbLanguage,
) -> Result<Vec<Candidate>> {
    search(key, MediaType::Tv, query, year, lang)
}

/// Search movies by title, optionally narrowed by release year.
pub fn search_movie(
    key: &str,
    query: &str,
    year: Option<i32>,
    lang: TmdbLanguage,
) -> Result<Vec<Candidate>> {
    search(key, MediaType::Movie, query, year, lang)
}

fn get_by_id(
    key: &str,
    media_type: MediaType,
    id: u64,
    lang: TmdbLanguage,
) -> Result<Option<Candidate>> {
    let path = match media_type {
        MediaType::Tv => format!("/tv/{id}"),
        MediaType::Movie => format!("/movie/{id}"),
    };
    let Some(v) = fetch_json_lang(key, &path, &[], lang.query_value())? else {
        return Ok(None);
    };
    let Some(mut cand) = parse_candidate(&v, media_type) else {
        return Ok(None);
    };
    // The library folder format wants the en-US title next to the
    // localized one. An en-US lookup already holds it; any other
    // language fetches the same detail once more in en-US.
    // Best effort: any failure just leaves `english_title` empty.
    if lang == TmdbLanguage::EnUs {
        cand.english_title = cand.title.clone();
    } else if let Ok(Some(en)) = fetch_json_lang(key, &path, &[], "en-US") {
        cand.english_title = english_title_of(&en, media_type);
    }
    Ok(Some(cand))
}

/// Fetch one TV show by TMDB id. `Ok(None)` means TMDB has no such id
/// (404) — a normal outcome, not an error.
pub fn get_tv(key: &str, id: u64, lang: TmdbLanguage) -> Result<Option<Candidate>> {
    get_by_id(key, MediaType::Tv, id, lang)
}

/// Fetch one movie by TMDB id. `Ok(None)` means TMDB has no such id.
pub fn get_movie(key: &str, id: u64, lang: TmdbLanguage) -> Result<Option<Candidate>> {
    get_by_id(key, MediaType::Movie, id, lang)
}

/// Fetch the episode list of one season of a TV show. A nonexistent
/// show or season yields an empty list.
pub fn get_episodes(
    key: &str,
    tv_id: u64,
    season: u32,
    lang: TmdbLanguage,
) -> Result<Vec<Episode>> {
    let path = format!("/tv/{tv_id}/season/{season}");
    match fetch_json_lang(key, &path, &[], lang.query_value())? {
        Some(v) => Ok(parse_episodes(&v)),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values() {
        assert_eq!(url_encode("红楼梦"), "%E7%BA%A2%E6%A5%BC%E6%A2%A6");
        assert_eq!(url_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(url_encode("S01-E.01_x~"), "S01-E.01_x~");
    }

    #[test]
    fn builds_urls_with_key_language_and_year() {
        let url = build_url_lang(
            "KEY",
            "/search/tv",
            &[
                ("query", "红楼梦".to_string()),
                ("page", "1".to_string()),
                ("first_air_date_year", "2010".to_string()),
            ],
            TmdbLanguage::ZhCn.query_value(),
        );
        assert_eq!(
            url,
            "https://api.themoviedb.org/3/search/tv?api_key=KEY&language=zh-CN&query=%E7%BA%A2%E6%A5%BC%E6%A2%A6&page=1&first_air_date_year=2010"
        );
        // The English mode reaches the URL untouched.
        let url = build_url_lang(
            "KEY",
            "/tv/1396/season/1",
            &[],
            TmdbLanguage::EnUs.query_value(),
        );
        assert_eq!(
            url,
            "https://api.themoviedb.org/3/tv/1396/season/1?api_key=KEY&language=en-US"
        );
    }
}
