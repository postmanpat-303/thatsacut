//! Timestamp parsing and formatting shared by the CLI and the cut planner.

use std::error::Error;
use std::fmt;

/// Error returned when a timestamp string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTimeError(pub String);

impl fmt::Display for ParseTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid timestamp: {:?}", self.0)
    }
}

impl Error for ParseTimeError {}

/// Parse a timestamp into seconds.
///
/// Accepted forms:
/// * plain seconds — `90`, `90.5`, `90,5`
/// * `MM:SS(.frac)` — `01:30`, `1:30.25`
/// * `HH:MM:SS(.frac)` — `01:02:03.5`
/// * unit suffixed — `1h30m`, `2m10s`, `500ms`, `1h2m3.5s`
pub fn parse_time(raw: &str) -> Result<f64, ParseTimeError> {
    let s = raw.trim().replace(',', ".");
    if s.is_empty() {
        return Err(ParseTimeError(raw.to_string()));
    }

    // Colon separated HH:MM:SS / MM:SS
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() > 3 {
            return Err(ParseTimeError(raw.to_string()));
        }
        let mut total = 0.0f64;
        for part in &parts {
            let value: f64 = part
                .trim()
                .parse()
                .map_err(|_| ParseTimeError(raw.to_string()))?;
            if value < 0.0 {
                return Err(ParseTimeError(raw.to_string()));
            }
            total = total * 60.0 + value;
        }
        return Ok(total);
    }

    // Unit suffixed form: 1h30m10.5s / 250ms
    if s.chars().any(|c| c.is_ascii_alphabetic()) {
        let mut total = 0.0f64;
        let mut number = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
                continue;
            }
            if number.is_empty() {
                return Err(ParseTimeError(raw.to_string()));
            }
            let value: f64 = number
                .parse()
                .map_err(|_| ParseTimeError(raw.to_string()))?;
            number.clear();
            total += match c {
                'h' | 'H' => value * 3600.0,
                'm' | 'M' => {
                    if matches!(chars.peek(), Some('s') | Some('S')) {
                        chars.next(); // consume the 's' of "ms"
                        value / 1000.0
                    } else {
                        value * 60.0
                    }
                }
                's' | 'S' => value,
                _ => return Err(ParseTimeError(raw.to_string())),
            };
        }
        if !number.is_empty() {
            // trailing digits with no unit
            return Err(ParseTimeError(raw.to_string()));
        }
        return Ok(total);
    }

    // Plain seconds
    match s.parse::<f64>() {
        Ok(v) if v >= 0.0 => Ok(v),
        _ => Err(ParseTimeError(raw.to_string())),
    }
}

/// Format seconds as `HH:MM:SS.mmm`.
pub fn format_time(seconds: f64) -> String {
    let secs = seconds.max(0.0);
    let hours = (secs / 3600.0).floor() as u64;
    let minutes = ((secs % 3600.0) / 60.0).floor() as u64;
    let rest = secs % 60.0;
    format!("{:02}:{:02}:{:06.3}", hours, minutes, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_seconds() {
        assert_eq!(parse_time("90").unwrap(), 90.0);
        assert_eq!(parse_time("90.5").unwrap(), 90.5);
        assert_eq!(parse_time("90,5").unwrap(), 90.5);
        assert!(parse_time("-5").is_err());
        assert!(parse_time("abc").is_err());
    }

    #[test]
    fn parses_colon_forms() {
        assert_eq!(parse_time("01:30").unwrap(), 90.0);
        assert_eq!(parse_time("1:30.25").unwrap(), 90.25);
        assert_eq!(parse_time("01:02:03.5").unwrap(), 3723.5);
        assert!(parse_time("1:2:3:4").is_err());
    }

    #[test]
    fn parses_unit_forms() {
        assert_eq!(parse_time("1h30m").unwrap(), 5400.0);
        assert_eq!(parse_time("2m10s").unwrap(), 130.0);
        assert_eq!(parse_time("500ms").unwrap(), 0.5);
        assert_eq!(parse_time("1h2m3.5s").unwrap(), 3723.5);
    }

    #[test]
    fn formats_time() {
        assert_eq!(format_time(0.0), "00:00:00.000");
        assert_eq!(format_time(3723.5), "01:02:03.500");
    }
}
