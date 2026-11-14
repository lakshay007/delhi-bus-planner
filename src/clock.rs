//! Time handling. Everything user-facing is Asia/Kolkata (UTC+5:30, no DST),
//! so a fixed offset is exact and no timezone database is needed.

pub const IST_OFFSET: i64 = 19_800;

/// Parse GTFS "HH:MM:SS" (hours may exceed 23) into seconds.
pub fn parse_hms(s: &str) -> Option<u32> {
    let mut it = s.trim().split(':');
    let h: u32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let sec: u32 = it.next().unwrap_or("0").parse().ok()?;
    Some(h * 3600 + m * 60 + sec)
}

pub fn fmt_hms(secs: u32) -> String {
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Number of day kinds `Local::day_type` can return.
pub const DAY_KINDS: usize = 4;
pub const DAY_KIND_NAMES: [&str; DAY_KINDS] = ["weekday", "saturday", "sunday", "holiday"];

/// Gazetted public holidays in Delhi on which buses ran visibly faster in the
/// October 2026 archive (2 Oct, 20 Oct), and the gazetted ones still to come
/// this year. Dates from published 2026 holiday lists;
/// extend each year. Lesser holidays (26 Oct, 29 Oct) barely moved speeds and
/// are left as ordinary days.
const HOLIDAYS: [(i64, i64, i64); 4] = [(2026, 10, 2), (2026, 10, 20), (2026, 11, 24), (2026, 12, 25)];

/// A moment split into the pieces the model cares about, in IST.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Local {
    /// Days since 1970-01-01 (IST calendar date).
    pub day: i64,
    /// Seconds since IST midnight.
    pub tod: u32,
}

impl Local {
    pub fn from_epoch(epoch: i64) -> Self {
        let l = epoch + IST_OFFSET;
        Local { day: l.div_euclid(86_400), tod: l.rem_euclid(86_400) as u32 }
    }

    pub fn epoch(&self) -> i64 {
        self.day * 86_400 + self.tod as i64 - IST_OFFSET
    }

    /// 0 = Monday … 6 = Sunday.
    pub fn weekday(&self) -> u32 {
        (self.day + 3).rem_euclid(7) as u32
    }

    /// Kind of day, for the travel-time model: 0 = Monday to Friday,
    /// 1 = Saturday, 2 = Sunday, 3 = public holiday on a Monday to
    /// Saturday. How kinds are pooled is the model's business.
    pub fn day_type(&self) -> usize {
        match self.weekday() {
            6 => 2,
            _ if HOLIDAYS.contains(&civil_from_days(self.day)) => 3,
            5 => 1,
            _ => 0,
        }
    }

    pub fn date_string(&self) -> String {
        let (y, m, d) = civil_from_days(self.day);
        format!("{y:04}-{m:02}-{d:02}")
    }

    pub fn iso(&self) -> String {
        format!("{}T{}+05:30", self.date_string(), fmt_hms(self.tod))
    }

    /// Parse "YYYY-MM-DDTHH:MM[:SS]" as IST wall-clock time. A trailing
    /// offset or "Z" is ignored: the API is defined in Delhi local time.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (date, time) = s.split_once(['T', ' '])?;
        let mut d = date.split('-');
        let (y, m, dd) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
        if !(1..=12).contains(&m) || !(1..=31).contains(&dd) {
            return None;
        }
        let time: String = time.chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
        let tod = parse_hms(&time)?;
        if tod >= 86_400 {
            return None;
        }
        Some(Local { day: days_from_civil(y, m, dd), tod })
    }

    pub fn plus(&self, secs: i64) -> Self {
        Local::from_epoch(self.epoch() + secs)
    }
}

pub fn now_epoch() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip_and_weekday() {
        // 2026-11-13 is a Friday; 2026-11-17 a Tuesday.
        let l = Local::parse("2026-11-13T01:27:46").unwrap();
        assert_eq!(l.weekday(), 4);
        assert_eq!(l.date_string(), "2026-11-13");
        assert_eq!(Local::parse("2026-11-17T08:30").unwrap().weekday(), 1);
        assert_eq!(Local::parse("2026-11-15T08:30").unwrap().day_type(), 2); // Sunday
        assert_eq!(Local::parse("2026-11-14T08:30").unwrap().day_type(), 1); // Saturday
        assert_eq!(Local::parse("2026-11-17T08:30").unwrap().day_type(), 0);
        assert_eq!(Local::parse("2026-10-20T08:30").unwrap().day_type(), 3); // Dussehra, a Tuesday
        // Epoch from the first archived snapshot: 1794513466 = 2026-11-13 01:27:46 IST.
        assert_eq!(Local::from_epoch(1_794_513_466), l);
        assert_eq!(l.epoch(), 1_794_513_466);
    }

    #[test]
    fn gtfs_times_past_midnight() {
        assert_eq!(parse_hms("25:10:05"), Some(25 * 3600 + 605));
    }
}
