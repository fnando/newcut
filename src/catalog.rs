//! What each release group is to you: owned, missing, coming, ignored, or
//! outside the discography's types; and whether it's new. Pure: today and
//! the config are passed in.

use std::time::Duration;

use jiff::civil::Date;

use crate::config::DiscographyConfig;
use crate::state::library::ReleaseGroup;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Owned,
    Missing,
    Upcoming,
    Ignored,
    /// Not one of `discography`'s types, or a title it excludes (and not
    /// owned).
    Other,
}

impl Status {
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Status::Owned => "owned",
            Status::Missing => "missing",
            Status::Upcoming => "upcoming",
            Status::Ignored => "ignored",
            Status::Other => "other",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Status::Owned => "Owned",
            Status::Missing => "Missing",
            Status::Upcoming => "Upcoming",
            Status::Ignored => "Ignored",
            Status::Other => "Not counted",
        }
    }
}

/// A release group, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub group: ReleaseGroup,
    pub status: Status,
    /// New and you don't have it (Missing or Upcoming): what New and the
    /// feeds list.
    pub is_new: bool,
    /// New, whether you have it or not (not ignored or left out).
    pub fresh: bool,
}

/// What `new` and `upcoming` are measured against.
#[derive(Debug, Clone, Copy)]
pub struct Today {
    pub date: Date,
    /// `schedule.new_window`.
    pub window: Duration,
}

impl Today {
    #[must_use]
    pub fn now(window: Duration) -> Self {
        Self {
            date: jiff::Zoned::now().date(),
            window,
        }
    }

    /// The earliest release date that's still new.
    fn since(self) -> Date {
        let days = i64::try_from(self.window.as_secs() / 86_400).unwrap_or(i64::MAX);
        self.date
            .checked_sub(jiff::Span::new().days(days.min(3_650_000)))
            .unwrap_or(Date::MIN)
    }

    /// Out within the window, or coming.
    fn recent(self, date: Option<&str>) -> bool {
        date.and_then(|d| parse_date(d, true))
            .is_some_and(|last| last >= self.since())
    }
}

/// Classify a release group.
#[must_use]
pub fn classify(group: ReleaseGroup, discography: &DiscographyConfig, today: Today) -> Entry {
    let counts = discography.counts_release(
        &group.title,
        group.primary_type.as_deref(),
        &group.secondary_types,
    );
    let upcoming = group
        .first_release_date
        .as_deref()
        .and_then(|d| parse_date(d, false))
        .is_some_and(|first| first > today.date);
    let status = if !group.owned_in.is_empty() {
        Status::Owned
    } else if group.ignored {
        Status::Ignored
    } else if !counts {
        Status::Other
    } else if upcoming {
        Status::Upcoming
    } else {
        Status::Missing
    };
    // Found after the artist's first check (a baseline), or by a fresh
    // releases source; and out within the window, or coming.
    let found_later = group.source != "musicbrainz"
        || group
            .artists
            .iter()
            .any(|(_, baseline)| baseline.is_none_or(|b| group.discovered_at > b));
    let fresh = !matches!(status, Status::Ignored | Status::Other)
        && found_later
        && today.recent(group.first_release_date.as_deref());
    Entry {
        is_new: fresh && matches!(status, Status::Missing | Status::Upcoming),
        group,
        status,
        fresh,
    }
}

/// `YYYY`, `YYYY-MM` or `YYYY-MM-DD` as a date: its first day, or with
/// `end`, its last.
#[must_use]
pub fn parse_date(text: &str, end: bool) -> Option<Date> {
    let mut parts = text.trim().splitn(3, '-');
    let year: i16 = parts.next()?.parse().ok()?;
    let month: Option<i8> = parts.next().map(str::parse).transpose().ok()?;
    let day: Option<i8> = parts.next().map(str::parse).transpose().ok()?;
    match (month, day) {
        (Some(m), Some(d)) => Date::new(year, m, d).ok(),
        (Some(m), None) => {
            let first = Date::new(year, m, 1).ok()?;
            Some(if end { first.last_of_month() } else { first })
        }
        (None, _) => Date::new(year, if end { 12 } else { 1 }, if end { 31 } else { 1 }).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn today() -> Today {
        Today {
            date: jiff::civil::date(2026, 11, 9),
            window: Duration::from_secs(60 * 86_400),
        }
    }

    fn group(date: &str, source: &str, discovered: &str) -> ReleaseGroup {
        ReleaseGroup {
            mbid: "g".into(),
            title: "T".into(),
            artist_credit: None,
            primary_type: Some("Album".into()),
            secondary_types: Vec::new(),
            first_release_date: Some(date.into()),
            source: source.into(),
            discovered_at: at(discovered),
            owned_in: Vec::new(),
            ignored: false,
            artists: vec![("a".into(), Some(at("2026-10-01T00:00:00Z")))],
        }
    }

    #[test]
    fn partial_dates() {
        assert_eq!(
            parse_date("2026", false),
            Some(jiff::civil::date(2026, 1, 1))
        );
        assert_eq!(
            parse_date("2026", true),
            Some(jiff::civil::date(2026, 12, 31))
        );
        assert_eq!(
            parse_date("2024-02", true),
            Some(jiff::civil::date(2024, 2, 29))
        );
        assert_eq!(
            parse_date("2026-11-09", true),
            Some(jiff::civil::date(2026, 11, 9))
        );
        assert_eq!(parse_date("", false), None);
        assert_eq!(parse_date("soon", false), None);
    }

    #[test]
    fn the_back_catalogue_is_never_new() {
        let d = DiscographyConfig::default();
        // Found by the first check (the baseline): not new, even if recent.
        let e = classify(
            group("2026-10-20", "musicbrainz", "2026-10-01T00:00:00Z"),
            &d,
            today(),
        );
        assert_eq!((e.status, e.is_new), (Status::Missing, false));
        // Found later, out within the window: new.
        let e = classify(
            group("2026-10-20", "musicbrainz", "2026-11-01T00:00:00Z"),
            &d,
            today(),
        );
        assert_eq!((e.status, e.is_new), (Status::Missing, true));
        // Found later, but old (just added to MusicBrainz): not new.
        let e = classify(
            group("2001", "musicbrainz", "2026-11-01T00:00:00Z"),
            &d,
            today(),
        );
        assert!(!e.is_new);
        // From fresh releases, before the artist's baseline: new.
        let e = classify(
            group("2026-10-20", "listenbrainz", "2026-09-01T00:00:00Z"),
            &d,
            today(),
        );
        assert!(e.is_new);
        // Coming: upcoming, and new.
        let e = classify(
            group("2027-01-15", "musicbrainz", "2026-11-01T00:00:00Z"),
            &d,
            today(),
        );
        assert_eq!((e.status, e.is_new), (Status::Upcoming, true));
    }

    #[test]
    fn owned_ignored_and_other_types_arent_missing() {
        let d = DiscographyConfig::default();
        let mut g = group("2026-10-20", "listenbrainz", "2026-11-01T00:00:00Z");
        g.owned_in = vec!["/music/x".into()];
        assert_eq!(classify(g.clone(), &d, today()).status, Status::Owned);
        let owned = classify(g.clone(), &d, today());
        assert!(!owned.is_new, "you have it");
        assert!(
            owned.fresh,
            "but it's new (for notifications of everything)"
        );
        g.owned_in.clear();
        g.ignored = true;
        let ignored = classify(g.clone(), &d, today());
        assert_eq!(ignored.status, Status::Ignored);
        assert!(!ignored.fresh && !ignored.is_new);
        g.ignored = false;
        g.secondary_types = vec!["Live".into()];
        let e = classify(g.clone(), &d, today());
        assert_eq!((e.status, e.is_new, e.fresh), (Status::Other, false, false));
        g.owned_in = vec!["/music/live".into()];
        assert_eq!(
            classify(g, &d, today()).status,
            Status::Owned,
            "owned is owned"
        );
    }
}
