//! Online metadata sources for VideoTag: TMDB, TheTVDB and
//! Bangumi, all normalized into the one [`Candidate`] / [`Episode`]
//! shape the app consumes.
//!
//! Mirrors MusicTag's sources crate: plain functions over the system
//! `curl` binary, no async, no TLS stack. API keys are passed in by
//! the caller on every call and are never stored here (the TVDB
//! bearer token is the one exception — it is cached in memory only,
//! keyed to the credentials that produced it).

pub mod bangumi;
pub mod http;
pub mod tmdb;
pub mod tvdb;

pub use tmdb::{get_episodes, get_movie, get_tv, search_movie, search_tv};
pub use tvdb::TvdbCredentials;
pub use videotag_core::IdPlatform;

use std::fmt;

/// Which TMDB catalogue an entry belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaType {
    Tv,
    Movie,
}

/// Language of the source payloads: titles, overviews and episode
/// names come back in this language where the catalogue supports
/// it (per-field fallback to the original language when
/// untranslated). The app passes the one matching the system
/// language — naming follows the UI language. TMDB takes it as a
/// `language=` value; TheTVDB and Bangumi read it as "Chinese
/// first vs. original first".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmdbLanguage {
    ZhCn,
    EnUs,
}

impl TmdbLanguage {
    /// The `language=` query value TMDB expects.
    pub fn query_value(self) -> &'static str {
        match self {
            TmdbLanguage::ZhCn => "zh-CN",
            TmdbLanguage::EnUs => "en-US",
        }
    }
}

/// One search result (or detail record) from any of the three
/// catalogues: everything the UI needs to show a match before the
/// user confirms it.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// The id on `source`'s catalogue.
    pub id: u64,
    /// Which catalogue this record came from.
    pub source: IdPlatform,
    pub media_type: MediaType,
    /// Localized title (in the requested `TmdbLanguage`):
    /// `name` for TV, `title` for movies.
    pub title: String,
    /// Title in the work's original language.
    pub original_title: String,
    /// First-air / release year, from the date field.
    pub year: Option<i32>,
    pub overview: String,
    /// Popularity-ish score; only used to break `pick_best` ties.
    pub popularity: f64,
    /// Average user vote, 0.0–10.0 (meaningless when `vote_count`
    /// is 0). TVDB records never carry one (always 0 / 0).
    pub vote_average: f64,
    /// Number of votes behind `vote_average`.
    pub vote_count: u64,
    /// Title in en-US (for TVDB: its `name`, usually the English
    /// original; for Bangumi: the original `name`). Only filled by
    /// the detail lookups: TMDB fetches an extra en-US copy when
    /// the lookup language is not English; search results leave
    /// it empty.
    pub english_title: String,
}

/// One episode of a season, from `/tv/{id}/season/{n}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Episode {
    pub number: u32,
    pub title: String,
    pub air_date: Option<String>,
}

#[derive(Debug)]
pub enum SourceError {
    /// curl could not run or exited with a failure.
    Http(String),
    /// The response was not the JSON shape the source promises.
    Parse(String),
    /// A source answered with a non-2xx status (other than a handled 404).
    Api { status: u32, message: String },
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Http(msg) => write!(f, "network error: {msg}"),
            SourceError::Parse(msg) => write!(f, "parse error: {msg}"),
            SourceError::Api { status, message } => {
                write!(f, "API error ({status}): {message}")
            }
        }
    }
}

impl std::error::Error for SourceError {}

pub type Result<T> = std::result::Result<T, SourceError>;

/// Normalize a title for comparison: run it through the core crate's
/// search-title cleaner (drops tech tags, years and bracketed sources
/// if the query came straight from a file name), then fold case and
/// collapse whitespace.
fn normalize_title(title: &str) -> String {
    videotag_core::clean_search_title(title)
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Score one candidate against the query. Exact title equality (with
/// either the localized or the original title) dominates; a matching
/// year comes next, with a one-year tolerance worth slightly less —
/// regional release years often differ by one.
fn score(cand: &Candidate, query: &str, year: Option<i32>) -> i32 {
    let mut points = 0;
    if !query.is_empty() {
        let title = normalize_title(&cand.title);
        let original = normalize_title(&cand.original_title);
        if title == query || original == query {
            points += 100;
        } else if title.contains(query) || original.contains(query) {
            points += 30;
        }
    }
    if let (Some(want), Some(have)) = (year, cand.year) {
        match (want - have).abs() {
            0 => points += 50,
            1 => points += 35,
            _ => {}
        }
    }
    points
}

/// Pick the best candidate for `query_title` (optionally with a year)
/// and return its index. Ties on score are broken by popularity.
/// Any non-empty list yields a pick — the source's own ranking is a
/// sane fallback when nothing scores.
pub fn pick_best(cands: &[Candidate], query_title: &str, year: Option<i32>) -> Option<usize> {
    let query = normalize_title(query_title);
    let mut best: Option<(usize, i32, f64)> = None;
    for (idx, cand) in cands.iter().enumerate() {
        let points = score(cand, &query, year);
        let popularity = if cand.popularity.is_nan() {
            0.0
        } else {
            cand.popularity
        };
        let better = match best {
            None => true,
            Some((_, best_points, best_pop)) => {
                points > best_points || (points == best_points && popularity > best_pop)
            }
        };
        if better {
            best = Some((idx, points, popularity));
        }
    }
    best.map(|(idx, _, _)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(
        title: &str,
        original_title: &str,
        year: Option<i32>,
        popularity: f64,
    ) -> Candidate {
        Candidate {
            id: 1,
            source: IdPlatform::Tmdb,
            media_type: MediaType::Tv,
            title: title.to_string(),
            original_title: original_title.to_string(),
            year,
            overview: String::new(),
            popularity,
            vote_average: 0.0,
            vote_count: 0,
            english_title: String::new(),
        }
    }

    #[test]
    fn empty_list_has_no_pick() {
        assert_eq!(pick_best(&[], "红楼梦", Some(2010)), None);
    }

    #[test]
    fn exact_title_and_year_beats_popularity() {
        let cands = vec![
            cand("红楼梦之淫丧隋唐", "红楼梦之淫丧隋唐", Some(2010), 99.0),
            cand("红楼梦", "红楼梦", Some(1987), 80.0),
            cand("红楼梦", "红楼梦", Some(2010), 10.0),
        ];
        assert_eq!(pick_best(&cands, "红楼梦", Some(2010)), Some(2));
    }

    #[test]
    fn original_title_can_match() {
        let cands = vec![
            cand("美国队长4", "Captain America: Brave New World", Some(2025), 90.0),
            cand("美国队长", "Captain America: The First Avenger", Some(2011), 95.0),
        ];
        assert_eq!(
            pick_best(&cands, "Captain America: Brave New World", None),
            Some(0)
        );
    }

    #[test]
    fn title_match_is_case_insensitive() {
        let cands = vec![
            cand("Other Show", "Other Show", None, 50.0),
            cand("breaking bad", "Breaking Bad", Some(2008), 5.0),
        ];
        assert_eq!(pick_best(&cands, "Breaking Bad", None), Some(1));
    }

    #[test]
    fn year_off_by_one_still_counts() {
        // No exact title anywhere; the ±1 year candidate must beat an
        // unrelated-year candidate of equal popularity.
        let cands = vec![
            cand("Some Drama", "Some Drama", Some(2005), 10.0),
            cand("Some Drama", "Some Drama", Some(2011), 10.0),
        ];
        assert_eq!(pick_best(&cands, "Some Drama", Some(2010)), Some(1));
    }

    #[test]
    fn exact_year_beats_off_by_one() {
        let cands = vec![
            cand("Some Drama", "Some Drama", Some(2011), 10.0),
            cand("Some Drama", "Some Drama", Some(2010), 10.0),
        ];
        assert_eq!(pick_best(&cands, "Some Drama", Some(2010)), Some(1));
    }

    #[test]
    fn popularity_breaks_score_ties() {
        let cands = vec![
            cand("Drama A", "Drama A", None, 3.0),
            cand("Drama B", "Drama B", None, 7.5),
        ];
        assert_eq!(pick_best(&cands, "unrelated query", None), Some(1));
    }
}
