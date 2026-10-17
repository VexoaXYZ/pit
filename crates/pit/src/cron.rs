//! Five-field cron expressions (`minute hour day-of-month month day-of-week`),
//! evaluated in UTC or in a time zone, for scheduled runs.
//!
//! Supports `*`, numbers, ranges (`1-5`), steps (`*/15`, `0-30/10`) and lists
//! (`1,15`). Day of week is 0-7 with both 0 and 7 meaning Sunday. When both
//! day fields are restricted, either may match, as in classic cron.

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    minute: Vec<bool>,
    hour: Vec<bool>,
    dom: Vec<bool>,
    month: Vec<bool>,
    dow: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

fn field(spec: &str, min: u32, max: u32) -> Result<Vec<bool>> {
    let mut set = vec![false; max as usize + 1];
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .with_context(|| format!("bad step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step must be at least 1 in {part:?}");
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse()?, b.parse()?)
        } else {
            let v: u32 = range
                .parse()
                .with_context(|| format!("bad value {part:?}"))?;
            // `5/10` means from 5 to the end, every 10.
            (v, if part.contains('/') { max } else { v })
        };
        if lo < min || hi > max || lo > hi {
            bail!("{part:?} is outside {min}-{max}");
        }
        for v in (lo..=hi).step_by(step as usize) {
            set[v as usize] = true;
        }
    }
    Ok(set)
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        let [m, h, dom, mon, dow] = parts[..] else {
            bail!("cron {expr:?} needs five fields: minute hour day-of-month month day-of-week");
        };
        let mut dow_set = field(dow, 0, 7).with_context(|| format!("day of week in {expr:?}"))?;
        if dow_set[7] {
            dow_set[0] = true;
        }
        Ok(Cron {
            minute: field(m, 0, 59).with_context(|| format!("minute in {expr:?}"))?,
            hour: field(h, 0, 23).with_context(|| format!("hour in {expr:?}"))?,
            dom: field(dom, 1, 31).with_context(|| format!("day of month in {expr:?}"))?,
            month: field(mon, 1, 12).with_context(|| format!("month in {expr:?}"))?,
            dow: dow_set,
            dom_any: dom == "*",
            dow_any: dow == "*",
        })
    }

    /// Whether the expression fires in this minute (minutes since the Unix
    /// epoch, UTC).
    pub fn matches(&self, epoch_minute: i64) -> bool {
        let t = Civil::from_minute(epoch_minute);
        let day = match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (false, true) => self.dom[t.day as usize],
            (true, false) => self.dow[t.weekday as usize],
            (false, false) => self.dom[t.day as usize] || self.dow[t.weekday as usize],
        };
        self.minute[t.minute as usize]
            && self.hour[t.hour as usize]
            && self.month[t.month as usize]
            && day
    }
}

/// A schedule: an expression read in UTC or in a time zone.
#[derive(Debug, Clone)]
pub struct Schedule {
    cron: Cron,
    tz: Option<jiff::tz::TimeZone>,
}

impl Schedule {
    pub fn parse(expr: &str, timezone: Option<&str>) -> Result<Schedule> {
        let tz = match timezone {
            None => None,
            Some(name) => Some(jiff::tz::TimeZone::get(name).with_context(|| {
                format!("unknown time zone {name:?} (use an IANA name like \"Europe/London\")")
            })?),
        };
        Ok(Schedule {
            cron: Cron::parse(expr)?,
            tz,
        })
    }

    /// The wall-clock minute in the schedule's zone, counted like a UTC
    /// minute, for a minute since the epoch.
    fn wall(&self, epoch_minute: i64) -> i64 {
        let Some(tz) = &self.tz else {
            return epoch_minute;
        };
        let offset = tz
            .to_offset(jiff::Timestamp::from_second(epoch_minute * 60).unwrap_or_default())
            .seconds() as i64;
        epoch_minute + offset.div_euclid(60)
    }

    /// Whether the schedule fires in this minute (since the epoch). Across
    /// daylight saving changes, as GitHub does: a time skipped by the clocks
    /// going forward fires at the next valid minute, and one repeated by the
    /// clocks going back fires only the first time.
    pub fn fires(&self, epoch_minute: i64) -> bool {
        let w = self.wall(epoch_minute);
        if self.tz.is_none() {
            return self.cron.matches(w);
        }
        let prev = self.wall(epoch_minute - 1);
        if self.cron.matches(w) {
            // The same wall time earlier (the repeated hour).
            return !(1..=180).any(|k| self.wall(epoch_minute - k) == w);
        }
        w > prev + 1 && (prev + 1..w).any(|m| self.cron.matches(m))
    }
}

/// A UTC calendar time, without a date library.
struct Civil {
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    /// 0 is Sunday.
    weekday: u32,
}

impl Civil {
    fn from_minute(epoch_minute: i64) -> Civil {
        let days = epoch_minute.div_euclid(1440);
        let in_day = epoch_minute.rem_euclid(1440);
        // Howard Hinnant's civil_from_days.
        let doe = (days + 719_468).rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        Civil {
            month,
            day,
            hour: (in_day / 60) as u32,
            minute: (in_day % 60) as u32,
            // 1970-01-01 was a Thursday.
            weekday: (days + 4).rem_euclid(7) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-16 03:07 UTC is a Friday.
    const FRI_0307: i64 = 1_792_120_020 / 60;

    #[test]
    fn calendar_is_right() {
        let t = Civil::from_minute(FRI_0307);
        assert_eq!(
            (t.month, t.day, t.hour, t.minute, t.weekday),
            (10, 16, 3, 7, 5)
        );
        // Leap day.
        let t = Civil::from_minute(1_709_164_800 / 60);
        assert_eq!((t.month, t.day), (2, 29));
    }

    #[test]
    fn matches_like_cron() {
        assert!(Cron::parse("7 3 * * *").unwrap().matches(FRI_0307));
        assert!(!Cron::parse("8 3 * * *").unwrap().matches(FRI_0307));
        assert!(Cron::parse("*/7 * * * *").unwrap().matches(FRI_0307));
        assert!(Cron::parse("0-10/7 3 * * 1-5").unwrap().matches(FRI_0307));
        assert!(!Cron::parse("7 3 * * 0,6").unwrap().matches(FRI_0307));
        // Either restricted day field may match.
        assert!(Cron::parse("7 3 1 * 5").unwrap().matches(FRI_0307));
        assert!(!Cron::parse("7 3 1 * 6").unwrap().matches(FRI_0307));
        assert!(Cron::parse("7 3 * 10 7").is_ok());
    }

    /// Minutes since the epoch of a UTC date and time.
    fn utc(s: &str) -> i64 {
        s.parse::<jiff::Timestamp>().unwrap().as_second() / 60
    }

    #[test]
    fn time_zones_follow_daylight_saving_like_github() {
        let ny = Schedule::parse("30 9 * * *", Some("America/New_York")).unwrap();
        // 9:30 in New York is 13:30 UTC in summer and 14:30 in winter.
        assert!(ny.fires(utc("2026-07-01T13:30:00Z")));
        assert!(!ny.fires(utc("2026-07-01T14:30:00Z")));
        assert!(ny.fires(utc("2026-12-01T14:30:00Z")));
        assert!(!ny.fires(utc("2026-12-01T13:30:00Z")));
        // 2:30 does not exist on 2026-03-08 (clocks go 2:00 -> 3:00 at
        // 07:00 UTC): it fires at 3:00 local instead, once.
        let gap = Schedule::parse("30 2 * * *", Some("America/New_York")).unwrap();
        let fired: Vec<i64> = (utc("2026-03-08T05:00:00Z")..utc("2026-03-08T09:00:00Z"))
            .filter(|m| gap.fires(*m))
            .collect();
        assert_eq!(fired, [utc("2026-03-08T07:00:00Z")]);
        // 1:30 happens twice on 2026-11-01; it fires the first time only.
        let twice = Schedule::parse("30 1 * * *", Some("America/New_York")).unwrap();
        let fired: Vec<i64> = (utc("2026-11-01T04:00:00Z")..utc("2026-11-01T08:00:00Z"))
            .filter(|m| twice.fires(*m))
            .collect();
        assert_eq!(fired, [utc("2026-11-01T05:30:00Z")]);
        // UTC as before, and bad zones are errors.
        assert!(Schedule::parse("30 9 * * *", None)
            .unwrap()
            .fires(utc("2026-07-01T09:30:00Z")));
        assert!(Schedule::parse("30 9 * * *", Some("Mars/Olympus")).is_err());
    }

    #[test]
    fn rejects_bad_expressions() {
        for bad in [
            "* * * *",
            "60 * * * *",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }
}
