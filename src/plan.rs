//! Cut planning: turns keep/remove range requests into a concrete segment list.

use crate::time::{parse_time, ParseTimeError};
use std::error::Error;
use std::fmt;

/// Half-open `[start, end)` range in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub start: f64,
    pub end: f64,
}

impl Range {
    /// Length of the range in seconds.
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

/// Errors produced while building a cut plan.
#[derive(Debug)]
pub enum PlanError {
    Parse(ParseTimeError),
    EmptyVideo,
    EmptyPlan,
    InvalidRange { start: f64, end: f64 },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Parse(e) => write!(f, "{e}"),
            PlanError::EmptyVideo => write!(f, "the video has zero (or unknown) duration"),
            PlanError::EmptyPlan => write!(f, "the cut plan is empty — nothing would be kept"),
            PlanError::InvalidRange { start, end } => {
                write!(
                    f,
                    "invalid range {start}–{end}: end must be greater than start"
                )
            }
        }
    }
}

impl Error for PlanError {}

impl From<ParseTimeError> for PlanError {
    fn from(e: ParseTimeError) -> Self {
        PlanError::Parse(e)
    }
}

/// Parse a comma-separated list of ranges, e.g. `0-10, 01:00-02:30.5, 1m30s-2m`.
pub fn parse_ranges(raw: &str) -> Result<Vec<Range>, PlanError> {
    let mut out = Vec::new();
    for chunk in raw.split(',') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let (a, b) = chunk
            .split_once('-')
            .ok_or_else(|| PlanError::Parse(ParseTimeError(chunk.to_string())))?;
        let start = parse_time(a)?;
        let end = parse_time(b)?;
        if end <= start {
            return Err(PlanError::InvalidRange { start, end });
        }
        out.push(Range { start, end });
    }
    if out.is_empty() {
        return Err(PlanError::EmptyPlan);
    }
    Ok(out)
}

/// Build the final ordered, non-overlapping list of segments to keep.
///
/// * `remove == false` — the listed ranges are the output (overlaps merged).
/// * `remove == true`  — the listed ranges are cut out; everything else stays.
///
/// Ranges are clamped to `[0, duration]`.
pub fn build_plan(duration: f64, ranges: &[Range], remove: bool) -> Result<Vec<Range>, PlanError> {
    if duration <= 0.0 {
        return Err(PlanError::EmptyVideo);
    }
    if ranges.is_empty() {
        return Err(PlanError::EmptyPlan);
    }

    let mut clamped: Vec<Range> = ranges
        .iter()
        .map(|r| Range {
            start: r.start.clamp(0.0, duration),
            end: r.end.clamp(0.0, duration),
        })
        .filter(|r| r.end - r.start > 1e-6)
        .collect();
    if clamped.is_empty() {
        return Err(PlanError::EmptyPlan);
    }

    clamped.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Merge overlapping / touching ranges.
    let mut merged: Vec<Range> = Vec::with_capacity(clamped.len());
    for r in clamped {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end + 1e-6 {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }

    if !remove {
        return Ok(merged);
    }

    // Invert against [0, duration].
    let mut kept = Vec::new();
    let mut cursor = 0.0f64;
    for r in &merged {
        if r.start - cursor > 1e-6 {
            kept.push(Range {
                start: cursor,
                end: r.start,
            });
        }
        cursor = r.end;
    }
    if duration - cursor > 1e-6 {
        kept.push(Range {
            start: cursor,
            end: duration,
        });
    }
    if kept.is_empty() {
        return Err(PlanError::EmptyPlan);
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_range_lists() {
        let rs = parse_ranges("0-10, 01:00-02:00, 1m30s-2m").unwrap();
        assert_eq!(
            rs[0],
            Range {
                start: 0.0,
                end: 10.0
            }
        );
        assert_eq!(
            rs[1],
            Range {
                start: 60.0,
                end: 120.0
            }
        );
        assert_eq!(
            rs[2],
            Range {
                start: 90.0,
                end: 120.0
            }
        );
        assert!(parse_ranges("10-5").is_err());
        assert!(parse_ranges("nope").is_err());
    }

    #[test]
    fn keep_mode_merges_overlaps() {
        let rs = parse_ranges("0-10,5-15,30-40").unwrap();
        let plan = build_plan(100.0, &rs, false).unwrap();
        assert_eq!(
            plan,
            vec![
                Range {
                    start: 0.0,
                    end: 15.0
                },
                Range {
                    start: 30.0,
                    end: 40.0
                }
            ]
        );
    }

    #[test]
    fn remove_mode_inverts() {
        let rs = parse_ranges("10-20,50-60").unwrap();
        let plan = build_plan(100.0, &rs, true).unwrap();
        assert_eq!(
            plan,
            vec![
                Range {
                    start: 0.0,
                    end: 10.0
                },
                Range {
                    start: 20.0,
                    end: 50.0
                },
                Range {
                    start: 60.0,
                    end: 100.0
                },
            ]
        );
    }

    #[test]
    fn clamps_to_duration() {
        let rs = parse_ranges("0-500").unwrap();
        let plan = build_plan(100.0, &rs, false).unwrap();
        assert_eq!(
            plan,
            vec![Range {
                start: 0.0,
                end: 100.0
            }]
        );
    }

    #[test]
    fn removing_everything_is_an_error() {
        let rs = parse_ranges("0-100").unwrap();
        assert!(matches!(
            build_plan(100.0, &rs, true),
            Err(PlanError::EmptyPlan)
        ));
    }
}
