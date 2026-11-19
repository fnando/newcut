//! Audio quality: of the albums you have (from their files) and of the
//! folders slskd finds, to tell when one is an upgrade of the other.
//!
//! Qualities fall in [`Tier`]s: lossy, lossy at 320 kbps, lossless (16-bit,
//! as on CDs) and hi-res (24-bit lossless). An album is as good as its worst
//! file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Lossless audio file extensions (lowercase).
pub const LOSSLESS: [&str; 8] = ["flac", "alac", "wav", "aif", "aiff", "ape", "wv", "dsf"];

/// A quality level, worst first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum Tier {
    /// Lossy, below 320 kbps (not a target: everything is at least that).
    #[serde(skip)]
    Lossy,
    /// Lossy at 320 kbps.
    #[serde(rename = "320")]
    R320,
    /// Lossless, 16-bit (as on CDs).
    #[serde(rename = "lossless")]
    Lossless,
    /// Lossless, 24-bit.
    #[serde(rename = "hi-res")]
    HiRes,
}

impl Tier {
    /// As the config says it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Tier::Lossy => "lossy",
            Tier::R320 => "320",
            Tier::Lossless => "lossless",
            Tier::HiRes => "hi-res",
        }
    }
}

/// A file's (or a folder's, or an album's) quality.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Quality {
    /// `FLAC`, `MP3`, …
    pub format: String,
    pub lossless: bool,
    pub bit_depth: Option<u32>,
    /// Hz.
    pub sample_rate: Option<u32>,
    /// kbps.
    pub bitrate: Option<u32>,
}

impl Quality {
    #[must_use]
    pub fn tier(&self) -> Tier {
        if self.lossless {
            if self.bit_depth.is_some_and(|d| d >= 24) {
                Tier::HiRes
            } else {
                Tier::Lossless
            }
        } else if self.bitrate.is_some_and(|b| b >= 320) {
            Tier::R320
        } else {
            Tier::Lossy
        }
    }

    /// Orders qualities, within tiers and across them.
    #[must_use]
    pub fn score(&self) -> u32 {
        if self.lossless {
            let depth = self.bit_depth.unwrap_or(16);
            let khz = self.sample_rate.unwrap_or(44_100) / 1000;
            10_000 + depth * 1000 + khz
        } else {
            self.bitrate.unwrap_or(0).min(9_999)
        }
    }

    fn rank(&self) -> (Tier, u32) {
        (self.tier(), self.score())
    }

    /// How this compares with `other`.
    #[must_use]
    pub fn cmp_rank(&self, other: &Quality) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }

    /// Whether this is good enough for `target`.
    #[must_use]
    pub fn meets(&self, target: Tier) -> bool {
        self.tier() >= target
    }

    /// Whether this is worth replacing `current` with: a better tier, or
    /// (both lossy) a higher bitrate. Lossless at another sample rate isn't.
    #[must_use]
    pub fn better_than(&self, current: &Quality) -> bool {
        match self.tier().cmp(&current.tier()) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => {
                self.tier() <= Tier::R320
                    && self.bitrate.unwrap_or(0) > current.bitrate.unwrap_or(0)
            }
        }
    }

    /// "MP3 192 kbps", "FLAC 24-bit/96 kHz", "FLAC".
    #[must_use]
    pub fn label(&self) -> String {
        let detail = if self.lossless {
            match (self.bit_depth, self.sample_rate) {
                (Some(depth), Some(rate)) => Some(format!("{depth}-bit/{}", khz(rate))),
                (Some(depth), None) => Some(format!("{depth}-bit")),
                (None, Some(rate)) => Some(khz(rate)),
                (None, None) => None,
            }
        } else {
            self.bitrate.map(|b| format!("{b} kbps"))
        };
        match (self.format.is_empty(), detail) {
            (false, Some(detail)) => format!("{} {detail}", self.format),
            (true, Some(detail)) => detail,
            (false, None) => self.format.clone(),
            (true, None) => "unknown".into(),
        }
    }

    /// The worst of `qualities` (an album's, from its files).
    #[must_use]
    pub fn worst<'a>(qualities: impl IntoIterator<Item = &'a Quality>) -> Option<Quality> {
        qualities.into_iter().min_by_key(|q| q.rank()).cloned()
    }
}

fn khz(rate: u32) -> String {
    let text = format!("{:.1}", f64::from(rate) / 1000.0);
    format!("{} kHz", text.strip_suffix(".0").unwrap_or(&text))
}

/// Whether `extension` (any case) is a lossless format's.
#[must_use]
pub fn lossless_extension(extension: &str) -> bool {
    LOSSLESS.contains(&extension.to_ascii_lowercase().as_str())
}

/// Which of `candidates` (best first, as slskd's order goes) to replace
/// `current` with, aiming for `target`: the one closest to `target` that
/// meets it, or else the best one better than `current`.
#[must_use]
pub fn pick<T>(candidates: Vec<(T, Quality)>, current: &Quality, target: Tier) -> Option<T> {
    let better: Vec<(T, Quality)> = candidates
        .into_iter()
        .filter(|(_, q)| q.better_than(current))
        .collect();
    let meeting = better
        .iter()
        .enumerate()
        .filter(|(_, (_, q))| q.meets(target))
        .min_by_key(|(i, (_, q))| (q.rank(), *i))
        .map(|(i, _)| i);
    let index = meeting.or_else(|| {
        better
            .iter()
            .enumerate()
            // The best; of equals, the first.
            .max_by_key(|(i, (_, q))| (q.rank(), std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
    })?;
    better.into_iter().nth(index).map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lossy(kbps: u32) -> Quality {
        Quality {
            format: "MP3".into(),
            bitrate: Some(kbps),
            ..Quality::default()
        }
    }

    fn lossless(depth: u32, rate: u32) -> Quality {
        Quality {
            format: "FLAC".into(),
            lossless: true,
            bit_depth: Some(depth),
            sample_rate: Some(rate),
            ..Quality::default()
        }
    }

    #[test]
    fn tiers_and_labels() {
        assert_eq!(lossy(192).tier(), Tier::Lossy);
        assert_eq!(lossy(320).tier(), Tier::R320);
        assert_eq!(lossless(16, 44_100).tier(), Tier::Lossless);
        assert_eq!(lossless(24, 96_000).tier(), Tier::HiRes);
        assert_eq!(lossy(192).label(), "MP3 192 kbps");
        assert_eq!(lossless(16, 44_100).label(), "FLAC 16-bit/44.1 kHz");
        assert_eq!(lossless(24, 96_000).label(), "FLAC 24-bit/96 kHz");
        let worst = Quality::worst(&[lossless(16, 44_100), lossy(320), lossy(256)]).unwrap();
        assert_eq!(worst, lossy(256));
    }

    #[test]
    fn what_counts_as_better() {
        assert!(lossy(256).better_than(&lossy(192)));
        assert!(lossless(16, 44_100).better_than(&lossy(320)));
        assert!(lossless(24, 48_000).better_than(&lossless(16, 44_100)));
        assert!(!lossless(16, 48_000).better_than(&lossless(16, 44_100)));
        assert!(!lossy(320).better_than(&lossless(16, 44_100)));
        assert!(!lossy(192).better_than(&lossy(192)));
    }

    #[test]
    fn picks_the_closest_to_the_target_or_the_best() {
        let current = lossy(192);
        let found = vec![
            ("hires", lossless(24, 96_000)),
            ("v0", lossy(245)),
            ("cd", lossless(16, 44_100)),
            ("cd2", lossless(16, 44_100)),
        ];
        assert_eq!(pick(found.clone(), &current, Tier::Lossless), Some("cd"));
        assert_eq!(pick(found.clone(), &current, Tier::HiRes), Some("hires"));
        assert_eq!(pick(found, &current, Tier::R320), Some("cd"));
        // Nothing meets it: the best there is.
        let found = vec![("a", lossy(256)), ("b", lossy(320)), ("c", lossy(128))];
        assert_eq!(pick(found, &current, Tier::Lossless), Some("b"));
        // Nothing better.
        let found = vec![("a", lossy(192)), ("b", lossy(128))];
        assert_eq!(pick(found, &current, Tier::Lossless), None::<&str>);
    }

    #[test]
    fn targets_are_written_as_in_the_config() {
        #[derive(Deserialize)]
        struct T {
            t: Tier,
        }
        let t = |s: &str| toml::from_str::<T>(&format!("t = {s:?}")).map(|t| t.t);
        assert_eq!(t("320").unwrap(), Tier::R320);
        assert_eq!(t("lossless").unwrap(), Tier::Lossless);
        assert_eq!(t("hi-res").unwrap(), Tier::HiRes);
        assert!(t("lossy").is_err());
    }
}
