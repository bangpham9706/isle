// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! Lyrics from LRCLIB (<https://lrclib.net>), a free database of synced lyrics,
//! cached on disk so each song is only looked up once (a song without any, once a week).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use isle_core::lrc::{self, LyricLine};
use isle_core::track::{artist_forms, full_title, search_terms};
use log::{debug, warn};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::art::fnv1a;
use crate::store::cache_dir;

const API: &str = "https://lrclib.net/api";
const TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!(
    "Isle/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/zickrian/isle)"
);
/// How far a search result's length may differ from the track's, in seconds.
const DURATION_TOLERANCE: f64 = 3.0;
/// LRCLIB often answers "busy" (HTTP 503) or "slow down" (429) for a moment;
/// ask again this many times before giving up on the lookup. Its busy spells
/// have been measured to last some seconds, so the waits below add up to more
/// than that; giving in sooner is what shows as "Lyrics are busy right now".
const BUSY_RETRIES: u32 = 4;
/// Wait before asking again, unless the server says (`Retry-After`), and the
/// most we wait however long it asks for.
const BUSY_WAIT: Duration = Duration::from_secs(1);
const BUSY_WAIT_MAX: Duration = Duration::from_secs(5);
/// A song LRCLIB had no lyrics for is looked up again after this, as they may have been added.
const NOT_FOUND_RETRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// LRCLIB knows nothing longer (it refuses the length with HTTP 400): a mix, a podcast or a stream.
const LONGEST_SONG_S: f64 = 3600.0;

/// What the lyrics page shows for the track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsStatus {
    /// No track is playing.
    #[default]
    None,
    Loading,
    /// Lines have times; highlight them as the song plays.
    Synced,
    /// Lines have no times (`time_ms` is -1).
    Plain,
    Instrumental,
    NotFound,
    /// LRCLIB could not be reached, e.g. while offline. Never cached, and
    /// retried after a growing delay for as long as the track stays current.
    Error,
    /// LRCLIB answered but could not serve the lyrics right now (busy, rate
    /// limited or broken). Never cached, and retried like [`Self::Error`].
    Unavailable,
    /// Looking lyrics up online is turned off in Settings.
    Off,
}

impl LyricsStatus {
    /// Whether the lookup failed and should be tried again later.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Unavailable)
    }
}

/// A track's lyrics as sent to the extension and kept in the cache.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Lyrics {
    /// Identifies the track these lyrics belong to.
    pub track: String,
    pub status: LyricsStatus,
    pub lines: Vec<LyricLine>,
}

/// What we know about a track for looking up its lyrics.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackQuery {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length_s: f64,
}

impl TrackQuery {
    /// One string per track, for the cache file name and to match lyrics to the track.
    pub fn key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{:.0}",
            self.artist, self.title, self.album, self.length_s
        )
    }

    /// Lyrics with no lines, only a status, for this track.
    pub fn status(&self, status: LyricsStatus) -> Lyrics {
        Lyrics {
            track: self.key(),
            status,
            lines: Vec::new(),
        }
    }
}

/// One LRCLIB entry, as its API sends it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default)]
    artist_name: Option<String>,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
    // `Option` so a `null` reads as `false` instead of failing the whole list.
    #[serde(default)]
    instrumental: Option<bool>,
    duration: Option<f64>,
}

impl Record {
    /// Unknown counts as not instrumental.
    fn instrumental(&self) -> bool {
        self.instrumental.unwrap_or(false)
    }
}

/// Why a lookup failed, which decides what the island tells the user.
#[derive(Debug)]
enum Failure {
    /// LRCLIB could not be reached: offline, DNS, TLS, a timeout.
    Network(ureq::Error),
    /// LRCLIB answered with an error, or with something we could not read.
    Service(String),
}

impl fmt::Display for Failure {
    /// The reason, for the log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(err) => write!(f, "could not reach LRCLIB: {err}"),
            Self::Service(message) => f.write_str(message),
        }
    }
}

impl Failure {
    /// Offline shows as an error; a refusing server as unavailable.
    fn status(&self) -> LyricsStatus {
        match self {
            Self::Network(_) => LyricsStatus::Error,
            Self::Service(_) => LyricsStatus::Unavailable,
        }
    }
}

/// Looks up lyrics, consulting the disk cache first. Blocking.
pub fn fetch(query: &TrackQuery) -> Lyrics {
    let (title, artist) = search_terms(&query.title, &query.artist);
    // Without an artist it is rarely a song (a browser tab called
    // "Instagram"), and searching would match any song of that name.
    if title.is_empty() || artist.is_empty() || query.length_s > LONGEST_SONG_S {
        return query.status(LyricsStatus::NotFound);
    }
    let cache = cache_path(query);
    if let Some(cached) = cached(&cache) {
        return cached;
    }

    let lyrics = match lookup(query, title, artist) {
        Ok(Some(record)) => from_record(query, record),
        Ok(None) => query.status(LyricsStatus::NotFound),
        Err(err) => {
            warn!("lyrics lookup failed: {err}");
            return query.status(err.status());
        }
    };
    // Misses too, with the file's time saying when (see `cached`).
    if let Some(parent) = cache.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(&lyrics) {
        let _ = fs::write(cache, json);
    }
    lyrics
}

/// The cached lyrics at `path`; a miss only while younger than [`NOT_FOUND_RETRY`].
fn cached(path: &Path) -> Option<Lyrics> {
    let lyrics: Lyrics = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    if lyrics.status == LyricsStatus::NotFound {
        let written = fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if written.elapsed().unwrap_or_default() >= NOT_FOUND_RETRY {
            return None;
        }
    }
    Some(lyrics)
}

/// Asks LRCLIB in three steps, from the most exact to the loosest, and repeats
/// the two first for the other ways the same artists are written (see [`artist_forms`]).
fn lookup(query: &TrackQuery, title: String, artist: String) -> Result<Option<Record>, Failure> {
    let duration = format!("{:.0}", query.length_s);

    for artist in artist_forms(&artist) {
        // An exact match on title, artist, album and duration is the most precise.
        // LRCLIB takes a length of 1 s to an hour; under a second rounds to 0, which it refuses.
        if !artist.is_empty() && query.length_s >= 0.5 {
            let exact = get_json::<Record>(
                "get",
                &[
                    ("track_name", &title),
                    ("artist_name", &artist),
                    ("album_name", &query.album),
                    ("duration", &duration),
                ],
            )?;
            if let Some(record) = exact.filter(has_lyrics) {
                return Ok(Some(record));
            }
        }

        // Otherwise search, preferring synced lyrics of about the right length.
        // Without an artist any song of that name matches, so the length must fit.
        let mut params = vec![("track_name", title.as_str())];
        if !artist.is_empty() {
            params.push(("artist_name", artist.as_str()));
        }
        let results = get_json::<Vec<Record>>("search", &params)?.unwrap_or_default();
        if let Some(record) = best_match(results, query.length_s, &artist, artist.is_empty()) {
            return Ok(Some(record));
        }
    }

    // Last, for YouTube-style titles, search the whole title as free text (catches a wrongly
    // split "Artist - Song"). Any song can turn up here, so the length must fit.
    let text = full_title(&query.title);
    if text == title && !text.contains(" - ") {
        return Ok(None);
    }
    let results = get_json::<Vec<Record>>("search", &[("q", &text)])?.unwrap_or_default();
    Ok(best_match(results, query.length_s, &artist, true))
}

/// GETs `{API}/{endpoint}` and reads the JSON answer; `None` on HTTP 404.
/// Asks again a few times while LRCLIB says it is busy.
fn get_json<T: DeserializeOwned>(
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<Option<T>, Failure> {
    let mut attempt = 0;
    loop {
        let mut response = crate::http::get(&format!("{API}/{endpoint}"), TIMEOUT)
            .config()
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .build()
            .query_pairs(params.iter().copied())
            .call()
            .map_err(Failure::Network)?;
        let status = response.status().as_u16();
        match status {
            200 => {
                return match response.body_mut().read_json() {
                    Ok(value) => Ok(Some(value)),
                    Err(ureq::Error::Json(err)) => Err(Failure::Service(format!(
                        "LRCLIB sent an unreadable {endpoint} answer: {err}"
                    ))),
                    // The connection dropped while the body was coming in.
                    Err(err) => Err(Failure::Network(err)),
                };
            }
            404 => return Ok(None),
            429 | 502..=504 if attempt < BUSY_RETRIES => {
                attempt += 1;
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(BUSY_WAIT * attempt)
                    .min(BUSY_WAIT_MAX);
                debug!("LRCLIB {endpoint} answered HTTP {status}, asking again in {wait:?}");
                thread::sleep(wait);
            }
            _ => {
                return Err(Failure::Service(format!(
                    "LRCLIB {endpoint} answered HTTP {status}"
                )));
            }
        }
    }
}

/// Some lines to show, or marked instrumental.
fn has_lyrics(record: &Record) -> bool {
    record
        .synced_lyrics
        .as_deref()
        .is_some_and(|l| !l.trim().is_empty())
        || record
            .plain_lyrics
            .as_deref()
            .is_some_and(|l| !l.trim().is_empty())
        || record.instrumental()
}

/// Picks the best result: right artist and length first, then synced over plain lyrics.
/// With `strict`, a result whose known length does not fit is never picked.
fn best_match(results: Vec<Record>, length_s: f64, artist: &str, strict: bool) -> Option<Record> {
    let fits = |record: &Record| {
        length_s <= 0.0
            || record
                .duration
                .is_none_or(|duration| (duration - length_s).abs() <= DURATION_TOLERANCE)
    };
    let artist = artist.to_lowercase();
    let same_artist = |record: &Record| {
        !artist.is_empty()
            && record
                .artist_name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(&artist))
    };
    let score = |record: &Record| {
        (
            same_artist(record),
            fits(record),
            record.synced_lyrics.is_some(),
            record.plain_lyrics.is_some(),
        )
    };
    results
        .into_iter()
        .filter(has_lyrics)
        .filter(|record| !strict || fits(record))
        .max_by_key(score)
}

/// Synced lines if any, else plain ones, else instrumental or not found.
fn from_record(query: &TrackQuery, record: Record) -> Lyrics {
    let synced = record
        .synced_lyrics
        .as_deref()
        .map(lrc::parse)
        .unwrap_or_default();
    let (status, lines) = if !synced.is_empty() {
        (LyricsStatus::Synced, synced)
    } else if let Some(plain) = record
        .plain_lyrics
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        (LyricsStatus::Plain, lrc::plain(plain))
    } else if record.instrumental() {
        (LyricsStatus::Instrumental, Vec::new())
    } else {
        (LyricsStatus::NotFound, Vec::new())
    };
    Lyrics {
        track: query.key(),
        status,
        lines,
    }
}

/// Where the track's lyrics are cached, named by a hash of its key.
fn cache_path(query: &TrackQuery) -> PathBuf {
    cache_dir()
        .join("lyrics")
        .join(format!("{:016x}.json", fnv1a(&query.key())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(artist: &str, duration: Option<f64>, synced: bool) -> Record {
        Record {
            artist_name: Some(artist.into()),
            synced_lyrics: synced.then(|| "[00:01.00] la".into()),
            plain_lyrics: Some("la".into()),
            instrumental: None,
            duration,
        }
    }

    #[test]
    fn records_with_null_fields_parse() {
        let json = r#"[{"artistName":null,"syncedLyrics":null,"plainLyrics":"la",
            "instrumental":null,"duration":null}]"#;
        let records: Vec<Record> = serde_json::from_str(json).unwrap();
        assert!(has_lyrics(&records[0]));
        assert!(!records[0].instrumental());
    }

    #[test]
    fn the_right_artist_wins_over_synced_lyrics() {
        let results = vec![
            record("Someone Else", Some(216.0), true),
            record("Lumen Coast", Some(216.0), false),
        ];
        let best = best_match(results, 216.0, "Lumen Coast", false).unwrap();
        assert_eq!(best.artist_name.as_deref(), Some("Lumen Coast"));
    }

    #[test]
    fn strict_matching_rejects_the_wrong_length() {
        let results = vec![record("Lumen Coast", Some(198.0), true)];
        assert!(best_match(results, 216.0, "Lumen Coast", true).is_none());
        let results = vec![record("Lumen Coast", Some(198.0), true)];
        assert!(best_match(results, 216.0, "Lumen Coast", false).is_some());
        // An unknown track length cannot rule anything out.
        let results = vec![record("Lumen Coast", Some(198.0), true)];
        assert!(best_match(results, 0.0, "", true).is_some());
    }

    #[test]
    fn records_without_lyrics_are_skipped() {
        let mut empty = record("Lumen Coast", Some(216.0), false);
        empty.plain_lyrics = Some("  ".into());
        assert!(best_match(vec![empty], 216.0, "Lumen Coast", false).is_none());
    }

    #[test]
    fn nothing_longer_than_an_hour_is_looked_up() {
        let query = TrackQuery {
            title: "Lofi beats to study to".into(),
            artist: "Lofi Girl".into(),
            album: String::new(),
            length_s: 3.0 * 60.0 * 60.0,
        };
        // Answered at once, without the network or the cache.
        assert_eq!(fetch(&query).status, LyricsStatus::NotFound);
    }

    #[test]
    fn misses_are_cached_for_a_week() {
        let dir = std::env::temp_dir().join(format!("isle-lyrics-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let query = TrackQuery {
            title: "Song".into(),
            artist: "Lumen Coast".into(),
            album: String::new(),
            length_s: 216.0,
        };
        let path = dir.join("miss.json");
        fs::write(
            &path,
            serde_json::to_string(&query.status(LyricsStatus::NotFound)).unwrap(),
        )
        .unwrap();
        assert_eq!(cached(&path).unwrap().status, LyricsStatus::NotFound);
        let old = std::time::SystemTime::now() - NOT_FOUND_RETRY;
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(old).unwrap();
        assert!(cached(&path).is_none());
        // Lyrics found stay, however old.
        fs::write(
            &path,
            serde_json::to_string(&query.status(LyricsStatus::Plain)).unwrap(),
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(cached(&path).unwrap().status, LyricsStatus::Plain);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failures_are_told_apart() {
        let offline = Failure::Network(ureq::Error::ConnectionFailed);
        assert_eq!(offline.status(), LyricsStatus::Error);
        let busy = Failure::Service("LRCLIB search answered HTTP 503".into());
        assert_eq!(busy.status(), LyricsStatus::Unavailable);
        assert!(LyricsStatus::Error.is_failure());
        assert!(LyricsStatus::Unavailable.is_failure());
        assert!(!LyricsStatus::NotFound.is_failure());
    }

    #[test]
    #[ignore = "asks LRCLIB, and LRCLIB is busy at times"]
    fn finds_a_collaboration_as_a_browser_names_it() {
        let query = TrackQuery {
            title: "Love Me Harder".into(),
            artist: "Ariana Grande và The Weeknd".into(),
            album: "My Everything (Deluxe)".into(),
            length_s: 236.0,
        };
        let (title, artist) = search_terms(&query.title, &query.artist);
        assert!(
            lookup(&query, title, artist).unwrap().is_some(),
            "LRCLIB answered, but not with this song's lyrics"
        );
    }
}
