//! Cron expression parsing + next trigger calculation (local calendar semantics), and custom scheduleRule's date engine.
//!
//! TS uses the `croner` library to do "parsing + calculating the next time" (automationCron.ts:59-63); this crate does not introduce it
//!croner (not available offline), changed to handwritten 5-segment standard cron parser + field-by-field bitmap matching. semantic alignment
//! General subset of croner: `*`, number, `A-B` range, `*​/N` and `A-B/N` steps, comma list;
//! Use Vixie cron's OR semantics when day/week restrictions are imposed (automationCron.ts relies on this behavior of croner).
//!
//! Scheduling only uses cron to calculate the "next time" and does not use it for actual scheduling - the scheduling cycle is polled by the scheduler itself
//! automation_runs / claimDue completed (automationCron.ts:51-52 same note).

use chrono::LocalResult;
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};

use crate::types::ScheduleRule;

/// One-time fixed month and day cron identification window "the target has just passed" (automationCron.ts:12).
const ONE_SHOT_STALE_TARGET_WINDOW_MS: i64 = 30 * 60 * 1_000;
/// The grace period in which a one-time task can still be safely executed after it is missed (<1min is regarded as a span-minute error in tool calls).
const ONE_SHOT_MISSED_RUN_GRACE_MS: i64 = 60 * 1_000;

/// Bitmap matcher for a field (value range ≤64, use u64 bitmap).
#[derive(Debug, Clone, Copy)]
struct FieldMask {
    bits: u64,
    /// Is this segment a wildcard (`*`/`?`) - Vixie OR semantic determination for dom/dow.
    any: bool,
}

impl FieldMask {
    fn matches(&self, value: u32) -> bool {
        self.bits & (1_u64 << value) != 0
    }
}

/// Parsed 5-segment cron (minute hour day-of-month month day-of-week).
#[derive(Debug, Clone)]
pub struct CronExpr {
    minute: FieldMask,
    hour: FieldMask,
    day_of_month: FieldMask,
    month: FieldMask,
    day_of_week: FieldMask,
}

/// Parse a single field into a bitmap. `min`/`max` are closed intervals; 7 of dow is normalized to 0 (Sunday).
fn parse_field(token: &str, min: u32, max: u32, normalize_seven: bool) -> Option<FieldMask> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let any = token == "*" || token == "?";
    let mut bits = 0_u64;
    for part in token.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        // range part: base or base/step.
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().ok()?;
                if step == 0 {
                    return None;
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (low, high) = if range == "*" || range == "?" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)
        } else {
            let v: u32 = range.parse().ok()?;
            (v, v)
        };
        if low > high || low < min || high > max {
            return None;
        }
        let mut v = low;
        while v <= high {
            let mut bit = v;
            if normalize_seven && bit == 7 {
                bit = 0;
            }
            bits |= 1_u64 << bit;
            v += step;
        }
    }
    if bits == 0 {
        return None;
    }
    Some(FieldMask { bits, any })
}

impl CronExpr {
    /// Parse the 5-segment standard cron. Returns None on failure (the caller treats it as an illegal expression).
    pub fn parse(expr: &str) -> Option<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }
        Some(Self {
            minute: parse_field(parts[0], 0, 59, false)?,
            hour: parse_field(parts[1], 0, 23, false)?,
            day_of_month: parse_field(parts[2], 1, 31, false)?,
            month: parse_field(parts[3], 1, 12, false)?,
            day_of_week: parse_field(parts[4], 0, 7, true)?,
        })
    }

    /// Vixie cron's day matching: day/week are both limited OR; use that one when only one is limited; both are wildcards and always true.
    fn day_matches(&self, date: NaiveDate) -> bool {
        let dom_ok = self.day_of_month.matches(date.day());
        // chrono: Monday=0..Sunday=6; cron: Sunday=0..Saturday=6. Convert to cron caliber.
        let cron_dow = date.weekday().num_days_from_sunday();
        let dow_ok = self.day_of_week.matches(cron_dow);
        match (self.day_of_month.any, self.day_of_week.any) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// Calculate the next trigger that is strictly later than `from` (local time zone milliseconds). Returns None if there is no future trigger.
    pub fn next_run(&self, from_ms: i64) -> Option<i64> {
        let from = Local.timestamp_millis_opt(from_ms).single()?;
        let from_naive = from.naive_local();
        let mut dt = floor_minute(from_naive) + Duration::minutes(1);
        let horizon_year = from_naive.year() + 5;
        loop {
            if dt.year() > horizon_year {
                return None;
            }
            let date = dt.date();
            if !self.month.matches(date.month()) {
                dt = first_of_next_month(date);
                continue;
            }
            if !self.day_matches(date) {
                dt = date.and_hms_opt(0, 0, 0)? + Duration::days(1);
                continue;
            }
            if !self.hour.matches(dt.hour()) {
                dt = floor_hour(dt) + Duration::hours(1);
                continue;
            }
            if !self.minute.matches(dt.minute()) {
                dt += Duration::minutes(1);
                continue;
            }
            // Exact match; local time in DST hole does not exist → jump to next minute and try again.
            if let Some(ms) = naive_to_ms(dt) {
                return Some(ms);
            }
            dt += Duration::minutes(1);
        }
    }

    /// Calculate the last trigger that is strictly earlier than `from` (used for staleness detection of one-time fixed calendar tasks,
    /// automationCron.ts:104 `previousRuns(1, from)`).
    pub fn prev_run(&self, from_ms: i64) -> Option<i64> {
        let from = Local.timestamp_millis_opt(from_ms).single()?;
        let from_naive = from.naive_local();
        let mut dt = floor_minute(from_naive) - Duration::minutes(1);
        let horizon_year = from_naive.year() - 5;
        loop {
            if dt.year() < horizon_year {
                return None;
            }
            let date = dt.date();
            if !self.month.matches(date.month()) {
                dt = last_of_prev_month(date);
                continue;
            }
            if !self.day_matches(date) {
                dt = date.and_hms_opt(0, 0, 0)? - Duration::days(1);
                continue;
            }
            if !self.hour.matches(dt.hour()) {
                dt = floor_hour(dt) - Duration::hours(1);
                continue;
            }
            if !self.minute.matches(dt.minute()) {
                dt -= Duration::minutes(1);
                continue;
            }
            if let Some(ms) = naive_to_ms(dt) {
                return Some(ms);
            }
            dt -= Duration::minutes(1);
        }
    }
}

/// Verify whether the cron expression is legal (5 segments, local time zone).
pub fn is_valid_cron(expr: &str) -> bool {
    CronExpr::parse(expr).is_some()
}

fn floor_minute(dt: NaiveDateTime) -> NaiveDateTime {
    dt.date()
        .and_hms_opt(dt.hour(), dt.minute(), 0)
        .expect("floor minute is a valid datetime")
}

fn floor_hour(dt: NaiveDateTime) -> NaiveDateTime {
    dt.date()
        .and_hms_opt(dt.hour(), 0, 0)
        .expect("floor hour is a valid datetime")
}

fn first_of_next_month(date: NaiveDate) -> NaiveDateTime {
    let (year, month) = if date.month() == 12 {
        (date.year() + 1, 1)
    } else {
        (date.year(), date.month() + 1)
    };
    NaiveDate::from_ymd_opt(year, month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("first of month is a valid datetime")
}

fn last_of_prev_month(date: NaiveDate) -> NaiveDateTime {
    let first_this = date.with_day(1).expect("first of month");
    (first_this - Duration::days(1))
        .and_hms_opt(23, 59, 0)
        .expect("end of prev day")
}

/// naive local → local time zone milliseconds. DST holes (moments do not exist) return None and are skipped by the caller.
fn naive_to_ms(naive: NaiveDateTime) -> Option<i64> {
    match naive.and_local_timezone(Local) {
        LocalResult::Single(dt) => Some(dt.timestamp_millis()),
        LocalResult::Ambiguous(dt, _) => Some(dt.timestamp_millis()),
        LocalResult::None => None,
    }
}

fn ms_to_naive_local(ms: i64) -> Option<NaiveDateTime> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.naive_local())
}

/// Stable serialization of rule content (ignoring anchorAt), used for equality comparison of "only changing the display field"
/// (automationCron.ts:16-27 scheduleRuleDefinition).
pub fn schedule_rule_definition(rule: &ScheduleRule) -> String {
    fn sorted(list: &Option<Vec<u32>>) -> Vec<u32> {
        list.clone()
            .map(|mut v| {
                v.sort_unstable();
                v
            })
            .unwrap_or_default()
    }
    let weekdays = rule.weekdays.as_ref().map(|_| sorted(&rule.weekdays));
    let month_days = rule.month_days.as_ref().map(|_| sorted(&rule.month_days));
    let months = rule.months.as_ref().map(|_| sorted(&rule.months));
    serde_json::json!([
        rule.unit,
        rule.interval,
        rule.hour,
        rule.minute,
        weekdays,
        month_days,
        months,
        rule.monthly_mode,
    ])
    .to_string()
}

/// Next run of custom recurrence rules (local calendar; automationCron.ts:133-220 branch-by-branch migration).
pub fn compute_schedule_rule_next_run_at(rule: &ScheduleRule, from_ms: i64) -> Option<i64> {
    let interval = rule.interval.max(1) as i64;
    let anchor = ms_to_naive_local(rule.anchor_at)?;
    match rule.unit {
        crate::types::ScheduleUnit::Minute => {
            let step = interval * 60 * 1_000;
            //The minute interval advances from the anchor point and does not follow the wall clock scale (to avoid accumulated drift of late dispatch by the scheduler).
            let steps = (((from_ms - rule.anchor_at) / step).max(0)) + 1;
            Some(rule.anchor_at + steps * step)
        }
        crate::types::ScheduleUnit::Hourly => {
            let base_naive =
                floor_hour(anchor.with_minute(rule.minute)?).with_minute(rule.minute)?;
            let base = naive_to_ms(base_naive)?;
            let step = interval * 60 * 60 * 1_000;
            let steps = (((from_ms - base) / step).max(0)) + 1;
            Some(base + steps * step)
        }
        crate::types::ScheduleUnit::Daily => {
            for index in 0..36_600_i64 {
                let date = anchor.date() + Duration::days(index * interval);
                if let Some(candidate) = at_time(date, rule.hour, rule.minute) {
                    if candidate > from_ms {
                        return Some(candidate);
                    }
                }
            }
            None
        }
        crate::types::ScheduleUnit::Weekly => {
            let anchor_dow = anchor.weekday().num_days_from_sunday();
            let anchor_week = anchor.date() - Duration::days(anchor_dow as i64);
            let mut weekdays = rule
                .weekdays
                .clone()
                .filter(|list| !list.is_empty())
                .unwrap_or_else(|| vec![1]);
            weekdays.sort_unstable();
            let mut week = 0_i64;
            while week < 5_220 {
                for weekday in &weekdays {
                    let day_offset = *weekday as i64;
                    let date = anchor_week + Duration::days(week * 7 + day_offset);
                    if let Some(candidate) = at_time(date, rule.hour, rule.minute) {
                        if candidate > from_ms {
                            return Some(candidate);
                        }
                    }
                }
                week += interval;
            }
            None
        }
        crate::types::ScheduleUnit::Monthly => {
            let mut offset = 0_i64;
            // 1200 is both the legal monthly interval and the search boundary (the next round after 100 years must be counted only when the boundary is included).
            while offset <= 1_200 {
                let month_first = add_months(anchor.date(), offset)?;
                let candidates: Vec<NaiveDate> = match rule.monthly_mode {
                    Some(crate::types::MonthlyMode::Weekday) => {
                        let weekday = rule
                            .weekdays
                            .as_ref()
                            .and_then(|list| list.first().copied())
                            .unwrap_or(1);
                        vec![first_weekday_of_month(
                            month_first.year(),
                            month_first.month(),
                            weekday,
                        )?]
                    }
                    _ => {
                        let mut days = rule
                            .month_days
                            .clone()
                            .filter(|list| !list.is_empty())
                            .unwrap_or_else(|| vec![1]);
                        days.sort_unstable();
                        days.into_iter()
                            .filter_map(|day| {
                                NaiveDate::from_ymd_opt(
                                    month_first.year(),
                                    month_first.month(),
                                    day,
                                )
                            })
                            // Overflow guard: If it rolls to the next month like 2/30, it should be skipped.
                            .filter(|date| date.month() == month_first.month())
                            .collect()
                    }
                };
                for date in candidates {
                    if let Some(candidate) = at_time(date, rule.hour, rule.minute) {
                        if candidate > from_ms {
                            return Some(candidate);
                        }
                    }
                }
                offset += interval;
            }
            None
        }
        crate::types::ScheduleUnit::Yearly => {
            //The month is given priority by rule.months (1-12), and the default fallback is to the anchor month (compatible with old records).
            let target_month = rule
                .months
                .as_ref()
                .and_then(|list| list.first().copied())
                .map(|m| ((m as i32 - 1).rem_euclid(12)) as u32 + 1)
                .unwrap_or(anchor.month());
            let target_day = rule
                .month_days
                .as_ref()
                .and_then(|list| list.first().copied())
                .unwrap_or(anchor.day());
            let mut offset = 0_i64;
            while offset < 400 {
                let year = anchor.year() + offset as i32;
                if let Some(date) = NaiveDate::from_ymd_opt(year, target_month, target_day) {
                    // Overflow guard: Non-leap years such as 2/29 will be rolled to the next month, skipping the year to avoid false triggers.
                    if date.month() == target_month {
                        if let Some(candidate) = at_time(date, rule.hour, rule.minute) {
                            if candidate > from_ms {
                                return Some(candidate);
                            }
                        }
                    }
                }
                offset += interval;
            }
            None
        }
    }
}

/// Calculate the next trigger by cronExpr or scheduleRule (automationCron.ts:343-350).
pub fn compute_automation_next_run_at(
    cron_expr: &str,
    schedule_rule: Option<&ScheduleRule>,
    from_ms: i64,
) -> Option<i64> {
    match schedule_rule {
        Some(rule) => compute_schedule_rule_next_run_at(rule, from_ms),
        None => CronExpr::parse(cron_expr).and_then(|cron| cron.next_run(from_ms)),
    }
}

/// Convert "a few minutes later" to a one-time rule anchored by the server's real clock (automationCron.ts:69-84).
pub fn build_relative_delay_schedule(
    delay_minutes: u32,
    from_ms: i64,
) -> Option<(String, ScheduleRule)> {
    let target = ms_to_naive_local(from_ms)? + Duration::minutes(delay_minutes as i64);
    let cron = format!(
        "{} {} {} {} *",
        target.minute(),
        target.hour(),
        target.day(),
        target.month(),
    );
    let rule = ScheduleRule {
        unit: crate::types::ScheduleUnit::Minute,
        interval: delay_minutes,
        hour: target.hour(),
        minute: target.minute(),
        anchor_at: from_ms,
        weekdays: None,
        month_days: None,
        months: None,
        monthly_mode: None,
    };
    Some((cron, rule))
}

/// The first trigger time of the new task (one-time fixed calendar staleness detection of automationCron.ts:90-121).
///
/// Pure one-time (non-cyclic and at most once) fixed month and day cron. If the target minute is just missed, the next time it will roll over to the next year;
/// Only the error span of <1min is allowed to be executed immediately, and the stale target is rejected (Err is returned).
pub fn compute_initial_next_run_at(
    cron_expr: &str,
    recurring: bool,
    max_runs: Option<u32>,
    schedule_rule: Option<&ScheduleRule>,
    from_ms: i64,
) -> Result<Option<i64>, String> {
    let next_run_at = compute_automation_next_run_at(cron_expr, schedule_rule, from_ms);
    let one_shot = !recurring && max_runs.unwrap_or(1) <= 1;
    // Fixed calendar cron: 5 segments of all numbers (such as "30 14 5 11 *"), excluding */, range, and list.
    let fixed_calendar = is_fixed_calendar_cron(cron_expr);
    if !one_shot || schedule_rule.is_some() || !fixed_calendar {
        return Ok(next_run_at);
    }
    let Some(cron) = CronExpr::parse(cron_expr) else {
        return Ok(next_run_at);
    };
    let previous_run = cron.prev_run(from_ms);
    let target_age = previous_run
        .filter(|prev| from_ms >= *prev && from_ms - prev <= ONE_SHOT_STALE_TARGET_WINDOW_MS)
        .map(|prev| from_ms - prev);
    let rolled_to_next =
        next_run_at.is_none_or(|next| next - from_ms > ONE_SHOT_STALE_TARGET_WINDOW_MS);
    if let Some(age) = target_age {
        if rolled_to_next {
            if age < ONE_SHOT_MISSED_RUN_GRACE_MS {
                return Ok(Some(from_ms));
            }
            return Err(format!(
                "The target time ({}) of the one-time scheduled task has passed; please use delayMinutes for relative time, and please confirm the future time and try again for absolute time",
                format_local(from_ms - age)
            ));
        }
    }
    Ok(next_run_at)
}

/// 5-segment fixed calendar cron with all numbers (excluding complex segments other than step/range/list/wildcard).
fn is_fixed_calendar_cron(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    // The first 4 paragraphs are pure numbers, and the 5th paragraph (day-of-week) is `*`.
    parts[..4].iter().all(|part| part.parse::<u32>().is_ok()) && parts[4] == "*"
}

/// Session side carrier normalized to authoritative scheduleRule (automationCron.ts:291-341).
pub fn build_interval_schedule_rule(
    unit: crate::types::ScheduleUnit,
    interval: u32,
    cron_expr: &str,
    anchor_at: i64,
) -> Option<ScheduleRule> {
    let anchor = ms_to_naive_local(anchor_at)?;
    let fields = parse_cron_fields(cron_expr);
    let minute = fields.minute.unwrap_or_else(|| anchor.minute());
    let hour = fields.hour.unwrap_or_else(|| anchor.hour());
    let month_days = fields.month_days.unwrap_or_else(|| vec![anchor.day()]);
    let months = fields.months.unwrap_or_else(|| vec![anchor.month()]);
    let weekdays = fields.weekdays.unwrap_or_else(|| vec![1]);
    let rule = match unit {
        crate::types::ScheduleUnit::Minute => ScheduleRule {
            unit,
            interval,
            hour: anchor.hour(),
            minute: anchor.minute(),
            anchor_at,
            weekdays: None,
            month_days: None,
            months: None,
            monthly_mode: None,
        },
        crate::types::ScheduleUnit::Hourly => ScheduleRule {
            unit,
            interval,
            hour: 0,
            minute,
            anchor_at,
            weekdays: None,
            month_days: None,
            months: None,
            monthly_mode: None,
        },
        crate::types::ScheduleUnit::Daily => ScheduleRule {
            unit,
            interval,
            hour,
            minute,
            anchor_at,
            weekdays: None,
            month_days: None,
            months: None,
            monthly_mode: None,
        },
        crate::types::ScheduleUnit::Weekly => ScheduleRule {
            unit,
            interval,
            hour,
            minute,
            anchor_at,
            weekdays: Some(weekdays),
            month_days: None,
            months: None,
            monthly_mode: None,
        },
        crate::types::ScheduleUnit::Monthly => ScheduleRule {
            unit,
            interval,
            hour,
            minute,
            anchor_at,
            weekdays: None,
            month_days: Some(month_days),
            months: None,
            monthly_mode: Some(crate::types::MonthlyMode::Date),
        },
        crate::types::ScheduleUnit::Yearly => ScheduleRule {
            unit,
            interval,
            hour,
            minute,
            anchor_at,
            weekdays: None,
            month_days: Some(month_days),
            months: Some(months),
            monthly_mode: None,
        },
    };
    Some(rule)
}

/// Recognize `*/N` minute cron as a rule with creation time anchor (automationCron.ts:223-236).
pub fn infer_minute_interval_schedule_rule(
    cron_expr: &str,
    anchor_at: i64,
) -> Option<ScheduleRule> {
    let expr = cron_expr.trim();
    let mut parts = expr.split_whitespace();
    let minute = parts.next()?;
    let rest: Vec<&str> = parts.collect();
    if rest.len() != 4 || rest.iter().any(|part| *part != "*") {
        return None;
    }
    let interval: u32 = minute.strip_prefix("*/")?.parse().ok()?;
    if interval == 0 {
        return None;
    }
    let anchor = ms_to_naive_local(anchor_at)?;
    Some(ScheduleRule {
        unit: crate::types::ScheduleUnit::Minute,
        interval,
        hour: anchor.hour(),
        minute: anchor.minute(),
        anchor_at,
        weekdays: None,
        month_days: None,
        months: None,
        monthly_mode: None,
    })
}

/// Pure numerical analysis results of each segment of cron (used for carrier normalization, automationCron.ts:243-277).
struct CronFields {
    minute: Option<u32>,
    hour: Option<u32>,
    month_days: Option<Vec<u32>>,
    months: Option<Vec<u32>>,
    weekdays: Option<Vec<u32>>,
}

fn parse_cron_fields(cron_expr: &str) -> CronFields {
    let parts: Vec<&str> = cron_expr.split_whitespace().collect();
    if parts.len() < 5 {
        return CronFields {
            minute: None,
            hour: None,
            month_days: None,
            months: None,
            weekdays: None,
        };
    }
    let parse_single = |token: &str| -> Option<u32> {
        if token == "*" || token == "?" {
            None
        } else {
            token.parse::<u32>().ok()
        }
    };
    let parse_list = |token: &str| -> Option<Vec<u32>> {
        if token == "*" || token == "?" {
            return None;
        }
        let values: Vec<u32> = token
            .split(',')
            .filter_map(|piece| piece.parse::<u32>().ok())
            .collect();
        if values.is_empty() {
            None
        } else {
            Some(values)
        }
    };
    CronFields {
        minute: parse_single(parts[0]),
        hour: parse_single(parts[1]),
        month_days: parse_list(parts[2]),
        months: parse_list(parts[3]),
        weekdays: parse_list(parts[4]),
    }
}

/// Adding a month (it is allowed to handle end-of-month overflow to the 1st of the next month - month advancement semantics).
fn add_months(date: NaiveDate, months: i64) -> Option<NaiveDate> {
    let total = (date.year() as i64) * 12 + (date.month() as i64 - 1) + months;
    let year = (total / 12) as i32;
    let month = (total % 12 + 1) as u32;
    NaiveDate::from_ymd_opt(year, month, 1)
}

/// The Nth weekday in a certain month (0=Sunday; automationCron.ts:127-131 firstWeekdayOfMonth).
fn first_weekday_of_month(year: i32, month: u32, weekday: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let first_dow = first.weekday().num_days_from_sunday();
    let offset = (weekday as i64 - first_dow as i64).rem_euclid(7) as u32;
    Some(first + Duration::days(offset as i64))
}

fn at_time(date: NaiveDate, hour: u32, minute: u32) -> Option<i64> {
    let naive = date.and_hms_opt(hour.min(23), minute.min(59), 0)?;
    naive_to_ms(naive)
}

fn format_local(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, m, d)
            .and_then(|date| date.and_hms_opt(h, mi, 0))
            .and_then(|naive| naive.and_local_timezone(Local).single())
            .expect("construct local time")
            .timestamp_millis()
    }

    #[test]
    fn is_valid_cron_accepts_standard_five_fields() {
        for expr in [
            "* * * * *",
            "30 9 * * 1-5",
            "0 */6 * * *",
            "15 14 1 * *",
            "0 0 1 1 *",
            "*/10 * * * *",
        ] {
            assert!(is_valid_cron(expr), "must accept {expr}");
        }
    }

    #[test]
    fn is_valid_cron_rejects_bad_fields() {
        for expr in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 32 * *",
            "* * * 13 *",
            "a b c d e",
            "* * * * 8",
        ] {
            assert!(!is_valid_cron(expr), "must reject {expr:?}");
        }
    }

    #[test]
    fn next_run_daily_at_specific_time() {
        // 0 9 * * * = 09:00 every day; departure from 08:00 → 09:00 that day.
        let cron = CronExpr::parse("0 9 * * *").expect("parse");
        let from = at(2026, 3, 10, 8, 0);
        assert_eq!(cron.next_run(from), Some(at(2026, 3, 10, 9, 0)));
        // Departs from 09:30 → 09:00 the next day (strictly later than from).
        let from = at(2026, 3, 10, 9, 30);
        assert_eq!(cron.next_run(from), Some(at(2026, 3, 11, 9, 0)));
    }

    #[test]
    fn next_run_uses_weekday_and_vixie_or_semantics() {
        // 0 9 * * 1 = Every Monday at 09:00.
        let cron = CronExpr::parse("0 9 * * 1").expect("parse");
        // 2026-03-11 is a Thursday; the next Monday is 03-16.
        let from = at(2026, 3, 11, 10, 0);
        assert_eq!(cron.next_run(from), Some(at(2026, 3, 16, 9, 0)));
        // Vixie OR: Day (15) + week (1) are limited at the same time, any one can be hit - 03-15 is Sunday, hit dom.
        let cron = CronExpr::parse("0 9 15 * 1").expect("parse");
        let from = at(2026, 3, 11, 10, 0);
        assert_eq!(cron.next_run(from), Some(at(2026, 3, 15, 9, 0)));
    }

    #[test]
    fn next_run_step_and_list_fields() {
        // */15 * * * * = 0/15/30/45 minutes per hour.
        let cron = CronExpr::parse("*/15 * * * *").expect("parse");
        let from = at(2026, 3, 10, 8, 7);
        assert_eq!(cron.next_run(from), Some(at(2026, 3, 10, 8, 15)));
        // Comma list + range: 0 9,17 * * 1-5 weekdays 9:00 and 17:00.
        let cron = CronExpr::parse("0 9,17 * * 1-5").expect("parse");
        let from = at(2026, 3, 14, 10, 0); // Saturday
        assert_eq!(cron.next_run(from), Some(at(2026, 3, 16, 9, 0))); // Monday 9 o'clock
    }

    #[test]
    fn monthly_and_yearly_next_run() {
        // 0 9 15 * * = 09:00 on the 15th of every month.
        let cron = CronExpr::parse("0 9 15 * *").expect("parse");
        let from = at(2026, 3, 10, 0, 0);
        assert_eq!(cron.next_run(from), Some(at(2026, 3, 15, 9, 0)));
        // 0 9 1 1 * = 09:00 on January 1 every year.
        let cron = CronExpr::parse("0 9 1 1 *").expect("parse");
        let from = at(2026, 3, 10, 0, 0);
        assert_eq!(cron.next_run(from), Some(at(2027, 1, 1, 9, 0)));
    }

    #[test]
    fn prev_run_is_symmetric_to_next() {
        let cron = CronExpr::parse("0 9 * * *").expect("parse");
        let from = at(2026, 3, 10, 10, 0);
        assert_eq!(cron.prev_run(from), Some(at(2026, 3, 10, 9, 0)));
        let from = at(2026, 3, 10, 8, 0);
        assert_eq!(cron.prev_run(from), Some(at(2026, 3, 9, 9, 0)));
    }

    #[test]
    fn minute_rule_advances_from_anchor_not_wall_clock() {
        // Every 10 minutes, anchor 10:03 → 10:13, 10:23... (not aligned with wall clock 10:10).
        let anchor = at(2026, 3, 10, 10, 3);
        let rule = ScheduleRule {
            unit: crate::types::ScheduleUnit::Minute,
            interval: 10,
            hour: 10,
            minute: 3,
            anchor_at: anchor,
            weekdays: None,
            month_days: None,
            months: None,
            monthly_mode: None,
        };
        assert_eq!(
            compute_schedule_rule_next_run_at(&rule, at(2026, 3, 10, 10, 4)),
            Some(at(2026, 3, 10, 10, 13))
        );
    }

    #[test]
    fn relative_delay_builds_anchor_rule() {
        let from = at(2026, 3, 10, 10, 0);
        let (cron, rule) = build_relative_delay_schedule(5, from).expect("build");
        assert_eq!(rule.unit, crate::types::ScheduleUnit::Minute);
        assert_eq!(rule.interval, 5);
        assert_eq!(rule.anchor_at, from);
        assert!(
            is_valid_cron(&cron),
            "The generated compatible cron must be legal: {cron}"
        );
    }

    #[test]
    fn infer_minute_interval_detects_star_n() {
        let anchor = at(2026, 3, 10, 10, 3);
        let rule = infer_minute_interval_schedule_rule("*/10 * * * *", anchor).expect("infer");
        assert_eq!(rule.interval, 10);
        assert_eq!(rule.anchor_at, anchor);
        assert!(infer_minute_interval_schedule_rule("0 9 * * *", anchor).is_none());
    }

    #[test]
    fn schedule_rule_definition_ignores_anchor_but_sorts_lists() {
        let a = ScheduleRule {
            unit: crate::types::ScheduleUnit::Weekly,
            interval: 1,
            hour: 9,
            minute: 0,
            anchor_at: 1,
            weekdays: Some(vec![3, 1]),
            month_days: None,
            months: None,
            monthly_mode: None,
        };
        let mut b = a.clone();
        b.anchor_at = 999;
        b.weekdays = Some(vec![1, 3]);
        assert_eq!(schedule_rule_definition(&a), schedule_rule_definition(&b));
    }
}
