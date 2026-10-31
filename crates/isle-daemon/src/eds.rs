// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! Evolution Data Server, where GNOME keeps the desktop's calendars (its own and those of Online Accounts
//! such as Google): opening one for isled, and the calendars a new event can go to.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use log::{debug, warn};
use serde::Serialize;
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// Each calendar service EDS has had, newest first.
const FACTORIES: [&str; 2] = [
    "org.gnome.evolution.dataserver.Calendar8",
    "org.gnome.evolution.dataserver.Calendar7",
];
const FACTORY_PATH: &str = "/org/gnome/evolution/dataserver/CalendarFactory";
const FACTORY: &str = "org.gnome.evolution.dataserver.CalendarFactory";
pub const CALENDAR: &str = "org.gnome.evolution.dataserver.Calendar";
const SOURCES: &str = "org.gnome.evolution.dataserver.Sources5";
const SOURCES_PATH: &str = "/org/gnome/evolution/dataserver/SourceManager";
const SOURCE: &str = "org.gnome.evolution.dataserver.Source";
/// Calendars no event can be added to (birthdays from contacts, the weather, a subscribed web
/// calendar), and EDS's own one on this computer, which Isle's own calendar stands for.
const LEFT_OUT: [&str; 4] = ["contacts", "weather", "webcal", "local"];
/// Each step (opening a calendar can reach its server) gives up after this.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Adding an event waits this long for its server: given up on sooner, it may still arrive, and the
/// event kept in Isle too would show twice.
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);
/// A calendar's server that doesn't take a connection within this counts as out of reach.
const REACH_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether each calendar takes new events (a subscribed one, such as holidays, doesn't), by uid, as EDS
/// said, and when: yes holds, read only is asked again after `READ_ONLY_FOR`.
static WRITABLE: LazyLock<Mutex<HashMap<String, (bool, Instant)>>> = LazyLock::new(Mutex::default);
/// How long a calendar found read only is left out before EDS is asked again: not long, as one still
/// signing in (just after login) can read as read only for a while.
const READ_ONLY_FOR: Duration = Duration::from_secs(600);
/// GNOME's proxy setting as last read, and when; read again after [`PROXY_FOR`].
static PROXIED: Mutex<Option<(bool, Instant)>> = Mutex::new(None);
const PROXY_FOR: Duration = Duration::from_secs(60);
/// Just opened, a calendar reads as read only until it has reached its server: asked this often, this
/// many times, before that is believed.
const SETTLE_STEP: Duration = Duration::from_millis(500);
const SETTLE_TRIES: u32 = 6;

/// Whether an event got to a calendar.
pub enum Created {
    /// There, or already there from an earlier try.
    Yes,
    /// Out of reach (no network, or its server down): not sent, so it can be tried again later.
    Offline,
    /// Turned down (read only, signed out): it can go elsewhere.
    No(zbus::Error),
    /// No answer in time after it was sent: it most likely got there, so it goes nowhere else.
    Unsure,
}

/// A calendar a new event can go to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub uid: String,
    pub name: String,
    /// The account it belongs to, e.g. "someone@gmail.com".
    pub account: String,
    /// "#rrggbb", or "" when it has none.
    pub color: String,
    /// Its server's host and port, to tell whether it can be reached; none for one on this computer.
    #[serde(skip)]
    pub server: Option<(String, u16)>,
}

/// `step`, or an error once it has taken `TIMEOUT`.
pub async fn timed<T>(step: impl Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    tokio::time::timeout(TIMEOUT, step)
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("timed out".into())))
}

/// Opens the calendar `source` for isled: its object path and bus name.
pub async fn open_calendar(bus: &Connection, source: &str) -> zbus::Result<(String, String)> {
    let mut failure = None;
    for factory in FACTORIES {
        match bus
            .call_method(
                Some(factory),
                FACTORY_PATH,
                Some(FACTORY),
                "OpenCalendar",
                &(source,),
            )
            .await
        {
            Ok(reply) => return reply.body().deserialize::<(String, String)>(),
            Err(err) => failure = Some(err),
        }
    }
    Err(failure.unwrap_or(zbus::Error::Unsupported))
}

/// Lets go of isled's hold on an opened calendar, however using it went; GNOME Shell keeps its own.
pub async fn close_calendar(bus: &Connection, path: &str, name: &str, source: &str) {
    if let Err(err) = timed(bus.call_method(Some(name), path, Some(CALENDAR), "Close", &())).await {
        debug!("could not close calendar {source}: {err}");
    }
}

/// Whether `target`'s server takes a connection now. EDS's own "online" isn't enough: with the network
/// up but the server out of reach, it keeps a new event to send later, and may never.
async fn reachable(target: &Target) -> bool {
    let Some((host, port)) = &target.server else {
        return true;
    };
    // Through a proxy, the server can't be tried directly: EDS's word is taken.
    if proxied().await {
        return true;
    }
    let connect = tokio::net::TcpStream::connect((host.as_str(), *port));
    matches!(
        tokio::time::timeout(REACH_TIMEOUT, connect).await,
        Ok(Ok(_))
    )
}

/// Whether the desktop goes online through a proxy: the login's, or GNOME's own setting (asked at most
/// once every [`PROXY_FOR`]).
async fn proxied() -> bool {
    const VARS: [&str; 4] = ["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"];
    if VARS
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
    {
        return true;
    }
    let known = *PROXIED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((proxied, at)) = known
        && at.elapsed() < PROXY_FOR
    {
        return proxied;
    }
    let mode = tokio::process::Command::new("gsettings")
        .args(["get", "org.gnome.system.proxy", "mode"])
        .kill_on_drop(true)
        .output();
    let proxied = match tokio::time::timeout(REACH_TIMEOUT, mode).await {
        Ok(Ok(out)) => {
            let mode = String::from_utf8_lossy(&out.stdout);
            matches!(mode.trim(), "'manual'" | "'auto'")
        }
        _ => false,
    };
    *PROXIED.lock().unwrap_or_else(PoisonError::into_inner) = Some((proxied, Instant::now()));
    proxied
}

/// Adds `ical` (one VEVENT) to `target`, unless its server is out of reach; `sending` is called as
/// the server is handed it.
pub async fn create_event(
    bus: &Connection,
    target: &Target,
    ical: &str,
    sending: impl FnOnce(),
) -> Created {
    if !reachable(target).await {
        return Created::Offline;
    }
    let source = target.uid.as_str();
    let (path, name) = match timed(open_calendar(bus, source)).await {
        Ok(opened) => opened,
        Err(err) => return Created::No(err),
    };
    let (path, name) = (path.as_str(), name.as_str());
    let created = match timed(bus.call_method(Some(name), path, Some(CALENDAR), "Open", &())).await
    {
        Err(err) => Created::No(err),
        // Offline, EDS would only queue it, to send who knows when: it is kept in Isle instead.
        Ok(_)
            if flag(bus, name, path, "Online")
                .await
                .is_ok_and(|online| !online) =>
        {
            Created::Offline
        }
        Ok(_) => {
            sending();
            let objects = (vec![ical], 0u32);
            let create =
                bus.call_method(Some(name), path, Some(CALENDAR), "CreateObjects", &objects);
            match tokio::time::timeout(CREATE_TIMEOUT, create).await {
                Ok(Ok(_)) => Created::Yes,
                // Sent before, with no word back: it got there.
                Ok(Err(err)) if is_cal_error(&err, CalError::AlreadyExists) => Created::Yes,
                Ok(Err(err)) => Created::No(err),
                Err(_) => Created::Unsure,
            }
        }
    };
    close_calendar(bus, path, name, source).await;
    created
}

/// Takes event `uid` out of calendar `source`, if it is there: one sent while it was deleted in Isle.
pub async fn remove_event(bus: &Connection, source: &str, uid: &str) {
    let removed = async {
        let (path, name) = timed(open_calendar(bus, source)).await?;
        let (path, name) = (path.as_str(), name.as_str());
        let calendar = Some(CALENDAR);
        let result = async {
            timed(bus.call_method(Some(name), path, calendar, "Open", &())).await?;
            let args = (vec![(uid, "")], "all", 0u32);
            timed(bus.call_method(Some(name), path, calendar, "RemoveObjects", &args)).await
        }
        .await;
        close_calendar(bus, path, name, source).await;
        result.map(drop)
    };
    match removed.await {
        Ok(()) => {}
        Err(err) if is_cal_error(&err, CalError::NotFound) => {}
        Err(err) => warn!("could not take event {uid} out of calendar {source}: {err}"),
    }
}

/// Calendar errors isled tells apart (`E_CAL_CLIENT_ERROR`'s codes).
#[derive(Clone, Copy)]
enum CalError {
    NotFound = 1,
    AlreadyExists = 4,
}

/// Whether `err` is EDS's calendar error `code`. It reaches D-Bus by its code alone
/// (`…_e_2dcal_2dclient_2derror_2dquark.Code4`), its words in the user's language.
fn is_cal_error(err: &zbus::Error, code: CalError) -> bool {
    matches!(err, zbus::Error::MethodError(name, ..) if cal_error_named(name.as_str(), code))
}

/// Whether the D-Bus error `name` is EDS's calendar error `code` (see `is_cal_error`).
fn cal_error_named(name: &str, code: CalError) -> bool {
    let named = match code {
        CalError::NotFound => "ObjectNotFound",
        CalError::AlreadyExists => "ObjectIdAlreadyExists",
    };
    name.ends_with(&format!(
        "_e_2dcal_2dclient_2derror_2dquark.Code{}",
        code as u32
    )) || name.ends_with(named)
}

/// A yes-or-no property of the opened calendar at `path` ("Online", "Writable").
async fn flag(bus: &Connection, name: &str, path: &str, property: &str) -> zbus::Result<bool> {
    let properties = Some("org.freedesktop.DBus.Properties");
    let reply =
        timed(bus.call_method(Some(name), path, properties, "Get", &(CALENDAR, property))).await?;
    let value: OwnedValue = reply.body().deserialize()?;
    Ok(bool::try_from(&*value)?)
}

/// The desktop's calendars a new event can go to, to choose from, by account and name: read only ones
/// left out (asking may take a few seconds). `None` when EDS couldn't be asked (not running yet).
pub async fn calendars(bus: &Connection) -> Option<Vec<Target>> {
    let found = accounts_calendars(bus).await?;
    let writable = join_all(found.iter().map(|target| writable(bus, target))).await;
    let found = found.into_iter().zip(writable);
    Some(
        found
            .filter_map(|(target, writable)| writable.then_some(target))
            .collect(),
    )
}

/// The desktop's calendars, read only ones too, at once: the one picked turns an event down itself if so.
/// `None` when EDS couldn't be asked (not running yet), rather than no calendars.
pub async fn accounts_calendars(bus: &Connection) -> Option<Vec<Target>> {
    match timed(sources(bus)).await {
        Ok(sources) => Some(targets(&sources)),
        Err(err) => {
            debug!("could not list the desktop's calendars: {err}");
            None
        }
    }
}

/// Whether EDS has said `uid` only reads (a subscribed holiday calendar), as isled knows now: such a
/// one is passed over at once, never asked again here.
pub fn known_read_only(uid: &str) -> bool {
    let known = WRITABLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(uid)
        .copied();
    known.is_some_and(|(writable, at)| !writable && at.elapsed() < READ_ONLY_FOR)
}

/// Every desktop calendar's colour ("#rrggbb"), by uid, read only ones too; none without EDS.
pub async fn colors(bus: &Connection) -> HashMap<String, String> {
    let sources = timed(sources(bus)).await.unwrap_or_default();
    sources
        .iter()
        .filter_map(|(uid, data)| {
            let file = key_file(data);
            let color = value(&file, "Calendar", "Color");
            (!color.is_empty()).then(|| (uid.clone(), color.to_owned()))
        })
        .collect()
}

/// Whether `target` takes new events, as EDS says once open; one that can't be asked is taken to. Read only
/// is believed only from a server that answers: out of reach, EDS calls every calendar read only.
async fn writable(bus: &Connection, target: &Target) -> bool {
    let uid = target.uid.as_str();
    let known = WRITABLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(uid)
        .copied();
    if let Some((writable, at)) = known
        && (writable || at.elapsed() < READ_ONLY_FOR)
    {
        return writable;
    }
    let Ok((path, name)) = timed(open_calendar(bus, uid)).await else {
        return true;
    };
    let mut answer = flag(bus, &name, &path, "Writable").await.ok();
    // Not open yet, a calendar reads as read only until it is.
    if answer == Some(false) {
        let open = bus.call_method(
            Some(name.as_str()),
            path.as_str(),
            Some(CALENDAR),
            "Open",
            &(),
        );
        answer = None;
        if reachable(target).await && timed(open).await.is_ok() {
            for _ in 0..SETTLE_TRIES {
                answer = flag(bus, &name, &path, "Writable").await.ok();
                if answer != Some(false) {
                    break;
                }
                tokio::time::sleep(SETTLE_STEP).await;
            }
        }
    }
    close_calendar(bus, &path, &name, uid).await;
    if let Some(answer) = answer {
        WRITABLE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(uid.to_owned(), (answer, Instant::now()));
    }
    answer.unwrap_or(true)
}

/// Every EDS source: its uid and its key file.
pub async fn sources(bus: &Connection) -> zbus::Result<HashMap<String, String>> {
    type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
    let objects: Objects = bus
        .call_method(
            Some(SOURCES),
            SOURCES_PATH,
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await?
        .body()
        .deserialize()?;
    let text = |value: Option<&OwnedValue>| {
        value.and_then(|value| <&str>::try_from(&**value).ok().map(str::to_owned))
    };
    Ok(objects
        .into_values()
        .filter_map(|interfaces| {
            let source = interfaces.get(SOURCE)?;
            Some((text(source.get("UID"))?, text(source.get("Data"))?))
        })
        .collect())
}

/// A key file's groups and their keys; translated keys ("DisplayName[de]") are left out.
pub type KeyFile<'a> = HashMap<&'a str, HashMap<&'a str, &'a str>>;

pub fn key_file(data: &str) -> KeyFile<'_> {
    let mut groups = KeyFile::new();
    let mut group = None;
    for line in data.lines().map(str::trim) {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            group = Some(name);
            groups.entry(name).or_default();
        } else if let (Some(group), Some((key, value))) = (group, line.split_once('='))
            && !key.contains('[')
            && !line.starts_with('#')
        {
            groups
                .entry(group)
                .or_default()
                .insert(key.trim(), value.trim());
        }
    }
    groups
}

/// The value of `key` in `group`, or "".
pub fn value<'a>(file: &KeyFile<'a>, group: &str, key: &str) -> &'a str {
    file.get(group)
        .and_then(|group| group.get(key))
        .copied()
        .unwrap_or("")
}

/// The calendars among `sources` (uid → key file) that take new events, each with its account.
fn targets(sources: &HashMap<String, String>) -> Vec<Target> {
    let files: HashMap<&str, KeyFile> = sources
        .iter()
        .map(|(uid, data)| (uid.as_str(), key_file(data)))
        .collect();
    let mut found: Vec<Target> = files
        .iter()
        .filter_map(|(uid, file)| {
            let backend = value(file, "Calendar", "BackendName");
            let enabled = |group| value(file, group, "Enabled") != "false";
            if !file.contains_key("Calendar")
                || LEFT_OUT.contains(&backend)
                || !enabled("Data Source")
                || !enabled("Calendar")
            {
                return None;
            }
            let name = value(file, "Data Source", "DisplayName");
            // An account's calendars hang from its collection, itself turned off with the account's
            // calendars in Online Accounts.
            let parent = files.get(value(file, "Data Source", "Parent"));
            if parent.is_some_and(|parent| {
                value(parent, "Data Source", "Enabled") == "false"
                    || value(parent, "Collection", "CalendarEnabled") == "false"
            }) {
                return None;
            }
            let account = [
                parent.map(|parent| value(parent, "Collection", "Identity")),
                parent.map(|parent| value(parent, "Data Source", "DisplayName")),
                Some(value(file, "Authentication", "User")),
            ]
            .into_iter()
            .flatten()
            .find(|account| !account.is_empty())
            .unwrap_or(name);
            let host = value(file, "Authentication", "Host");
            let port = value(file, "Authentication", "Port").parse().unwrap_or(0);
            Some(Target {
                uid: (*uid).to_owned(),
                name: name.to_owned(),
                account: account.to_owned(),
                color: value(file, "Calendar", "Color").to_owned(),
                server: (!host.is_empty())
                    .then(|| (host.to_owned(), if port == 0 { 443 } else { port })),
            })
        })
        .collect();
    // By account, each one's own calendar (named after it) first: the one events go to unless picked.
    found.sort_by_key(|target| {
        (
            target.account.clone(),
            target.name != target.account,
            target.name.clone(),
            target.uid.clone(),
        )
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_errors_are_told_by_their_code() {
        // As EDS 3.52 (GNOME 46) answers.
        let quark = "org.gtk.GDBus.UnmappedGError.Quark._e_2dcal_2dclient_2derror_2dquark";
        let (exists, not_found) = (format!("{quark}.Code4"), format!("{quark}.Code1"));
        assert!(cal_error_named(&exists, CalError::AlreadyExists));
        assert!(!cal_error_named(&exists, CalError::NotFound));
        assert!(cal_error_named(&not_found, CalError::NotFound));
        assert!(!cal_error_named(
            &format!("{quark}.Code14"),
            CalError::AlreadyExists
        ));
        let named = "org.gnome.evolution.dataserver.Calendar.ObjectIdAlreadyExists";
        assert!(cal_error_named(named, CalError::AlreadyExists));
        assert!(!cal_error_named(
            "org.freedesktop.DBus.Error.Failed",
            CalError::NotFound
        ));
    }

    const GOOGLE_ACCOUNT: &str = "[Data Source]\nDisplayName=someone@gmail.com\nEnabled=true\nParent=\n\
        [Collection]\nBackendName=google\nCalendarEnabled=true\nIdentity=someone@gmail.com\n";
    const GOOGLE_CALENDAR: &str = "[Data Source]\nDisplayName=someone@gmail.com\nEnabled=true\nParent=acct\n\
        [Authentication]\nHost=apidata.googleusercontent.com\nPort=443\nUser=someone@gmail.com\n\
        [Calendar]\nBackendName=caldav\nColor=#9fe1e7\nSelected=true\n";
    const BIRTHDAYS: &str = "[Data Source]\nDisplayName[de]=Geburtstage\nDisplayName=Birthdays\nEnabled=true\n\
        [Calendar]\nBackendName=contacts\n";
    const PERSONAL: &str =
        "[Data Source]\nDisplayName=Personal\nEnabled=true\n[Calendar]\nBackendName=local\n";

    fn sources(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(uid, data)| ((*uid).to_owned(), (*data).to_owned()))
            .collect()
    }

    #[test]
    fn an_accounts_calendar_is_offered_with_its_account() {
        let found = targets(&sources(&[
            ("acct", GOOGLE_ACCOUNT),
            ("cal", GOOGLE_CALENDAR),
            ("birthdays", BIRTHDAYS),
            ("system-calendar", PERSONAL),
        ]));
        assert_eq!(
            found,
            [Target {
                uid: "cal".into(),
                name: "someone@gmail.com".into(),
                account: "someone@gmail.com".into(),
                color: "#9fe1e7".into(),
                server: Some(("apidata.googleusercontent.com".into(), 443)),
            }]
        );
    }

    #[test]
    fn calendars_turned_off_are_left_out() {
        let off = GOOGLE_ACCOUNT.replace("CalendarEnabled=true", "CalendarEnabled=false");
        assert!(targets(&sources(&[("acct", &off), ("cal", GOOGLE_CALENDAR)])).is_empty());
        let disabled = GOOGLE_CALENDAR.replace("Enabled=true", "Enabled=false");
        assert!(targets(&sources(&[("acct", GOOGLE_ACCOUNT), ("cal", &disabled)])).is_empty());
    }

    #[test]
    fn several_accounts_come_in_order() {
        let work = GOOGLE_ACCOUNT.replace("someone@gmail.com", "me@work.com");
        let work_calendar = GOOGLE_CALENDAR
            .replace("Parent=acct", "Parent=work")
            .replace("someone@gmail.com", "Team");
        let found = targets(&sources(&[
            ("work", &work),
            ("team", &work_calendar),
            ("acct", GOOGLE_ACCOUNT),
            ("cal", GOOGLE_CALENDAR),
        ]));
        let order: Vec<_> = found
            .iter()
            .map(|t| (t.account.as_str(), t.name.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("me@work.com", "Team"),
                ("someone@gmail.com", "someone@gmail.com")
            ]
        );
    }

    #[test]
    fn an_accounts_own_calendar_comes_before_its_others() {
        let holidays = GOOGLE_CALENDAR.replace(
            "DisplayName=someone@gmail.com",
            "DisplayName=Holidays in Indonesia",
        );
        let found = targets(&sources(&[
            ("acct", GOOGLE_ACCOUNT),
            ("cal", GOOGLE_CALENDAR),
            ("hol", &holidays),
        ]));
        let order: Vec<_> = found.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(order, ["someone@gmail.com", "Holidays in Indonesia"]);
    }

    #[tokio::test]
    async fn a_server_is_reachable_only_while_it_listens() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = Target {
            uid: "cal".into(),
            name: "Calendar".into(),
            account: "me".into(),
            color: String::new(),
            server: Some(("127.0.0.1".into(), port)),
        };
        assert!(reachable(&target).await);
        drop(listener);
        assert!(!reachable(&target).await);
        assert!(
            reachable(&Target {
                server: None,
                ..target
            })
            .await
        );
    }

    #[test]
    fn translated_names_are_not_taken_for_the_name() {
        let file = key_file(BIRTHDAYS);
        assert_eq!(file["Data Source"]["DisplayName"], "Birthdays");
    }
}
