//! TheTVDB API v4 endpoints used by VideoTag.
//!
//! TVDB needs a login round-trip: the caller's API key (plus an
//! optional subscriber PIN) is exchanged for a bearer token, which
//! is cached in memory, keyed to the credentials that produced
//! it — a key change logs in afresh, and a 401 earns one
//! re-login + retry. The key itself is supplied by the caller on
//! every call and never stored here.
//!
//! TVDB has no trustworthy user rating, so every record leaves
//! the vote at 0/0 and the library format prints its `[0.0]`
//! placeholder.

use crate::http::{self, Request};
use crate::{Candidate, Episode, IdPlatform, MediaType, Result, SourceError, TmdbLanguage};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;

const BASE: &str = "https://api4.thetvdb.com/v4";

/// TheTVDB API credentials from the app settings. `pin` is only
/// needed by subscriber (user-supported) keys and may be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TvdbCredentials {
    pub api_key: String,
    pub pin: String,
}

/// Percent-encode a query parameter value (same rules as tmdb.rs).
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

// ---------------------------------------------------------------------------
// login / token cache

/// (api_key, pin, token) of the last successful login.
static TOKEN_CACHE: Mutex<Option<(String, String, String)>> = Mutex::new(None);

/// Pull the bearer token out of a `/login` response body.
pub fn parse_login_token(v: &Value) -> Option<String> {
    let token = v.get("data")?.get("token")?.as_str()?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn login(creds: &TvdbCredentials) -> Result<String> {
    // Keys pasted from elsewhere can carry a trailing newline,
    // which the API rejects as "apikey invalid" — trim here too,
    // whatever the caller passed in.
    let mut body = serde_json::json!({ "apikey": creds.api_key.trim() });
    if !creds.pin.trim().is_empty() {
        body["pin"] = Value::String(creds.pin.trim().to_string());
    }
    let (status, text) = http::send(&Request::post_json(
        &format!("{BASE}/login"),
        &body.to_string(),
    ))?;
    let parsed: std::result::Result<Value, _> = serde_json::from_str(&text);
    if !(200..300).contains(&status) {
        let message = parsed
            .as_ref()
            .ok()
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("login failed")
            .to_string();
        return Err(SourceError::Api { status, message });
    }
    let value = parsed
        .map_err(|e| SourceError::Parse(format!("invalid JSON from TVDB: {e}")))?;
    parse_login_token(&value)
        .ok_or_else(|| SourceError::Parse("TVDB login response has no token".to_string()))
}

fn bearer(creds: &TvdbCredentials, force_fresh: bool) -> Result<String> {
    if !force_fresh {
        if let Ok(guard) = TOKEN_CACHE.lock() {
            if let Some((key, pin, token)) = guard.as_ref() {
                if key == &creds.api_key && pin == &creds.pin {
                    return Ok(token.clone());
                }
            }
        }
    }
    let token = login(creds)?;
    if let Ok(mut guard) = TOKEN_CACHE.lock() {
        *guard = Some((
            creds.api_key.clone(),
            creds.pin.clone(),
            token.clone(),
        ));
    }
    Ok(token)
}

/// GET JSON from TVDB with the bearer token. A 404 yields
/// `Ok(None)` (a normal outcome for id lookups); a 401 clears the
/// cached token, logs in again and retries exactly once.
fn authed_get(creds: &TvdbCredentials, url: &str) -> Result<Option<Value>> {
    for attempt in 0..2 {
        let token = bearer(creds, attempt > 0)?;
        let headers = [("Authorization", format!("Bearer {token}"))];
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, body) = http::send(&Request::get(url).with_headers(&header_refs))?;
        if status == 404 {
            return Ok(None);
        }
        if status == 401 && attempt == 0 {
            if let Ok(mut guard) = TOKEN_CACHE.lock() {
                *guard = None;
            }
            continue;
        }
        let parsed: std::result::Result<Value, _> = serde_json::from_str(&body);
        if !(200..300).contains(&status) {
            let message = parsed
                .as_ref()
                .ok()
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("request failed")
                .to_string();
            return Err(SourceError::Api { status, message });
        }
        let value = parsed
            .map_err(|e| SourceError::Parse(format!("invalid JSON from TVDB: {e}")))?;
        return Ok(Some(value));
    }
    Err(SourceError::Api {
        status: 401,
        message: "unauthorized after re-login".to_string(),
    })
}

// ---------------------------------------------------------------------------
// parsing (pure; fixture-tested)

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// An id that may arrive as a JSON number or as a digit string
/// (TVDB search results spell `tvdb_id` both ways).
fn id_field(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Year from a date-ish string (`1991-07-05`), when shaped like one.
fn year_of_str(date: &str) -> Option<i32> {
    let year: i32 = date.get(..4)?.parse().ok()?;
    if year > 0 {
        Some(year)
    } else {
        None
    }
}

fn year_of(v: &Value) -> Option<i32> {
    if let Some(y) = v.get("year").and_then(Value::as_u64) {
        return i32::try_from(y).ok().filter(|y| *y > 0);
    }
    for key in ["firstAired", "first_air_time"] {
        if let Some(date) = v.get(key).and_then(Value::as_str) {
            if let Some(y) = year_of_str(date) {
                return Some(y);
            }
        }
    }
    None
}

/// Overview text: a plain string, or a language map (search
/// results) where the first available translation wins.
fn overview_of(v: &Value) -> String {
    match v.get("overview") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => {
            for key in ["zho", "chi", "eng", "en"] {
                if let Some(s) = map.get(key).and_then(Value::as_str) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
            map.values()
                .filter_map(Value::as_str)
                .find(|s| !s.is_empty())
                .unwrap_or("")
                .to_string()
        }
        _ => String::new(),
    }
}

/// A translated name from a `nameTranslations` list, by language
/// code (`zho` for Chinese).
fn translated_name(v: &Value, language: &str) -> Option<String> {
    let items = v.get("nameTranslations")?.as_array()?;
    items.iter().find_map(|item| {
        if item.get("language").and_then(Value::as_str) == Some(language) {
            item.get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        } else {
            None
        }
    })
}

/// Parse one TVDB search result into a [`Candidate`]. Search
/// results carry a single (usually original-language) name; the
/// Chinese translation only arrives with the extended detail.
pub fn parse_search_candidate(v: &Value, media_type: MediaType) -> Option<Candidate> {
    let id = id_field(v, "tvdb_id").or_else(|| id_field(v, "id"))?;
    let name = str_field(v, "name");
    Some(Candidate {
        id,
        source: IdPlatform::Tvdb,
        media_type,
        title: name.clone(),
        original_title: name,
        year: year_of(v),
        overview: overview_of(v),
        popularity: v.get("score").and_then(Value::as_f64).unwrap_or(0.0),
        vote_average: 0.0,
        vote_count: 0,
        english_title: String::new(),
    })
}

/// Parse a `/search` response body into candidates.
pub fn parse_search_results(v: &Value, media_type: MediaType) -> Vec<Candidate> {
    v.get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| parse_search_candidate(item, media_type))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an extended detail record (`/series/{id}/extended`,
/// `/movies/{id}/extended`). With `chinese`, the title prefers
/// the `zho` name translation; the record's own `name` — usually
/// the English original — fills both the original and the
/// English slot (the folder format's `(English)` segment).
pub fn parse_extended(v: &Value, media_type: MediaType, chinese: bool) -> Option<Candidate> {
    let data = v.get("data")?;
    let id = id_field(data, "id")?;
    let name = str_field(data, "name");
    let title = if chinese {
        translated_name(data, "zho").unwrap_or_else(|| name.clone())
    } else {
        name.clone()
    };
    let overview = if chinese {
        data.get("overviewTranslations")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find_map(|item| {
                    if item.get("language").and_then(Value::as_str) == Some("zho") {
                        item.get("overview")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| overview_of(data))
    } else {
        overview_of(data)
    };
    Some(Candidate {
        id,
        source: IdPlatform::Tvdb,
        media_type,
        title,
        original_title: name.clone(),
        year: year_of(data),
        overview,
        popularity: data
            .get("score")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        vote_average: 0.0,
        vote_count: 0,
        english_title: name,
    })
}

/// Parse one page of an episodes listing: `data.episodes` with
/// season/episode numbers, plus the `links.next` page URL when
/// there is one. Episodes without numbers are skipped; an empty
/// `aired` becomes `None`.
pub fn parse_episodes_page(v: &Value) -> (Vec<(u32, Episode)>, Option<String>) {
    let episodes = v
        .get("data")
        .and_then(|d| d.get("episodes"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let season =
                        item.get("seasonNumber")?.as_u64()? as u32;
                    // The live API names this field "number";
                    // "episodeNumber" was a docs-based guess
                    // that silently dropped every episode.
                    let number = item
                        .get("number")
                        .or_else(|| item.get("episodeNumber"))?
                        .as_u64()? as u32;
                    if number == 0 {
                        return None;
                    }
                    let air_date = item
                        .get("aired")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    Some((
                        season,
                        Episode {
                            number,
                            title: str_field(item, "name"),
                            air_date,
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let next = v
        .get("links")
        .and_then(|l| l.get("next"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (episodes, next)
}

// ---------------------------------------------------------------------------
// endpoints

fn search(
    creds: &TvdbCredentials,
    media_type: MediaType,
    query: &str,
    year: Option<i32>,
) -> Result<Vec<Candidate>> {
    let type_param = match media_type {
        MediaType::Tv => "series",
        MediaType::Movie => "movie",
    };
    let mut url = format!(
        "{BASE}/search?query={}&type={type_param}",
        url_encode(query)
    );
    if let Some(y) = year {
        url.push_str(&format!("&year={y}"));
    }
    match authed_get(creds, &url)? {
        Some(v) => Ok(parse_search_results(&v, media_type)),
        None => Ok(Vec::new()),
    }
}

/// Search TV series by title, optionally narrowed by year.
pub fn search_tv(
    creds: &TvdbCredentials,
    query: &str,
    year: Option<i32>,
    _lang: TmdbLanguage,
) -> Result<Vec<Candidate>> {
    search(creds, MediaType::Tv, query, year)
}

/// Search movies by title, optionally narrowed by year.
pub fn search_movie(
    creds: &TvdbCredentials,
    query: &str,
    year: Option<i32>,
    _lang: TmdbLanguage,
) -> Result<Vec<Candidate>> {
    search(creds, MediaType::Movie, query, year)
}

fn get_by_id(
    creds: &TvdbCredentials,
    media_type: MediaType,
    id: u64,
    lang: TmdbLanguage,
) -> Result<Option<Candidate>> {
    let path = match media_type {
        MediaType::Tv => format!("{BASE}/series/{id}/extended"),
        MediaType::Movie => format!("{BASE}/movies/{id}/extended"),
    };
    match authed_get(creds, &path)? {
        Some(v) => Ok(parse_extended(
            &v,
            media_type,
            lang == TmdbLanguage::ZhCn,
        )),
        None => Ok(None),
    }
}

/// Fetch one series by TVDB id (extended record). `Ok(None)`
/// means TVDB has no such id.
pub fn get_tv(
    creds: &TvdbCredentials,
    id: u64,
    lang: TmdbLanguage,
) -> Result<Option<Candidate>> {
    get_by_id(creds, MediaType::Tv, id, lang)
}

/// Fetch one movie by TVDB id (extended record).
pub fn get_movie(
    creds: &TvdbCredentials,
    id: u64,
    lang: TmdbLanguage,
) -> Result<Option<Candidate>> {
    get_by_id(creds, MediaType::Movie, id, lang)
}

/// Fetch one language's whole episode listing, flattened to
/// (season, episode) pairs across all pages. A language TVDB
/// does not carry for the series (404) yields an empty vec.
fn fetch_episode_listing(
    creds: &TvdbCredentials,
    series_id: u64,
    code: &str,
) -> Result<Vec<(u32, Episode)>> {
    let mut url = Some(format!(
        "{BASE}/series/{series_id}/episodes/default/{code}"
    ));
    let mut flat: Vec<(u32, Episode)> = Vec::new();
    let mut pages = 0;
    while let Some(page_url) = url.take() {
        pages += 1;
        if pages > 10 {
            break;
        }
        match authed_get(creds, &page_url)? {
            Some(v) => {
                let (eps, next) = parse_episodes_page(&v);
                flat.extend(eps);
                url = next;
            }
            None => break, // 404: no listing in this language
        }
    }
    Ok(flat)
}

fn bucket_episodes(flat: Vec<(u32, Episode)>) -> Vec<(u32, Vec<Episode>)> {
    let mut buckets: BTreeMap<u32, Vec<Episode>> = BTreeMap::new();
    for (season, ep) in flat {
        buckets.entry(season).or_default().push(ep);
    }
    buckets
        .into_iter()
        .map(|(season, mut eps)| {
            eps.sort_by_key(|e| e.number);
            (season, eps)
        })
        .collect()
}

/// Fill missing titles / air dates in `primary` from `fallback`,
/// matched by (season, episode number). The Chinese listing
/// often carries the episode but leaves its name untranslated
/// (null) — the English listing supplies it (verified
/// against the live API).
pub fn fill_episode_gaps(
    primary: &mut [(u32, Vec<Episode>)],
    fallback: &[(u32, Vec<Episode>)],
) {
    fn title_missing(e: &Episode) -> bool {
        e.title.is_empty()
    }
    let mut by_key: BTreeMap<(u32, u32), &Episode> = BTreeMap::new();
    for (season, eps) in fallback {
        for ep in eps {
            by_key.insert((*season, ep.number), ep);
        }
    }
    for (season, eps) in primary.iter_mut() {
        for ep in eps.iter_mut() {
            let Some(fb) = by_key.get(&(*season, ep.number)) else {
                continue;
            };
            if title_missing(ep) {
                ep.title = fb.title.clone();
            }
            if ep.air_date.is_none() {
                ep.air_date = fb.air_date.clone();
            }
        }
    }
}

/// Fetch a series' whole episode listing, bucketed by season
/// (ascending). Chinese UI: the Chinese listing, with any gaps
/// (untranslated titles, missing dates) filled from the English
/// listing; if there is no Chinese listing at all, English
/// alone. English UI: English only.
pub fn get_episodes(
    creds: &TvdbCredentials,
    series_id: u64,
    lang: TmdbLanguage,
) -> Result<Vec<(u32, Vec<Episode>)>> {
    let (primary, fallback) = match lang {
        TmdbLanguage::ZhCn => ("zho", Some("eng")),
        TmdbLanguage::EnUs => ("eng", None),
    };
    let mut result = bucket_episodes(fetch_episode_listing(creds, series_id, primary)?);
    if let Some(code) = fallback {
        let incomplete = result.is_empty()
            || result.iter().any(|(_, eps)| {
                eps.iter()
                    .any(|e| e.title.is_empty() || e.air_date.is_none())
            });
        if incomplete {
            let fb_flat = fetch_episode_listing(creds, series_id, code)?;
            if !fb_flat.is_empty() {
                let fb = bucket_episodes(fb_flat);
                if result.is_empty() {
                    result = fb;
                } else {
                    fill_episode_gaps(&mut result, &fb);
                }
            }
        }
    }
    Ok(result)
}
