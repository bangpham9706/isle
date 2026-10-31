// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! The desktop's calendars' own events, read straight from Evolution Data Server (GNOME Online
//! Accounts, so Google's too): the ones the island's calendar page shows beside Isle's own, so the
//! AI knows the user's day whole. Asked once and kept [`CACHE_FOR`], so a turn never waits twice.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use futures_util::future::join_all;
use log::debug;
use zbus::Connection;

use crate::eds;

/// The events as last read, and when: asked again within this, they come at once.
const CACHE_FOR: Duration = Duration::from_secs(60);
/// Backends left to isled: "local" is EDS's own calendar, which stands in for Isle's own; its
/// events are isled's already, and listing them too would say them twice.
const OWN_BACKENDS: [&str; 1] = ["local"];

/// The events as last read, and when.
static CACHE: LazyLock<Mutex<Option<(Vec<Desktop>, Instant)>>> = LazyLock::new(Mutex::default);

/// One of the desktop's events, in Isle's own shape, without Isle's id (the agent's calendar
/// actions work on Isle's own alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desktop {
    pub title: String,
    /// The day, as "YYYY-MM-DD".
    pub date: String,
    /// Start time as "HH:MM"; `None` for an all-day event.
    pub start: Option<String>,
    /// End time as "HH:MM" when there is one that day.
    pub end: Option<String>,
}

/// The desktop's events from the day `begin` is on through `end`'s. Empty when EDS says nothing.
pub async fn events(bus: &Connection, begin: NaiveDate, end: NaiveDate) -> Vec<Desktop> {
    if let Some((found, at)) = CACHE.lock().unwrap_or_else(PoisonError::into_inner).clone()
        && at.elapsed() < CACHE_FOR
    {
        return window(found, begin, end);
    }
    let found = read_all(bus, begin, end).await;
    *CACHE.lock().unwrap_or_else(PoisonError::into_inner) = Some((found.clone(), Instant::now()));
    window(found, begin, end)
}

/// The events kept, cut to the days asked for.
fn window(all: Vec<Desktop>, begin: NaiveDate, end: NaiveDate) -> Vec<Desktop> {
    let (begin, end) = (begin.format("%F").to_string(), end.format("%F").to_string());
    all.into_iter()
        .filter(|event| (begin.as_str()..=end.as_str()).contains(&event.date.as_str()))
        .collect()
}

/// Every calendar's events, read at once; a calendar that says nothing answers with none.
async fn read_all(bus: &Connection, begin: NaiveDate, end: NaiveDate) -> Vec<Desktop> {
    let sources = match eds::sources(bus).await {
        Ok(sources) => sources,
        Err(err) => {
            debug!("could not list the desktop's calendars: {err}");
            return Vec::new();
        }
    };
    let files: HashMap<&str, eds::KeyFile> = sources
        .iter()
        .map(|(uid, data)| (uid.as_str(), eds::key_file(data)))
        .collect();
    let reads = files
        .iter()
        .filter_map(|(uid, file)| {
            let enabled = |group| eds::value(file, group, "Enabled") != "false";
            if !file.contains_key("Calendar")
                || OWN_BACKENDS.contains(&eds::value(file, "Calendar", "BackendName"))
                || !enabled("Data Source")
                || !enabled("Calendar")
                || eds::value(file, "Calendar", "Selected") == "false"
            {
                return None;
            }
            Some(uid.to_owned())
        })
        .map(|uid| async move { read_one(bus, uid, &query(begin, end)).await })
        .collect::<Vec<_>>();
    join_all(reads)
        .await
        .into_iter()
        .flat_map(|one| match one {
            Ok(found) => found,
            Err(err) => {
                debug!("a calendar did not answer: {err}");
                Vec::new()
            }
        })
        .collect()
}

/// One calendar's events of the range, as `Desktop`s. `GetObjectList` answers the iCalendar texts
/// of a source whole at once, from its cache. A calendar out of reach answers with none; isled
/// lets go of it however the read went.
async fn read_one(bus: &Connection, uid: &str, query: &str) -> Result<Vec<Desktop>, zbus::Error> {
    let (path, name) = eds::timed(eds::open_calendar(bus, uid)).await?;
    let opened = eds::timed(bus.call_method(
        Some(name.as_str()),
        path.as_str(),
        Some(eds::CALENDAR),
        "Open",
        &(),
    ));
    let ics: Vec<String> = match opened.await {
        Ok(_) => match eds::timed(bus.call_method(
            Some(name.as_str()),
            path.as_str(),
            Some(eds::CALENDAR),
            "GetObjectList",
            &(query.to_owned(),),
        ))
        .await
        {
            Ok(reply) => reply.body().deserialize()?,
            Err(_) => Vec::new(),
        },
        Err(err) => {
            debug!("could not open calendar {uid} for reading: {err}");
            Vec::new()
        }
    };
    eds::close_calendar(bus, &path, &name, uid).await;
    Ok(ics.iter().flat_map(|text| text_events(text)).collect())
}

/// An EDS source of the range, its own words: a day before `begin` and two after `end`, so events
/// of the days asked come from any clock.
fn query(begin: NaiveDate, end: NaiveDate) -> String {
    let to = end
        .succ_opt()
        .and_then(|after| after.succ_opt())
        .unwrap_or(end);
    format!(
        "(and (occur-in-time-range? (make-time \"{}\") (make-time \"{}\")))",
        midnight(begin.pred_opt().unwrap_or(begin)),
        midnight(to),
    )
}

/// `day`'s local midnight as a UTC time, as EDS's `make-time` takes it: "YYYYMMDDTHHMMSSZ".
fn midnight(day: NaiveDate) -> String {
    let at = Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap_or_else(Local::now)
        .with_timezone(&Utc);
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// An ics text's VEVENTs, as `Desktop`s.
fn text_events(ics: &str) -> Vec<Desktop> {
    let lines = unfolded(ics);
    let mut found = Vec::new();
    let mut vevent: Vec<&str> = Vec::new();
    for line in &lines {
        match line.as_str() {
            "BEGIN:VEVENT" => vevent.clear(),
            "END:VEVENT" => {
                if let Some(event) = vevent_event(&vevent) {
                    found.push(event);
                }
            }
            _ => vevent.push(line),
        }
    }
    found
}

/// Lines with their continuations (a leading space or tab) folded onto the line before.
fn unfolded(ics: &str) -> Vec<String> {
    ics.lines().fold(Vec::new(), |mut all: Vec<String>, line| {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = all.last_mut() {
                last.push_str(&line[1..]);
            }
        } else {
            all.push(line.to_owned());
        }
        all
    })
}

/// A VEVENT's lines as one `Desktop`: its summary and its start (and end) read, or none when there
/// is no summary or no start.
fn vevent_event(lines: &[&str]) -> Option<Desktop> {
    let mut summary = "";
    let mut start: Option<(&str, &str)> = None;
    let mut end: Option<(&str, &str)> = None;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let (name, params) = name.split_once(';').unwrap_or((name, ""));
        match name {
            "SUMMARY" if summary.is_empty() => summary = value,
            "DTSTART" => start = Some((params, value)),
            "DTEND" => end = Some((params, value)),
            _ => {}
        }
    }
    let (start_params, start_value) = start?;
    if summary.is_empty() {
        return None;
    }
    let (date, start) = when(start_params, start_value)?;
    // The end only counts when the event ends that same day; models reading "HH:MM" do not follow
    // an event over midnight. An all-day one has neither time.
    let end = end
        .and_then(|(params, value)| when(params, value))
        .and_then(|(end_date, end_time)| if end_date == date { end_time } else { None });
    Some(Desktop {
        title: title(summary),
        date,
        start,
        end,
    })
}

/// An ics summary as plain words: its escapes (`\,`, `\;`, `\\`, `\n`) undone.
fn title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        if char == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push_str(", "),
                Some(',' | ';' | '\\') => {}
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(char);
        }
    }
    out.trim().to_owned()
}

/// An ics date or date-time as ("YYYY-MM-DD", "HH:MM" or `None` for an all-day date). A day alone
/// is an all-day one; "…Z" is UTC (Google's usual); a `TZID` or a bare one is the wall clock where
/// the user is: the same times the calendar shows.
fn when(params: &str, value: &str) -> Option<(String, Option<String>)> {
    let _ = params; // the TZID itself is not read: EDS's own ones are common enough to pass over
    // All day: "20261105".
    if value.len() == 8 && value.bytes().all(|char| char.is_ascii_digit()) {
        let day = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        return Some((format_day(day), None));
    }
    let utc = value.ends_with('Z');
    let wall = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
    let at = if utc {
        Local.from_utc_datetime(&wall)
    } else {
        Local.from_local_datetime(&wall).earliest()?
    };
    Some((
        format_day(at.date_naive()),
        Some(at.format("%H:%M").to_string()),
    ))
}

/// The day as "YYYY-MM-DD".
fn format_day(day: NaiveDate) -> String {
    day.format("%F").to_string()
}
