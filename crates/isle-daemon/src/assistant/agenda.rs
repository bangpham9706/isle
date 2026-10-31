// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! The user's own to-dos, events and reminders, summed up for the AI so it can answer
//! "what do I have today?".

use std::fmt::Write;

use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone};
use isle_core::calendar::Event;
use isle_core::reminders::Reminder;
use isle_core::todo::TodoItem;
use zbus::Connection;
use zbus::object_server::InterfaceRef;

use crate::calendar::CalendarService;
use crate::desktop::{self, Desktop};
use crate::reminders::RemindersService;
use crate::todo::TodoService;

/// Each list shows at most this many items.
const MAX_ITEMS: usize = 15;
/// The calendar shows more: two weeks of a full timetable runs past what the shorter lists need.
const MAX_EVENTS: usize = 40;
/// A longer title is cut to this many characters.
const MAX_TITLE_CHARS: usize = 100;
/// Events are listed for today and this many days after: two weeks, so the assistant can answer for
/// this week and the next alike; it tells them apart by the day each is on.
const DAYS_AHEAD: u64 = 13;

/// The services that keep the user's data, and the session bus to ask EDS on.
pub struct Sources {
    pub todo: InterfaceRef<TodoService>,
    pub calendar: InterfaceRef<CalendarService>,
    pub reminders: InterfaceRef<RemindersService>,
    pub bus: Connection,
}

impl Sources {
    /// The summary as of now, cut to the days `asked` names when it names them.
    pub async fn summary(&self, asked: &str) -> String {
        let todo = self.todo.get().await;
        let calendar = self.calendar.get().await;
        let reminders = self.reminders.get().await;
        let now = chrono::Local::now();
        let today = now.date_naive();
        let last = today
            .checked_add_days(Days::new(DAYS_AHEAD))
            .unwrap_or(today);
        let desktop = desktop::events(&self.bus, today, last).await;
        summary(
            todo.list().items(),
            calendar.calendar().events(),
            reminders.reminders().items(),
            &now,
            &desktop,
            asked_range(asked, today),
        )
    }
}

/// Open to-dos, events from today to thirteen days on (Isle's own and the desktop calendars'
/// alike), and
/// pending reminders, in plain text.
pub fn summary<Tz: TimeZone>(
    todos: &[TodoItem],
    events: &[Event],
    reminders: &[Reminder],
    now: &DateTime<Tz>,
    desktop: &[Desktop],
    focus: Option<(NaiveDate, NaiveDate, String)>,
) -> String {
    let mut text = String::new();

    let open = todos.iter().filter(|todo| !todo.done);
    list(
        &mut text,
        "Reminders (your to-dos on Isle's Reminders page)",
        open.map(|todo| format!("[#{}] {}", todo.id, title(&todo.title))),
    );

    let today = now.date_naive();
    let last = today
        .checked_add_days(Days::new(DAYS_AHEAD))
        .unwrap_or(today);
    // What the question names is all that is listed, so the assistant need not cut the two weeks down
    // itself; a question that names no days gets them whole.
    let (from, to, heading) = match focus {
        Some((begin, end, label)) => (begin, end, format!("Calendar ({label})")),
        None => (
            today,
            last,
            format!("Calendar, {today} to {last} (list only the days the person asked about)"),
        ),
    };
    // Each isled's own event's line, ready, so a desktop one that is the same event is not said twice.
    let coming: Vec<(NaiveDate, Option<String>, String)> = events
        .iter()
        .filter_map(|event| {
            let day = NaiveDate::parse_from_str(&event.date, "%Y-%m-%d").ok()?;
            ((from..=to).contains(&day)).then_some((day, event))
        })
        .map(|(day, event)| (day, event.start.clone(), own_line(day, event)))
        .collect();
    let mut lines = coming;
    for one in desktop {
        let Ok(day) = NaiveDate::parse_from_str(&one.date, "%Y-%m-%d") else {
            continue;
        };
        if !(from..=to).contains(&day) {
            continue;
        }
        // One that isle's own sent to a desktop calendar and that calendar lists: not said twice.
        if events.iter().any(|event| {
            event.date == one.date
                && event.start == one.start
                && event.end == one.end
                && title(&event.title) == title(&one.title)
        }) {
            continue;
        }
        lines.push((day, one.start.clone(), desktop_line(day, one)));
    }
    // All-day ones (no start) come first in their day.
    lines.sort_by(|(a_day, a_start, _), (b_day, b_start, _)| {
        (a_day, a_start).cmp(&(b_day, b_start))
    });
    list_capped(
        &mut text,
        &heading,
        MAX_EVENTS,
        lines.into_iter().map(|(_, _, line)| line),
    );

    let timezone = now.timezone();
    list(
        &mut text,
        "Timed reminders (alerts Isle sets itself)",
        reminders.iter().filter_map(|reminder| {
            let due = DateTime::from_timestamp_millis(reminder.due_ms)?.with_timezone(&timezone);
            let due = due.naive_local().format("%a %Y-%m-%d %H:%M");
            Some(format!(
                "[#{}] {due}: {}",
                reminder.id,
                title(&reminder.text)
            ))
        }),
    );
    text.truncate(text.trim_end().len());
    text
}

/// The days a question names ("today", "tomorrow", "this week", "next week"), as the range to list
/// and what to call it: none when it names no days, and the two weeks are listed whole.
fn asked_range(asked: &str, today: NaiveDate) -> Option<(NaiveDate, NaiveDate, String)> {
    let asked = asked.to_lowercase();
    let on = |offset: u64| today.checked_add_days(Days::new(offset));
    let named = |asked: &str, words: &[&str]| words.iter().any(|word| asked.contains(word));
    if named(&asked, &["today", "hôm nay"]) {
        return Some((today, today, format!("today, {today}")));
    }
    if named(&asked, &["tomorrow", "ngày mai"]) {
        let day = on(1)?;
        return Some((day, day, format!("tomorrow, {day}")));
    }
    let monday = u64::from(today.weekday().num_days_from_monday());
    // This week runs to Sunday, the last of its days.
    if named(&asked, &["this week", "tuần này"]) {
        let sunday = on(6 - monday)?;
        return Some((today, sunday, format!("this week, {today} to {sunday}")));
    }
    if named(&asked, &["next week", "tuần sau", "tuần tới"]) {
        let (first, last) = (on(7 - monday)?, on(13 - monday)?);
        return Some((first, last, format!("next week, {first} to {last}")));
    }
    None
}

/// One isled's own event as a calendar line, with its Isle id (the agent can act on it).
fn own_line(day: NaiveDate, event: &Event) -> String {
    let time = match (&event.start, &event.end) {
        (Some(start), Some(end)) => format!("{start}-{end}"),
        (Some(start), None) => start.clone(),
        (None, _) => "all day".into(),
    };
    format!(
        "[#{}] {}, {time}: {}",
        event.id,
        day.format("%a %Y-%m-%d"),
        title(&event.title)
    )
}

/// One of the desktop's calendars' events as a calendar line: its day (the agent can only answer
/// which day something is on if it is told), no Isle id (the agent's calendar actions cannot act on
/// it), and where it came from said.
fn desktop_line(day: NaiveDate, event: &Desktop) -> String {
    let time = match (&event.start, &event.end) {
        (Some(start), Some(end)) => format!("{start}-{end}"),
        (Some(start), None) => start.clone(),
        (None, _) => "all day".into(),
    };
    format!(
        "{}, {time}: {} — from a desktop calendar (GNOME)",
        day.format("%a %Y-%m-%d"),
        title(&event.title)
    )
}

/// Writes a heading and up to [`MAX_ITEMS`] items, or "none".
fn list(text: &mut String, heading: &str, items: impl Iterator<Item = String>) {
    list_capped(text, heading, MAX_ITEMS, items);
}

/// The same, with its own cut: what the calendar uses.
fn list_capped(
    text: &mut String,
    heading: &str,
    most: usize,
    items: impl Iterator<Item = String>,
) {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        let _ = writeln!(text, "{heading}: none");
        return;
    }
    let _ = writeln!(text, "{heading}:");
    for item in items.iter().take(most) {
        let _ = writeln!(text, "- {item}");
    }
    if items.len() > most {
        let _ = writeln!(text, "- and {} more", items.len() - most);
    }
}

/// One line, cut to [`MAX_TITLE_CHARS`].
fn title(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(MAX_TITLE_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn todo(id: u64, title: &str, done: bool) -> TodoItem {
        TodoItem {
            id,
            title: title.into(),
            done,
            done_at_ms: None,
        }
    }

    fn event(id: u64, date: &str, start: Option<&str>, end: Option<&str>, title: &str) -> Event {
        Event {
            id,
            title: title.into(),
            date: date.into(),
            start: start.map(Into::into),
            end: end.map(Into::into),
            send: None,
        }
    }

    /// Wednesday 2026-10-21 10:00 in UTC+7.
    fn now() -> DateTime<FixedOffset> {
        let zone = FixedOffset::east_opt(7 * 3600).unwrap();
        zone.with_ymd_and_hms(2026, 10, 21, 10, 0, 0).unwrap()
    }

    fn desktop(dbg: &str, date: &str, start: Option<&str>, end: Option<&str>) -> Desktop {
        Desktop {
            title: dbg.into(),
            date: date.into(),
            start: start.map(Into::into),
            end: end.map(Into::into),
        }
    }

    #[test]
    fn says_none_for_empty_lists() {
        assert_eq!(
            summary(&[], &[], &[], &now(), &[], None),
            "Reminders (your to-dos on Isle's Reminders page): none\n\
             Calendar, 2026-10-21 to 2026-11-03 (list only the days the person asked about): none\n\
             Timed reminders (alerts Isle sets itself): none"
        );
    }

    #[test]
    fn lists_open_todos_the_coming_week_and_reminders_in_local_time() {
        let todos = [todo(1, "Buy milk", false), todo(2, "Done already", true)];
        let events = [
            event(1, "2026-10-20", None, None, "Yesterday"),
            event(2, "2026-10-21", Some("09:00"), Some("09:15"), "Standup"),
            event(3, "2026-10-21", None, None, "Holiday"),
            event(4, "2026-10-27", Some("20:00"), None, "Dinner"),
            event(5, "2026-11-10", None, None, "Too far"),
        ];
        // 2026-10-21 07:30 UTC is 14:30 in UTC+7.
        let due = now().timestamp_millis() + 4 * 3600 * 1000 + 30 * 60 * 1000;
        let reminders = [Reminder {
            id: 1,
            text: "Tea\nnow".into(),
            due_ms: due,
        }];
        assert_eq!(
            summary(&todos, &events, &reminders, &now(), &[], None),
            "Reminders (your to-dos on Isle's Reminders page):\n\
             - [#1] Buy milk\n\
             Calendar, 2026-10-21 to 2026-11-03 (list only the days the person asked about):\n\
             - [#3] Wed 2026-10-21, all day: Holiday\n\
             - [#2] Wed 2026-10-21, 09:00-09:15: Standup\n\
             - [#4] Tue 2026-10-27, 20:00: Dinner\n\
             Timed reminders (alerts Isle sets itself):\n\
             - [#1] Wed 2026-10-21 14:30: Tea now"
        );
    }

    #[test]
    fn the_desktop_calendars_events_come_with_isles_own_once() {
        let events = [event(
            1,
            "2026-10-21",
            Some("09:00"),
            Some("09:15"),
            "Standup",
        )];
        let desktop = [
            desktop("M05W03", "2026-10-21", Some("20:00"), Some("22:00")),
            desktop("Standup", "2026-10-21", Some("09:00"), Some("09:15")),
            desktop("Too far", "2026-11-20", None, None),
        ];
        assert_eq!(
            summary(&[], &events, &[], &now(), &desktop, None),
            "Reminders (your to-dos on Isle's Reminders page): none\n\
             Calendar, 2026-10-21 to 2026-11-03 (list only the days the person asked about):\n\
             - [#1] Wed 2026-10-21, 09:00-09:15: Standup\n\
             - Wed 2026-10-21, 20:00-22:00: M05W03 — from a desktop calendar (GNOME)\n\
             Timed reminders (alerts Isle sets itself): none"
        );
    }

    #[test]
    fn a_question_naming_a_day_lists_only_that_day() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
        let focus = asked_range("What's on my calendar today?", today);
        assert_eq!(
            focus,
            Some((today, today, format!("today, {today}")))
        );
        let events = [
            event(1, "2026-10-21", Some("20:00"), Some("22:00"), "Today"),
            event(2, "2026-10-22", Some("20:00"), Some("22:00"), "Tomorrow"),
        ];
        let text = summary(&[], &events, &[], &now(), &[], focus);
        assert!(text.contains("- [#1] Wed 2026-10-21, 20:00-22:00: Today"));
        assert!(!text.contains("Tomorrow"));
        assert_eq!(
            asked_range("what about next week", today),
            Some((
                NaiveDate::from_ymd_opt(2026, 10, 26).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
                "next week, 2026-10-26 to 2026-11-01".into()
            ))
        );
    }

    #[test]
    fn caps_long_lists_and_titles() {
        let todos: Vec<TodoItem> = (0..20).map(|id| todo(id, "x", false)).collect();
        let text = summary(&todos, &[], &[], &now(), &[], None);
        assert_eq!(text.matches("] x\n").count(), MAX_ITEMS);
        assert!(text.contains("- and 5 more\n"));
        assert_eq!(title(&"a".repeat(150)), format!("{}…", "a".repeat(100)));
    }
}
