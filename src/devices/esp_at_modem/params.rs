//! AT command parameter parsing and small formatting helpers.

/// One comma-separated AT parameter.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Param {
    /// Omitted (`,,`).
    Empty,
    Int(i64),
    /// Double-quoted string with ESP-AT escapes (`\\`, `\"`, `\,`) removed.
    Str(String),
}

impl Param {
    pub fn int(&self) -> Option<i64> {
        match self {
            Param::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Param::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Parse the part after `=` of an `AT+CMD=...` line. `None` = malformed (ESP-AT replies ERROR).
pub(super) fn parse_params(s: &str) -> Option<Vec<Param>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        if chars.peek() == Some(&'"') {
            chars.next();
            let mut v = String::new();
            loop {
                match chars.next()? {
                    '\\' => v.push(chars.next()?),
                    '"' => break,
                    c => v.push(c),
                }
            }
            out.push(Param::Str(v));
            match chars.next() {
                None => return Some(out),
                Some(',') => continue,
                Some(_) => return None,
            }
        }
        let mut tok = String::new();
        let mut last = true;
        for c in chars.by_ref() {
            if c == ',' {
                last = false;
                break;
            }
            tok.push(c);
        }
        let tok = tok.trim();
        if tok.is_empty() {
            out.push(Param::Empty);
        } else {
            out.push(Param::Int(tok.parse().ok()?));
        }
        if last {
            return Some(out);
        }
    }
}

/// `xx:xx:xx:xx:xx:xx`, lower case like ESP-AT prints it.
pub(super) fn fmt_mac(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// C `asctime()` layout without the trailing newline, e.g. `Tue Oct 19 17:47:56 2021`
/// (single-digit days are space padded: `Tue Oct  5 ...`). This is what AT+CIPSNTPTIME? prints.
pub(super) fn asctime(epoch_secs: i64) -> String {
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = epoch_secs.div_euclid(86_400);
    let sod = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{} {}{:3} {:02}:{:02}:{:02} {}",
        WD[days.rem_euclid(7) as usize],
        MON[(m - 1) as usize],
        d,
        sod / 3600,
        (sod / 60) % 60,
        sod % 60,
        y
    )
}

/// Howard Hinnant's days-since-epoch -> (year, month 1..=12, day 1..=31).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params() {
        assert_eq!(
            parse_params(r#"2,1,"http://a/b?x=1\,2&q=\"z\"",,,2"#).unwrap(),
            vec![
                Param::Int(2),
                Param::Int(1),
                Param::Str(r#"http://a/b?x=1,2&q="z""#.into()),
                Param::Empty,
                Param::Empty,
                Param::Int(2)
            ]
        );
        assert_eq!(parse_params(r#""a\\b","""#).unwrap(), vec![Param::Str(r"a\b".into()), Param::Str("".into())]);
        assert_eq!(parse_params("-5").unwrap(), vec![Param::Int(-5)]);
        assert!(parse_params("x").is_none());
        assert!(parse_params(r#""unterminated"#).is_none());
    }

    #[test]
    fn asctime_format() {
        assert_eq!(asctime(1_634_665_676), "Tue Oct 19 17:47:56 2021");
        assert_eq!(asctime(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(asctime(951_782_400), "Tue Feb 29 00:00:00 2000");
    }
}
