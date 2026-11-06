//! Export plans: when to start and stop them, and what to write.
//!
//! An export uses Deye's "forced discharge" recipe: every time-of-use slot
//! gets the export power, the stop SOC and no grid charging; then solar sell
//! and time of use are switched on and the work mode becomes "Selling first".
//! The bridge answers each command with the values it read before writing;
//! those are written back when the export ends.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

/// Wait this long before retrying a failed start or finish.
pub const RETRY_AFTER: Duration = Duration::seconds(60);
/// Longest export a plan may cover.
pub const MAX_PLAN: Duration = Duration::hours(24);
/// How far in the past a new plan may start (the form is filled in slowly).
pub const START_GRACE: Duration = Duration::minutes(2);
/// Export power steps (the inverter stores 10 W steps).
pub const POWER_STEP_W: i32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Scheduled,
    Active,
    Done,
    Cancelled,
    Failed,
    Missed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Scheduled => "scheduled",
            Status::Active => "active",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
            Status::Failed => "failed",
            Status::Missed => "missed",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        [
            Status::Scheduled,
            Status::Active,
            Status::Done,
            Status::Cancelled,
            Status::Failed,
            Status::Missed,
        ]
        .into_iter()
        .find(|st| st.as_str() == s)
    }

    pub fn is_open(self) -> bool {
        matches!(self, Status::Scheduled | Status::Active)
    }
}

/// The parts of a plan the decisions need.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub id: i64,
    pub start: DateTime<Utc>,
    /// Latest end: the export is stopped then even if the SOC is not reached.
    pub end: DateTime<Utc>,
    pub target_soc: i32,
    pub power_w: i32,
    pub status: Status,
    pub cancel_requested: bool,
    /// Set after a failed attempt; nothing is tried before then.
    pub retry_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    TargetReached,
    EndTime,
    Cancelled,
}

impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FinishReason::TargetReached => "battery reached the target SOC",
            FinishReason::EndTime => "latest end time reached",
            FinishReason::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Apply the export settings.
    Start(i64),
    /// Restore the settings saved at the start.
    Finish(i64, FinishReason),
    /// Close a plan that never started.
    Close(i64, Status, &'static str),
}

/// What to do now. `soc` is the live battery SOC, if fresh.
pub fn decide(now: DateTime<Utc>, plans: &[Plan], soc: Option<f64>) -> Vec<Action> {
    let waiting = |p: &Plan| p.retry_after.is_some_and(|t| now < t);
    let at_target = |p: &Plan| soc.is_some_and(|soc| soc <= f64::from(p.target_soc));
    let mut actions = Vec::new();
    let mut busy = plans.iter().any(|p| p.status == Status::Active);

    for p in plans.iter().filter(|p| p.status == Status::Active) {
        if waiting(p) {
            continue;
        }
        let reason = if p.cancel_requested {
            Some(FinishReason::Cancelled)
        } else if at_target(p) {
            Some(FinishReason::TargetReached)
        } else if now >= p.end {
            Some(FinishReason::EndTime)
        } else {
            None
        };
        if let Some(reason) = reason {
            actions.push(Action::Finish(p.id, reason));
        }
    }

    let mut scheduled: Vec<&Plan> = plans
        .iter()
        .filter(|p| p.status == Status::Scheduled)
        .collect();
    scheduled.sort_by_key(|p| p.start);
    for p in scheduled {
        if waiting(p) {
            continue;
        }
        if p.cancel_requested {
            actions.push(Action::Close(
                p.id,
                Status::Cancelled,
                "cancelled before it started",
            ));
        } else if now >= p.end {
            actions.push(Action::Close(
                p.id,
                Status::Missed,
                "the latest end passed before it could start",
            ));
        } else if now >= p.start && !busy {
            if at_target(p) {
                actions.push(Action::Close(
                    p.id,
                    Status::Done,
                    "battery already at or below the target SOC",
                ));
            } else {
                actions.push(Action::Start(p.id));
                busy = true;
            }
        }
    }
    actions
}

/// Settings written to start an export, in order: the work mode last, so the
/// inverter only starts selling once everything else is in place.
pub fn export_writes(target_soc: i32, power_w: i32) -> Vec<(String, f64)> {
    let power = f64::from(power_w);
    let mut w = Vec::with_capacity(22);
    for slot in 1..=6 {
        w.push((format!("tou_charge_{slot}"), 0.0));
        w.push((format!("tou_soc_{slot}"), f64::from(target_soc)));
        w.push((format!("tou_power_{slot}"), power));
    }
    w.push(("max_sell_power".into(), power));
    w.push(("solar_sell".into(), 1.0));
    w.push(("tou_selling".into(), 255.0));
    w.push(("work_mode".into(), 0.0));
    w
}

/// Every setting an export changes.
pub fn export_keys() -> Vec<String> {
    export_writes(0, 0).into_iter().map(|(k, _)| k).collect()
}

/// Everything the inverter settings view shows: what an export changes, plus
/// the time-of-use slot start times.
pub fn display_keys() -> Vec<String> {
    let mut keys = export_keys();
    keys.extend((1..=6).map(|slot| format!("tou_time_{slot}")));
    keys
}

/// Writes that put back the values read before the export: the work mode
/// first, so selling stops before anything else changes.
pub fn restore_writes(before: &BTreeMap<String, f64>) -> Vec<(String, f64)> {
    let mut keys = export_keys();
    keys.rotate_right(1); // work_mode is last in an export; first here
    keys.into_iter()
        .filter_map(|k| before.get(&k).map(|v| (k, *v)))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub min_soc: i32,
    pub max_power_w: i32,
}

/// Checks a new plan. `existing` are the plans already stored.
pub fn validate(
    now: DateTime<Utc>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    target_soc: i32,
    power_w: i32,
    limits: Limits,
    existing: &[Plan],
) -> Result<(), String> {
    if target_soc < limits.min_soc || target_soc > 99 {
        return Err(format!(
            "stop SOC must be between {}% and 99%",
            limits.min_soc
        ));
    }
    if power_w < POWER_STEP_W || power_w > limits.max_power_w {
        return Err(format!(
            "export power must be between {POWER_STEP_W} and {} W",
            limits.max_power_w
        ));
    }
    if power_w % POWER_STEP_W != 0 {
        return Err(format!(
            "export power must be a multiple of {POWER_STEP_W} W"
        ));
    }
    if start < now - START_GRACE {
        return Err("the start is in the past".into());
    }
    if end <= start {
        return Err("the latest end must be after the start".into());
    }
    if end - start > MAX_PLAN {
        return Err("a plan may cover at most 24 hours".into());
    }
    if let Some(other) = existing
        .iter()
        .find(|p| p.status.is_open() && p.start < end && start < p.end)
    {
        return Err(format!("it overlaps plan #{}", other.id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const START: &str = "2026-11-05T17:00:00Z";

    fn plan(status: Status) -> Plan {
        Plan {
            id: 1,
            start: utc(START),
            end: utc(START) + Duration::hours(4),
            target_soc: 30,
            power_w: 4000,
            status,
            cancel_requested: false,
            retry_after: None,
        }
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        utc(START) + Duration::minutes(minutes)
    }

    #[test]
    fn waits_for_the_start() {
        assert!(decide(at(-1), &[plan(Status::Scheduled)], Some(80.0)).is_empty());
    }

    #[test]
    fn starts_when_due() {
        assert_eq!(
            decide(at(0), &[plan(Status::Scheduled)], Some(80.0)),
            [Action::Start(1)]
        );
        // A late start (e.g. after a restart) still happens before the end.
        assert_eq!(
            decide(at(90), &[plan(Status::Scheduled)], None),
            [Action::Start(1)]
        );
    }

    #[test]
    fn skips_when_already_at_target() {
        assert_eq!(
            decide(at(0), &[plan(Status::Scheduled)], Some(30.0)),
            [Action::Close(
                1,
                Status::Done,
                "battery already at or below the target SOC"
            )]
        );
    }

    #[test]
    fn misses_after_the_end() {
        assert!(matches!(
            decide(at(240), &[plan(Status::Scheduled)], Some(80.0))[..],
            [Action::Close(1, Status::Missed, _)]
        ));
    }

    #[test]
    fn cancelling_a_scheduled_plan_touches_nothing() {
        let p = Plan {
            cancel_requested: true,
            ..plan(Status::Scheduled)
        };
        assert!(matches!(
            decide(at(-30), &[p], Some(80.0))[..],
            [Action::Close(1, Status::Cancelled, _)]
        ));
    }

    #[test]
    fn retries_wait() {
        let p = Plan {
            retry_after: Some(at(1)),
            ..plan(Status::Scheduled)
        };
        assert!(decide(at(0), std::slice::from_ref(&p), Some(80.0)).is_empty());
        assert_eq!(decide(at(1), &[p], Some(80.0)), [Action::Start(1)]);

        let p = Plan {
            retry_after: Some(at(31)),
            ..plan(Status::Active)
        };
        assert!(decide(at(30), &[p], Some(10.0)).is_empty());

        // Closing may need a restore, which waits too.
        let p = Plan {
            retry_after: Some(at(241)),
            ..plan(Status::Scheduled)
        };
        assert!(decide(at(240), &[p], Some(80.0)).is_empty());
    }

    #[test]
    fn finishes_active_exports() {
        let active = plan(Status::Active);
        assert!(decide(at(30), std::slice::from_ref(&active), Some(50.0)).is_empty());
        assert!(
            decide(at(30), std::slice::from_ref(&active), None).is_empty(),
            "no SOC: keep going"
        );
        assert_eq!(
            decide(at(30), std::slice::from_ref(&active), Some(30.0)),
            [Action::Finish(1, FinishReason::TargetReached)]
        );
        assert_eq!(
            decide(at(240), std::slice::from_ref(&active), Some(50.0)),
            [Action::Finish(1, FinishReason::EndTime)]
        );
        let cancelled = Plan {
            cancel_requested: true,
            ..active
        };
        assert_eq!(
            decide(at(30), &[cancelled], Some(50.0)),
            [Action::Finish(1, FinishReason::Cancelled)]
        );
    }

    #[test]
    fn one_export_at_a_time() {
        let active = plan(Status::Active);
        let next = Plan {
            id: 2,
            start: at(10),
            ..plan(Status::Scheduled)
        };
        assert!(decide(at(20), &[active, next], Some(50.0)).is_empty());
    }

    #[test]
    fn closed_plans_are_ignored() {
        for st in [
            Status::Done,
            Status::Cancelled,
            Status::Failed,
            Status::Missed,
        ] {
            assert!(decide(at(0), &[plan(st)], Some(80.0)).is_empty(), "{st:?}");
        }
    }

    #[test]
    fn export_writes_set_every_slot_and_switch_mode_last() {
        let w = export_writes(25, 4000);
        assert_eq!(w.len(), 22);
        assert_eq!(w.last().unwrap(), &("work_mode".to_owned(), 0.0));
        let get = |k: &str| w.iter().find(|(key, _)| key == k).map(|(_, v)| *v);
        for slot in 1..=6 {
            assert_eq!(get(&format!("tou_power_{slot}")), Some(4000.0));
            assert_eq!(get(&format!("tou_soc_{slot}")), Some(25.0));
            assert_eq!(get(&format!("tou_charge_{slot}")), Some(0.0));
        }
        assert_eq!(get("max_sell_power"), Some(4000.0));
        assert_eq!(get("solar_sell"), Some(1.0));
        assert_eq!(get("tou_selling"), Some(255.0));

        let mut keys: Vec<String> = w.into_iter().map(|(k, _)| k).collect();
        let mut all = export_keys();
        keys.sort();
        all.sort();
        assert_eq!(keys, all);
    }

    #[test]
    fn restore_switches_mode_first() {
        let before: BTreeMap<String, f64> = export_keys()
            .into_iter()
            .map(|k| {
                let v = if k == "work_mode" { 2.0 } else { 1.0 };
                (k, v)
            })
            .collect();
        let w = restore_writes(&before);
        assert_eq!(w.len(), 22);
        assert_eq!(w[0], ("work_mode".to_owned(), 2.0));
        assert!(w[1..].iter().all(|(_, v)| *v == 1.0));
    }

    #[test]
    fn restore_writes_only_what_was_read() {
        let before = BTreeMap::from([("work_mode".to_owned(), 2.0), ("unrelated".to_owned(), 5.0)]);
        assert_eq!(restore_writes(&before), [("work_mode".to_owned(), 2.0)]);
    }

    #[test]
    fn validates_new_plans() {
        let limits = Limits {
            min_soc: 10,
            max_power_w: 4000,
        };
        let now = at(-60);
        let ok = |start, end, soc, power, existing: &[Plan]| {
            validate(now, start, end, soc, power, limits, existing)
        };
        assert_eq!(ok(at(0), at(240), 30, 4000, &[]), Ok(()));
        assert!(
            ok(at(0), at(240), 9, 4000, &[])
                .unwrap_err()
                .contains("SOC")
        );
        assert!(
            ok(at(0), at(240), 100, 4000, &[])
                .unwrap_err()
                .contains("SOC")
        );
        assert!(
            ok(at(0), at(240), 30, 4100, &[])
                .unwrap_err()
                .contains("4000")
        );
        assert!(ok(at(0), at(240), 30, 0, &[]).is_err());
        assert!(
            ok(at(0), at(240), 30, 3050, &[])
                .unwrap_err()
                .contains("100 W")
        );
        assert!(
            ok(at(-63), at(240), 30, 4000, &[])
                .unwrap_err()
                .contains("past")
        );
        assert!(
            ok(at(0), at(0), 30, 4000, &[])
                .unwrap_err()
                .contains("after")
        );
        assert!(
            ok(at(0), at(0) + Duration::hours(25), 30, 4000, &[])
                .unwrap_err()
                .contains("24")
        );

        let other = Plan {
            id: 7,
            start: at(60),
            end: at(120),
            ..plan(Status::Scheduled)
        };
        assert!(
            ok(at(0), at(61), 30, 4000, std::slice::from_ref(&other))
                .unwrap_err()
                .contains("overlaps")
        );
        assert_eq!(
            ok(at(0), at(60), 30, 4000, std::slice::from_ref(&other)),
            Ok(()),
            "touching is fine"
        );
        let done = Plan {
            status: Status::Done,
            ..other
        };
        assert_eq!(ok(at(0), at(240), 30, 4000, &[done]), Ok(()));
    }

    #[test]
    fn status_round_trip() {
        for st in [
            Status::Scheduled,
            Status::Active,
            Status::Done,
            Status::Cancelled,
            Status::Failed,
            Status::Missed,
        ] {
            assert_eq!(Status::parse(st.as_str()), Some(st));
        }
        assert_eq!(Status::parse("nope"), None);
    }
}
