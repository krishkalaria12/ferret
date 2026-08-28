//! Display formatting: sizes, dates and natural name order.
//!
//! No date library — civil-from-days is thirty lines (Howard Hinnant's
//! algorithm) and keeps the dependency tree honest. Tabular alignment comes
//! from the caller's font styling, not from padding here.

use std::time::{SystemTime, UNIX_EPOCH};

/// Human size: `512 B`, `12 KB`, `1.5 MB`, `174 GB`. One decimal only when
/// the value is fractional and small, matching what Finder shows.
pub fn format_size(len: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

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
        return format!("{len} B");
    };

    let fractional = (value - value.trunc()).abs() > 0.04;
    if fractional && value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

/// `25 Aug 2026 at 12:34` — the mockup's status format.
pub fn format_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let (y, mo, d) = civil_from_days(days);

    static MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{d:>2} {month} {y} at {h:02}:{m:02}",
        month = MONTHS[(mo - 1) as usize]
    )
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

    #[test]
    fn dates_render_in_civil_time() {
        // 2026-08-25 12:34 UTC
        let t = UNIX_EPOCH + Duration::from_secs(1_787_661_240);
        assert_eq!(format_date(t), "25 Aug 2026 at 12:34");
        // The epoch itself.
        assert_eq!(format_date(UNIX_EPOCH), " 1 Jan 1970 at 00:00");
        // Leap year day: 2024-02-29.
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(format_date(leap), "29 Feb 2024 at 00:00");
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
