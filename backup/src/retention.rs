//! Which nights to keep: every night of the last 30 days, plus the first night
//! of each month for the last 12 months. The newest night is never deleted.

use chrono::{Datelike, NaiveDate};
use std::collections::HashSet;

pub const DAILY_DAYS: i64 = 30;
pub const MONTHLY_DAYS: i64 = 366;

pub fn to_delete(nights: &[NaiveDate], today: NaiveDate) -> Vec<NaiveDate> {
    let mut sorted = nights.to_vec();
    sorted.sort();
    let mut keep: HashSet<NaiveDate> = HashSet::new();
    let mut months_seen: HashSet<(i32, u32)> = HashSet::new();
    for &d in &sorted {
        let age = (today - d).num_days();
        if age < DAILY_DAYS {
            keep.insert(d);
        }
        if age < MONTHLY_DAYS && months_seen.insert((d.year(), d.month())) {
            keep.insert(d); // first night we have in that month
        }
    }
    if let Some(&newest) = sorted.last() {
        keep.insert(newest);
    }
    sorted.into_iter().filter(|d| !keep.contains(d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn every_day(from: &str, to: &str) -> Vec<NaiveDate> {
        let (mut cur, end) = (d(from), d(to));
        let mut out = vec![];
        while cur <= end {
            out.push(cur);
            cur = cur.succ_opt().unwrap();
        }
        out
    }

    #[test]
    fn keeps_last_30_days() {
        let nights = every_day("2026-09-01", "2026-10-18");
        let gone = to_delete(&nights, d("2026-10-18"));
        assert!(!gone.contains(&d("2026-09-19"))); // 29 days old
        assert!(gone.contains(&d("2026-09-18"))); // 30 days old
    }

    #[test]
    fn keeps_first_night_of_each_month() {
        let nights = every_day("2025-09-01", "2026-10-18");
        let gone = to_delete(&nights, d("2026-10-18"));
        assert!(!gone.contains(&d("2026-03-01")));
        assert!(gone.contains(&d("2026-03-02")));
        assert!(gone.contains(&d("2025-10-01"))); // older than 12 months
    }

    #[test]
    fn missed_first_of_month_keeps_next_one() {
        let nights = vec![d("2026-05-03"), d("2026-05-10"), d("2026-10-18")];
        let gone = to_delete(&nights, d("2026-10-18"));
        assert_eq!(gone, vec![d("2026-05-10")]);
    }

    #[test]
    fn never_deletes_newest() {
        let nights = vec![d("2024-01-05")];
        assert!(to_delete(&nights, d("2026-10-18")).is_empty());
    }
}
