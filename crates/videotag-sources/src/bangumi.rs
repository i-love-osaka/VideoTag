//! Bangumi (bgm.tv) API v0 endpoints used by VideoTag.
//!
//! Keyless, but the API refuses generic user agents, so every
//! request carries a descriptive one naming the app. Bangumi is
//! an anime-first catalogue with a live-action corner: subject
//! type 2 (anime) and 6 (live action) are the only ones searched,
//! and subjects are series by nature — this source only ever
//! feeds TV matching.
//!
//! One Bangumi subject is usually ONE season of a show; the
//! episode list therefore belongs to a single season (the app
//! decides which of the work's seasons it fills). Subject fields
//! map onto the shared shape as: Chinese title `name_cn`,
//! original title `name` (also the folder format's "(English)"
//! slot — the original-language name is its equivalent here),
//! year from `date`, real user rating from `rating`.

use crate::http::{self, Request};
use crate::{Candidate, Episode, IdPlatform, MediaType, Result, SourceError, TmdbLanguage};
use serde_json::Value;

const BASE: &str = "https://api.bgm.tv/v0";

/// Bangumi rejects requests without a descriptive User-Agent.
const BGM_USER_AGENT: &str = "VideoTag/0.1 (https://mrben.net)";

/// GET JSON from Bangumi. A 404 yields `Ok(None)`; any other
/// non-2xx is an [`SourceError::Api`] carrying Bangumi's `title`.
fn fetch_json(url: &str) -> Result<Option<Value>> {
    let (status, body) = http::send(&Request::get(url).with_user_agent(BGM_USER_AGENT))?;
    if status == 404 {
        return Ok(None);
    }
    let parsed: std::result::Result<Value, _> = serde_json::from_str(&body);
    if !(200..300).contains(&status) {
        let message = parsed
            .as_ref()
            .ok()
            .and_then(|v| v.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("request failed")
            .to_string();
        return Err(SourceError::Api { status, message });
    }
    let value = parsed
        .map_err(|e| SourceError::Parse(format!("invalid JSON from Bangumi: {e}")))?;
    Ok(Some(value))
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Year from a Bangumi `date` (`1991-07-05`, possibly null).
fn year_of(v: &Value) -> Option<i32> {
    let date = v.get("date")?.as_str()?;
    let year: i32 = date.get(..4)?.parse().ok()?;
    if year > 0 {
        Some(year)
    } else {
        None
    }
}

/// Parse one Bangumi subject (search result and detail share the
/// shape) into a [`Candidate`]. With `chinese`, the title prefers
/// `name_cn`; otherwise the original `name` leads and the Chinese
/// name is the fallback.
pub fn parse_subject(v: &Value, chinese: bool) -> Option<Candidate> {
    let id = v.get("id")?.as_u64()?;
    let name = str_field(v, "name");
    let name_cn = str_field(v, "name_cn");
    let title = if chinese {
        if name_cn.is_empty() {
            name.clone()
        } else {
            name_cn
        }
    } else if name.is_empty() {
        name_cn
    } else {
        name.clone()
    };
    let rating = v.get("rating");
    let vote_average = rating
        .and_then(|r| r.get("score"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let vote_count = rating
        .and_then(|r| r.get("total"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Candidate {
        id,
        source: IdPlatform::Bangumi,
        media_type: MediaType::Tv,
        title,
        original_title: name.clone(),
        year: year_of(v),
        overview: str_field(v, "summary"),
        // The vote total doubles as the tiebreak "popularity":
        // widely rated subjects rank first.
        popularity: vote_count as f64,
        vote_average,
        vote_count,
        english_title: name,
    })
}

/// Parse a `/search/subjects` response body into candidates.
pub fn parse_search_results(v: &Value, chinese: bool) -> Vec<Candidate> {
    v.get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| parse_subject(item, chinese))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse one page of an `/episodes` response: the main-story
/// episodes (type 0) and the body's `total` count, for paging.
/// Episode numbers come from `ep`, falling back to the float
/// `sort`; titles prefer `name_cn`; an empty `airdate` is `None`.
pub fn parse_episodes_page(v: &Value) -> (Vec<Episode>, u64) {
    let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
    let episodes = v
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    if item.get("type").and_then(Value::as_u64) != Some(0) {
                        return None;
                    }
                    let number = match item.get("ep").and_then(Value::as_u64) {
                        Some(n) => n as u32,
                        None => item
                            .get("sort")
                            .and_then(Value::as_f64)
                            .map(|s| s.round() as u32)?,
                    };
                    if number == 0 {
                        return None;
                    }
                    let name = str_field(item, "name");
                    let name_cn = str_field(item, "name_cn");
                    let air_date = item
                        .get("airdate")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    Some(Episode {
                        number,
                        title: if name_cn.is_empty() { name } else { name_cn },
                        air_date,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (episodes, total)
}

/// Search subjects by keyword. Only anime (type 2) and live
/// action (type 6) subjects are returned — books, games and
/// music never match a video work.
pub fn search_subjects(query: &str, lang: TmdbLanguage) -> Result<Vec<Candidate>> {
    let body = serde_json::json!({
        "keyword": query,
        "sort": "match",
        "filter": { "type": [2, 6] },
    });
    let (status, text) = http::send(
        &Request::post_json(&format!("{BASE}/search/subjects"), &body.to_string())
            .with_user_agent(BGM_USER_AGENT),
    )?;
    if !(200..300).contains(&status) {
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "search failed".to_string());
        return Err(SourceError::Api { status, message });
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| SourceError::Parse(format!("invalid JSON from Bangumi: {e}")))?;
    Ok(parse_search_results(
        &value,
        lang == TmdbLanguage::ZhCn,
    ))
}

/// Fetch one subject by Bangumi id. `Ok(None)` means Bangumi has
/// no such id.
pub fn get_subject(id: u64, lang: TmdbLanguage) -> Result<Option<Candidate>> {
    match fetch_json(&format!("{BASE}/subjects/{id}"))? {
        Some(v) => Ok(parse_subject(&v, lang == TmdbLanguage::ZhCn)),
        None => Ok(None),
    }
}

/// Fetch a subject's whole main-story episode list (paged at
/// 100 per request), ascending by episode number.
pub fn get_episodes(subject_id: u64) -> Result<Vec<Episode>> {
    const LIMIT: u64 = 100;
    let mut out: Vec<Episode> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let url = format!(
            "{BASE}/episodes?subject_id={subject_id}&type=0&limit={LIMIT}&offset={offset}"
        );
        let Some(v) = fetch_json(&url)? else {
            break;
        };
        let (page, total) = parse_episodes_page(&v);
        let got = page.len() as u64;
        out.extend(page);
        offset += got;
        if got == 0 || offset >= total {
            break;
        }
    }
    out.sort_by_key(|e| e.number);
    Ok(out)
}
