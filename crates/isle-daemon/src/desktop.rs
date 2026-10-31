// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! The desktop's calendars' own events, read straight from Evolution Data Server (GNOME Online
//! Accounts, so Google's too): the ones the island's calendar page shows beside Isle's own, so the
//! AI knows the user's day whole. Asked once and kept [`CACHE_FOR`], so a turn never waits twice.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{
    Datelike, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone, Utc,
    Weekday,
};
use futures_util::future::join_all;
use log::debug;
use zbus::Connection;

use crate::eds;

/// The events as last read, and when: asked again within this, they come at once.
const CACHE_FOR: Duration = Duration::from_secs(60);
/// Backends left to isled: "local" is EDS's own calendar, which stands in for Isle's own; its
/// events are isled's already, and listing them too would say them twice.
const OWN_BACKENDS: [&str; 1] = ["local"];
/// A repeat is followed this many occurrences at the most, in case a rule has no end.
const MAX_OCCURRENCES: u64 = 500;

/// The events as last read, and the days they were read for.
struct Read {
    events: Vec<Desktop>,
    begin: NaiveDate,
    end: NaiveDate,
    at: Instant,
}

/// The events as last read, and when.
static CACHE: LazyLock<Mutex<Option<Read>>> = LazyLock::new(Mutex::default);

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
    if let Some(read) = CACHE.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
        && read.at.elapsed() < CACHE_FOR
        && read.begin <= begin
        && end <= read.end
    {
        return cut(&read.events, begin, end);
    }
    let found = read_all(bus, begin, end).await;
    *CACHE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Read {
        events: found.clone(),
        begin,
        end,
        at: Instant::now(),
    });
    cut(&found, begin, end)
}

/// The events kept, cut to the days asked for.
fn cut(all: &[Desktop], begin: NaiveDate, end: NaiveDate) -> Vec<Desktop> {
    let (begin, end) = (begin.format("%F").to_string(), end.format("%F").to_string());
    all.iter()
        .filter(|event| (begin.as_str()..=end.as_str()).contains(&event.date.as_str()))
        .cloned()
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
        .map(|uid| async move { read_one(bus, uid, &query(begin, end), begin, end).await })
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
/// of a source whole at once, from its cache; a repeating event is answered once, as its rule, so
/// it is spread over the range here. A calendar out of reach answers with none; isled lets go of it
/// however the read went.
async fn read_one(
    bus: &Connection,
    uid: &str,
    query: &str,
    begin: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<Desktop>, zbus::Error> {
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
    Ok(ics
        .iter()
        .flat_map(|text| text_events(text, begin, end))
        .collect())
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

/// An ics text's VEVENTs as they fall between `begin` and `end`, as `Desktop`s: a repeating event
/// once for each of its occurrences in the range, the way the calendar itself shows it.
fn text_events(ics: &str, begin: NaiveDate, end: NaiveDate) -> Vec<Desktop> {
    let lines = unfolded(ics);
    let mut entries: Vec<Entry> = Vec::new();
    let mut vevent: Vec<&str> = Vec::new();
    for line in &lines {
        match line.as_str() {
            "BEGIN:VEVENT" => vevent.clear(),
            "END:VEVENT" => {
                if let Some(entry) = entry(&vevent) {
                    entries.push(entry);
                }
            }
            _ => vevent.push(line),
        }
    }
    // An occurrence moved to another day (its own RECURRENCE-ID) stands in for the one its repeat
    // would have put there.
    let moved: Vec<(String, NaiveDate)> = entries
        .iter()
        .filter_map(|entry| Some((entry.uid.clone()?, entry.moved?.date())))
        .collect();
    let mut found: Vec<Desktop> = entries
        .iter()
        .flat_map(|entry| entry.occurrences(begin, end, &moved))
        .collect();
    found.sort_by(|one, other| (&one.date, &one.start).cmp(&(&other.date, &other.start)));
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

/// A VEVENT's lines as one entry: its summary, when it starts and ends, and how it repeats.
struct Entry {
    title: String,
    /// The event's own name, shared by its repeat and the occurrences moved apart from it.
    uid: Option<String>,
    /// The day this occurrence was moved to, for one standing apart from its repeat.
    moved: Option<Starts>,
    start: Starts,
    end: Option<Starts>,
    rrule: Option<Rrule>,
    exdates: Vec<Starts>,
}

/// A VEVENT's lines as one [`Entry`], or none when it has no summary or no start.
fn entry(lines: &[&str]) -> Option<Entry> {
    let mut summary = String::new();
    let mut uid = None;
    let mut moved = None;
    let mut start = None;
    let mut end = None;
    let mut repeat = None;
    let mut exdates = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let (name, _) = name.split_once(';').unwrap_or((name, ""));
        match name {
            "SUMMARY" if summary.is_empty() => summary = title(value),
            "UID" => uid = Some(value.to_owned()),
            "DTSTART" => start = Some(starts(value)?),
            "DTEND" => end = Some(starts(value)?),
            "RECURRENCE-ID" => moved = Some(starts(value)?),
            "RRULE" => repeat = Some(rrule(value)),
            "EXDATE" => exdates.extend(value.split(',').filter_map(starts)),
            _ => {}
        }
    }
    let start = start?;
    (!summary.is_empty()).then_some(Entry {
        title: summary,
        uid,
        moved,
        start,
        end,
        rrule: repeat,
        exdates,
    })
}

impl Entry {
    /// This entry's occurrences from `begin` to `end`, one `Desktop` each. `moved` holds the days
    /// other entries of the same event have claimed (`uid`, day): the repeat does not run there.
    fn occurrences(
        &self,
        begin: NaiveDate,
        end: NaiveDate,
        moved: &[(String, NaiveDate)],
    ) -> Vec<Desktop> {
        let (time, all_day) = match self.start {
            Starts::Day(_) => (NaiveTime::MIN, true),
            Starts::At(at) => (at.time(), false),
        };
        let length = self
            .end
            .map(|end| end.at() - self.start.at())
            .filter(|length| *length >= TimeDelta::zero());
        let kept = |day: NaiveDate| {
            !self.exdates.iter().any(|skip| skip.matches(day, time))
                && !moved
                    .iter()
                    .any(|(uid, at)| self.uid.as_deref() == Some(uid.as_str()) && *at == day)
        };
        self.days(end)
            .into_iter()
            .filter(|day| *day >= begin && *day <= end && kept(*day))
            .map(|day| Desktop {
                title: self.title.clone(),
                date: day.format("%F").to_string(),
                start: (!all_day).then(|| time.format("%H:%M").to_string()),
                end: length
                    .map(|length| day.and_time(time) + length)
                    .filter(|at| at.date() == day)
                    .map(|at| at.format("%H:%M").to_string()),
            })
            .collect()
    }

    /// The days this event falls on, from its first on through `end`'s day, as its rule says: its
    /// start alone when it does not repeat.
    fn days(&self, end: NaiveDate) -> Vec<NaiveDate> {
        let from = self.start.date();
        let Some(rule) = &self.rrule else {
            return vec![from];
        };
        let time = self.start.at().time();
        let all_day = matches!(self.start, Starts::Day(_));
        let horizon = end.succ_opt().unwrap_or(end);
        let over = |day: NaiveDate| {
            day > horizon
                || rule.until.is_some_and(|until| {
                    if all_day {
                        day > until.date()
                    } else {
                        day.and_time(time) > until
                    }
                })
        };
        let weekdays: Vec<Weekday> = if rule.weekdays.is_empty() {
            vec![from.weekday()]
        } else {
            rule.weekdays.clone()
        };
        let monday = from - Days::new(from.weekday().num_days_from_monday().into());
        let mut days = Vec::new();
        let mut step = 0u64;
        while (days.len() as u64) < MAX_OCCURRENCES {
            let batch: Vec<NaiveDate> = match rule.freq {
                Freq::Daily => vec![from + Days::new(rule.interval as u64 * step)],
                Freq::Weekly => {
                    let base = monday + Days::new(rule.interval as u64 * 7 * step);
                    weekdays
                        .iter()
                        .map(|weekday| base + Days::new(weekday.num_days_from_monday().into()))
                        .filter(|day| *day >= from)
                        .collect()
                }
                Freq::Monthly => add_months(from, rule.interval as i32 * step as i32)
                    .into_iter()
                    .collect(),
                Freq::Yearly => add_months(from, 12 * rule.interval as i32 * step as i32)
                    .into_iter()
                    .collect(),
            };
            let empty = batch.is_empty();
            match batch.iter().find(|day| over(**day)).copied() {
                Some(stop) => {
                    days.extend(batch.iter().take_while(|day| **day < stop).copied());
                    break;
                }
                None => days.extend(batch),
            }
            step += 1;
            // A month or year without that day is stepped over, so an empty batch is not the end.
            if empty && step > MAX_OCCURRENCES {
                break;
            }
        }
        days.truncate(rule.count.map_or(usize::MAX, |count| count as usize));
        days.sort_unstable();
        days.dedup();
        days
    }
}

/// When an event starts: a whole day, or a time on one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Starts {
    Day(NaiveDate),
    At(NaiveDateTime),
}

impl Starts {
    fn date(self) -> NaiveDate {
        match self {
            Starts::Day(day) => day,
            Starts::At(at) => at.date(),
        }
    }

    fn at(self) -> NaiveDateTime {
        match self {
            Starts::Day(day) => day.and_time(NaiveTime::MIN),
            Starts::At(at) => at,
        }
    }

    /// Whether this is the day (and time, timed) that `day` and `time` name.
    fn matches(self, day: NaiveDate, time: NaiveTime) -> bool {
        match self {
            Starts::Day(at) => at == day,
            Starts::At(at) => at.date() == day && at.time() == time,
        }
    }
}

/// An ics date or date-time as a [`Starts`]: a day alone ("20261105") is an all-day one; "...Z" is
/// UTC (Google's usual); a `TZID` or a bare one is the wall clock where the user is, as shown.
fn starts(value: &str) -> Option<Starts> {
    let value = value.trim();
    if value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Starts::Day(NaiveDate::parse_from_str(value, "%Y%m%d").ok()?));
    }
    let utc = value.ends_with('Z');
    let wall = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
    let at = if utc {
        Local.from_utc_datetime(&wall).naive_local()
    } else {
        Local.from_local_datetime(&wall).earliest()?.naive_local()
    };
    Some(Starts::At(at))
}

/// How an event repeats, as its `RRULE` says: as much of RFC 5545 as the calendars met use.
struct Rrule {
    freq: Freq,
    interval: u32,
    count: Option<u32>,
    until: Option<NaiveDateTime>,
    weekdays: Vec<Weekday>,
}

enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// An `RRULE`'s words as a rule; parts Isle does not use are passed over.
fn rrule(text: &str) -> Rrule {
    let mut rule = Rrule {
        freq: Freq::Weekly,
        interval: 1,
        count: None,
        until: None,
        weekdays: Vec::new(),
    };
    for part in text.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "FREQ" => {
                rule.freq = match value {
                    "DAILY" => Freq::Daily,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    _ => Freq::Weekly,
                };
            }
            "INTERVAL" => rule.interval = value.parse::<u32>().unwrap_or(1).max(1),
            "COUNT" => rule.count = value.parse().ok(),
            "UNTIL" => rule.until = starts(value).map(Starts::at),
            "BYDAY" => rule.weekdays = value.split(',').filter_map(weekday).collect(),
            _ => {}
        }
    }
    rule
}

/// "MO" through "SU" as a weekday; an order prefix ("2MO") is passed over.
fn weekday(text: &str) -> Option<Weekday> {
    match text
        .trim_start_matches(|char: char| char.is_ascii_digit())
        .to_uppercase()
        .as_str()
    {
        "MO" => Some(Weekday::Mon),
        "TU" => Some(Weekday::Tue),
        "WE" => Some(Weekday::Wed),
        "TH" => Some(Weekday::Thu),
        "FR" => Some(Weekday::Fri),
        "SA" => Some(Weekday::Sat),
        "SU" => Some(Weekday::Sun),
        _ => None,
    }
}

/// `day` moved `months` on in the calendar: the same day of the month, or none where there is no
/// such day (the 31st of a short month).
fn add_months(day: NaiveDate, months: i32) -> Option<NaiveDate> {
    let months = day.year() * 12 + day.month0() as i32 + months;
    NaiveDate::from_ymd_opt(months.div_euclid(12), months.rem_euclid(12) as u32 + 1, day.day())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn day(day: &str) -> NaiveDate {
        NaiveDate::parse_from_str(day, "%F").unwrap()
    }

    /// A calendar's answer, as EDS gives it: one VEVENT per line group.
    fn ics(vevents: &[&str]) -> String {
        vevents
            .iter()
            .map(|body| format!("BEGIN:VCALENDAR\nBEGIN:VEVENT\n{body}\nEND:VEVENT\nEND:VCALENDAR"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_plain_event_falls_on_its_own_day() {
        let text = ics(&["UID:1\nSUMMARY:Standup\nDTSTART:20261021T090000\nDTEND:20261021T091500"]);
        assert_eq!(
            text_events(&text, day("2026-10-20"), day("2026-10-22")),
            [Desktop {
                title: "Standup".into(),
                date: "2026-10-21".into(),
                start: Some("09:00".into()),
                end: Some("09:15".into()),
            }]
        );
    }

    #[test]
    fn a_weekly_repeat_comes_once_a_week() {
        let text = ics(&[
            "UID:1\nSUMMARY:Class\nDTSTART:20260825T090000\nDTEND:20260825T115000\n\
             RRULE:FREQ=WEEKLY;UNTIL=20261208T020000Z",
        ]);
        let found = text_events(&text, day("2026-09-07"), day("2026-09-21"));
        assert_eq!(
            found.iter().map(|event| event.date.as_str()).collect::<Vec<_>>(),
            ["2026-09-08", "2026-09-15"]
        );
    }

    #[test]
    fn a_moved_occurrence_takes_the_day_it_moved_to() {
        let text = ics(&[
            "UID:1\nSUMMARY:Class\nDTSTART:20260825T090000\nDTEND:20260825T115000\n\
             RRULE:FREQ=WEEKLY;UNTIL=20261208T020000Z",
            "UID:1\nRECURRENCE-ID:20260908T090000\nSUMMARY:Class\n\
             DTSTART:20260910T130000\nDTEND:20260910T155000",
        ]);
        let found = text_events(&text, day("2026-09-07"), day("2026-09-21"));
        assert_eq!(
            found
                .iter()
                .map(|event| (event.date.as_str(), event.start.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("2026-09-10", Some("13:00")),
                ("2026-09-15", Some("09:00")),
            ]
        );
    }

    #[test]
    fn a_skipped_day_is_left_out() {
        let text = ics(&[
            "UID:1\nSUMMARY:Class\nDTSTART:20260825T090000\nEXDATE:20260908T090000\n\
             RRULE:FREQ=WEEKLY;COUNT=4",
        ]);
        let found = text_events(&text, day("2026-08-24"), day("2026-09-30"));
        assert_eq!(
            found.iter().map(|event| event.date.as_str()).collect::<Vec<_>>(),
            ["2026-08-25", "2026-09-01", "2026-09-15"]
        );
    }

    #[test]
    fn an_all_day_event_has_no_times() {
        let text = ics(&["UID:1\nSUMMARY:Holiday\nDTSTART;VALUE=DATE:20261105"]);
        assert_eq!(
            text_events(&text, day("2026-11-01"), day("2026-11-10")),
            [Desktop {
                title: "Holiday".into(),
                date: "2026-11-05".into(),
                start: None,
                end: None,
            }]
        );
    }

    #[test]
    fn events_outside_the_range_are_left_out() {
        let text = ics(&[
            "UID:1\nSUMMARY:Near\nDTSTART:20261105T090000",
            "UID:2\nSUMMARY:Far\nDTSTART:20261225T090000",
        ]);
        assert_eq!(
            text_events(&text, day("2026-11-01"), day("2026-11-10"))
                .iter()
                .map(|event| event.title.as_str())
                .collect::<Vec<_>>(),
            ["Near"]
        );
    }
}
