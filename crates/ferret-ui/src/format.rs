//! Display formatting: sizes, dates and natural name order.
//!
//! The layout of a date is Ferret's own — `25 Aug 2026 at 12:34`, thirty lines
//! of Howard Hinnant's civil-from-days, no library needed. Working out *which*
//! civil date an instant falls on is not, and that is what `jiff` is here for
//! (D-39): the answer depends on the user's zone and on whether that instant
//! was in daylight saving, and the rules change several times a year.
//!
//! Tabular alignment comes from the caller's font styling, not from padding
//! here.

use jiff::{Timestamp, tz::TimeZone};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// Widest a formatted size gets: `1023.9 KB` and a terminator's worth of slack.
const SIZE_CAPACITY: usize = 12;
/// Widest a formatted date gets: `31 Dec 2026 at 23:59` is 20.
const DATE_CAPACITY: usize = 24;

/// Human size: `512 B`, `12 KB`, `1.5 MB`, `174 GB`. One decimal only when
/// the value is fractional and small, matching what Finder shows.
pub fn format_size(len: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    // Written into a pre-sized buffer rather than `format!`ed. `format!`
    // guesses a capacity from the format string and reallocates when it is
    // short, which it is for both of these — two allocations per cell instead
    // of one, once per visible cell per frame. The §11 allocation budget is
    // what noticed.
    let mut out = String::with_capacity(SIZE_CAPACITY);
    let n = len as f64;
    let (value, unit) = if n >= TB {
        (n / TB, "TB")
    } else if n >= GB {
        (n / GB, "GB")
    } else if n >= MB {
        (n / MB, "MB")
    } else if n >= KB {
        (n / KB, "KB")
    } else {
        let _ = write!(out, "{len} B");
        return out;
    };

    let fractional = (value - value.trunc()).abs() > 0.04;
    let _ = if fractional && value < 10.0 {
        write!(out, "{value:.1} {unit}")
    } else {
        write!(out, "{value:.0} {unit}")
    };
    out
}

/// `25 Aug 2026 at 12:34`, in the user's own time zone.
///
/// A file saved a minute ago has to read as a minute ago. Formatting the UTC
/// civil date instead — which is what this did until D-39 — puts every row in
/// the Date Modified column hours out, and in half the world's zones on the
/// wrong day (A-27).
pub fn format_date(t: SystemTime) -> String {
    format_date_in(t, &TimeZone::system())
}

/// [`format_date`] against an explicit zone.
///
/// Exists so the tests can assert a real offset without touching the process
/// environment: `TimeZone::system()` resolves `TZ` once and caches it, so a
/// test that set the variable would be both racy under a threaded runner and
/// unable to check a second zone.
fn format_date_in(t: SystemTime, tz: &TimeZone) -> String {
    // Seconds since the epoch, which is what both the UTC fallback and jiff
    // want. `duration_since` fails only for a pre-1970 timestamp, which no
    // filesystem in play produces; that falls back to the epoch as before.
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // The local wall-clock reading of that instant. A timestamp jiff cannot
    // represent — beyond year 9999 — keeps the old UTC arithmetic rather than
    // failing to draw a row.
    let (days, rem) = match Timestamp::from_second(secs) {
        Ok(ts) => {
            let zoned = ts.to_zoned(tz.clone());
            let local = zoned.timestamp().as_second() + i64::from(zoned.offset().seconds());
            (local.div_euclid(86_400), local.rem_euclid(86_400))
        }
        Err(_) => (secs.div_euclid(86_400), secs.rem_euclid(86_400)),
    };

    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let (y, mo, d) = civil_from_days(days);

    static MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // Pre-sized, for the same reason as `format_size`.
    let mut out = String::with_capacity(DATE_CAPACITY);
    let _ = write!(
        out,
        "{d:>2} {month} {y} at {h:02}:{m:02}",
        month = MONTHS[(mo - 1) as usize]
    );
    out
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Natural comparison: digit runs compare numerically, so `file10` sorts
/// after `file9`. Case-insensitive on both platforms, because both default
/// to case-insensitive filesystems and surprising orders read as bugs.
pub fn natural_compare(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut ai = a.char_indices().peekable();
    let mut bi = b.char_indices().peekable();

    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&(ax, ac)), Some(&(bx, bc))) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    // Take the whole digit runs and compare them as numbers.
                    let a_end = digit_run_end(a, ax);
                    let b_end = digit_run_end(b, bx);
                    let a_run = a[ax..a_end].trim_start_matches('0');
                    let b_run = b[bx..b_end].trim_start_matches('0');
                    match a_run
                        .len()
                        .cmp(&b_run.len())
                        .then_with(|| a_run.cmp(b_run))
                        .then_with(|| {
                            // Equal numerically: shorter zero-padding first,
                            // which matches Finder.
                            (a_end - ax).cmp(&(b_end - bx))
                        }) {
                        Ordering::Equal => {}
                        other => return other,
                    }
                    for _ in ax..a_end {
                        ai.next();
                    }
                    for _ in bx..b_end {
                        bi.next();
                    }
                } else {
                    let alower = ac.to_lowercase().next().unwrap_or(ac);
                    let blower = bc.to_lowercase().next().unwrap_or(bc);
                    match alower.cmp(&blower) {
                        Ordering::Equal => {}
                        other => return other,
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// Case-insensitive byte order, for `settings.browser.natural_sort = false`.
///
/// The plain alternative to [`natural_compare`]: `file10` sorts before
/// `file9`, because `1` precedes `9`. Some people want exactly that — it is
/// what `ls` does — which is why the setting exists.
///
/// Case-insensitive like its natural sibling, and for the same reason: both
/// supported platforms have case-insensitive filesystems by default, so a
/// listing that put `Zebra` before `apple` would read as a bug either way.
pub fn plain_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
}

/// Whether `name` begins with `prefix`, ignoring case, without allocating.
///
/// Type-ahead and path completion both ask this once per entry per keystroke,
/// and both used to answer it by lowercasing both sides into fresh `String`s —
/// ~400,000 allocations per character typed in a large directory (A-13).
///
/// The ASCII path is a byte compare. The general path folds both sides lazily
/// through `char::to_lowercase`, which is exactly what `to_lowercase()` does
/// minus the two strings it built to do it — so a name with an ß or an İ in it
/// still matches what it matched before.
pub fn starts_with_ignore_case(name: &str, prefix: &str) -> bool {
    if name.is_ascii() && prefix.is_ascii() {
        return name.len() >= prefix.len()
            && name.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes());
    }

    let mut wanted = prefix.chars().flat_map(char::to_lowercase);
    let mut have = name.chars().flat_map(char::to_lowercase);
    loop {
        match (wanted.next(), have.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

fn digit_run_end(s: &str, from: usize) -> usize {
    s[from..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| from + i)
        .unwrap_or(s.len())
}

/// First free variant of `wanted`: `untitled folder`, `untitled folder 2`,
/// `report copy 3`…
///
/// Takes a predicate rather than a set of names (A-23). Every caller answered
/// it from the browsed directory, and materialising a `HashSet<String>` of
/// 200,000 names to ask a handful of yes-or-no questions is 200,000
/// allocations on the UI thread; `Folder::contains_name` answers each one from
/// the index the listing already maintains.
pub fn unique_name(is_taken: impl Fn(&str) -> bool, wanted: &str) -> String {
    if !is_taken(wanted) {
        return wanted.to_string();
    }
    let (stem, ext) = match wanted.rsplit_once('.') {
        // A leading dot makes a hidden file, not an extension.
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (wanted, None),
    };
    for n in 2..10_000 {
        let candidate = match ext {
            Some(ext) => format!("{stem} {n}.{ext}"),
            None => format!("{stem} {n}"),
        };
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    unreachable!("10,000 collisions on one name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::Duration;

    fn taken(names: &[&str]) -> impl Fn(&str) -> bool + use<> {
        let names: HashSet<String> = names.iter().map(|n| n.to_string()).collect();
        move |candidate: &str| names.contains(candidate)
    }

    #[test]
    fn unique_names_suffix_before_the_extension() {
        assert_eq!(
            unique_name(taken(&["notes.txt", "notes 2.txt"]), "notes.txt"),
            "notes 3.txt"
        );
        assert_eq!(unique_name(taken(&[]), "notes.txt"), "notes.txt");
    }

    #[test]
    fn dotfiles_are_not_treated_as_extensions() {
        assert_eq!(
            unique_name(taken(&[".gitignore"]), ".gitignore"),
            ".gitignore 2"
        );
    }

    #[test]
    fn extensionless_names_suffix_cleanly() {
        assert_eq!(unique_name(taken(&["Makefile"]), "Makefile"), "Makefile 2");
    }

    #[test]
    fn sizes_read_like_the_mockup() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(12 * 1024), "12 KB");
        assert_eq!(format_size(174 * 1024), "174 KB");
        assert_eq!(format_size((1.5 * 1024.0 * 1024.0) as u64), "1.5 MB");
        assert_eq!(format_size(1024 * 1024 * 3), "3 MB");
    }

    /// 2026-08-25 12:34:00 UTC.
    fn noon() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_787_661_240)
    }

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).expect("the system tz database must know this zone")
    }

    #[test]
    fn dates_render_in_civil_time() {
        assert_eq!(
            format_date_in(noon(), &TimeZone::UTC),
            "25 Aug 2026 at 12:34"
        );
        // The epoch itself.
        assert_eq!(
            format_date_in(UNIX_EPOCH, &TimeZone::UTC),
            " 1 Jan 1970 at 00:00"
        );
        // Leap year day: 2024-02-29.
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(format_date_in(leap, &TimeZone::UTC), "29 Feb 2024 at 00:00");
    }

    #[test]
    fn dates_are_shown_in_the_users_own_zone() {
        // The A-27 bug: this read 12:34 everywhere on earth. An offset does
        // not just move the clock — east of Greenwich it can move the *date*,
        // which is what made the old behaviour worth calling critical.
        assert_eq!(
            format_date_in(noon(), &zone("Asia/Kolkata")),
            "25 Aug 2026 at 18:04",
            "+05:30, and a half-hour offset at that"
        );
        assert_eq!(
            format_date_in(noon(), &zone("America/Los_Angeles")),
            "25 Aug 2026 at 05:34",
            "-07:00 in August"
        );
        assert_eq!(
            format_date_in(noon(), &zone("Pacific/Kiritimati")),
            "26 Aug 2026 at 02:34",
            "+14:00 rolls the date over into the next day"
        );
    }

    #[test]
    fn daylight_saving_is_not_a_fixed_offset() {
        // The reason this cannot be a constant, and the reason it cannot be
        // hand-rolled: the same zone is +01:00 in January and +02:00 in July.
        let berlin = zone("Europe/Berlin");
        // 2026-01-15 12:00 UTC → 13:00 CET
        let winter = UNIX_EPOCH + Duration::from_secs(1_768_478_400);
        assert_eq!(format_date_in(winter, &berlin), "15 Jan 2026 at 13:00");
        // 2026-07-15 12:00 UTC → 14:00 CEST
        let summer = UNIX_EPOCH + Duration::from_secs(1_784_116_800);
        assert_eq!(format_date_in(summer, &berlin), "15 Jul 2026 at 14:00");
    }

    #[test]
    fn plain_order_is_what_natural_order_is_not() {
        // The setting exists because this is what `ls` does, and some people
        // want it: byte order, so `file10` lands before `file9`.
        let mut names = vec!["file10", "file9", "file2", "File1"];
        names.sort_by(|a, b| plain_compare(a, b));
        assert_eq!(names, ["File1", "file10", "file2", "file9"]);
        // Still case-insensitive, like the natural one.
        assert_eq!(plain_compare("Apple", "apple"), std::cmp::Ordering::Equal);
        assert_eq!(plain_compare("apple", "Zebra"), std::cmp::Ordering::Less);
    }

    #[test]
    fn natural_order_puts_file10_after_file9() {
        let mut names = vec!["file10", "file9", "file2", "File1"];
        names.sort_by(|a, b| natural_compare(a, b));
        assert_eq!(names, ["File1", "file2", "file9", "file10"]);
    }

    #[test]
    fn leading_zeroes_break_ties_stably() {
        assert_eq!(
            natural_compare("img007", "img7"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(natural_compare("a1b", "a1b"), std::cmp::Ordering::Equal);
        assert_eq!(natural_compare("2", "10"), std::cmp::Ordering::Less);
    }

    #[test]
    fn case_insensitive_prefixes_match_what_lowercasing_did() {
        assert!(starts_with_ignore_case("Documents", "doc"));
        assert!(starts_with_ignore_case("documents", "DOC"));
        assert!(starts_with_ignore_case("Documents", ""));
        assert!(starts_with_ignore_case("Doc", "Doc"), "the whole name");
        assert!(!starts_with_ignore_case("Documents", "down"));
        assert!(!starts_with_ignore_case("Doc", "Documents"), "too short");

        // The non-ASCII path has to fold the same way `to_lowercase` did,
        // which is why it is a fold rather than a byte compare.
        assert!(starts_with_ignore_case("Ärger", "är"));
        assert!(starts_with_ignore_case("Ünicode.txt", "ÜNI"));
        assert!(!starts_with_ignore_case("Ärger", "arg"));
        // An ASCII needle against a non-ASCII name still takes the fold path.
        assert!(starts_with_ignore_case("Ärger", ""));
    }

    #[test]
    fn digit_run_detection() {
        assert_eq!(digit_run_end("ab12cd", 2), 4);
        assert_eq!(digit_run_end("ab12", 2), 4);
        assert_eq!(digit_run_end("abc", 0), 0);
    }
}
