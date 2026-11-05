//! What an export from the battery is expected to bring.
//!
//! Model: from `start`, the inverter exports `power_w` to the grid while the
//! battery also covers the home (`home_load_w`), until the battery reaches
//! the target SOC. Battery energy available above the target is
//! `(soc - target) * capacity * efficiency`.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use crate::prices::export_value_pln;
use crate::time::{SLOT, floor_slot};

#[derive(Debug, Clone, PartialEq)]
pub struct ExportParams {
    pub start: DateTime<Utc>,
    pub soc_start: f64,
    pub target_soc: f64,
    pub capacity_kwh: f64,
    pub power_w: f64,
    pub home_load_w: f64,
    /// Battery discharge efficiency, 0..=1.
    pub efficiency: f64,
    pub price_factor: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedSlot {
    pub start: DateTime<Utc>,
    pub kwh: f64,
    /// PLN/MWh, if known.
    pub price: Option<f64>,
    pub income_pln: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub export_kwh: f64,
    pub end: DateTime<Utc>,
    pub income_pln: f64,
    /// True if some slots have no price yet (their income is not counted).
    pub missing_prices: bool,
    pub slots: Vec<ProjectedSlot>,
}

/// Hours the battery lasts and the energy exported, or `None` when there is
/// nothing to export.
fn duration_and_energy(p: &ExportParams) -> Option<(f64, f64)> {
    let battery_kwh = (p.soc_start - p.target_soc) / 100.0 * p.capacity_kwh * p.efficiency;
    let export_kw = p.power_w / 1000.0;
    let drain_kw = export_kw + p.home_load_w.max(0.0) / 1000.0;
    if battery_kwh <= 0.0 || export_kw <= 0.0 || !battery_kwh.is_finite() {
        return None;
    }
    let hours = battery_kwh / drain_kw;
    Some((hours, export_kw * hours))
}

pub fn project(p: &ExportParams, prices: &HashMap<DateTime<Utc>, f64>) -> Option<Projection> {
    let (hours, export_kwh) = duration_and_energy(p)?;
    let export_kw = p.power_w / 1000.0;
    let length_s = hours * 3600.0;
    let end = p.start + Duration::milliseconds((length_s * 1000.0).round() as i64);

    let mut slots = Vec::new();
    let mut income_pln = 0.0;
    let mut missing_prices = false;
    let mut slot = floor_slot(p.start);
    while slot < end {
        // Offsets from the start in seconds, exact enough to add up.
        let from = (slot - p.start).num_milliseconds() as f64 / 1000.0;
        let to = from + SLOT.num_seconds() as f64;
        let seconds = to.min(length_s) - from.max(0.0);
        let kwh = export_kw * seconds / 3600.0;
        let price = prices.get(&slot).copied();
        let income = price.map(|price| export_value_pln(kwh, price, p.price_factor));
        match income {
            Some(v) => income_pln += v,
            None => missing_prices = true,
        }
        slots.push(ProjectedSlot {
            start: slot,
            kwh,
            price,
            income_pln: income,
        });
        slot += SLOT;
    }
    Some(Projection {
        export_kwh,
        end,
        income_pln,
        missing_prices,
        slots,
    })
}

/// The start (on the slot grid, within `[from, until - duration]`) whose
/// export earns the most, using only slots with known prices.
pub fn best_start(
    p: &ExportParams,
    prices: &HashMap<DateTime<Utc>, f64>,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Option<(DateTime<Utc>, Projection)> {
    let mut start = floor_slot(from);
    if start < from {
        start += SLOT;
    }
    let mut best: Option<(DateTime<Utc>, Projection)> = None;
    while start < until {
        let candidate = ExportParams { start, ..p.clone() };
        let proj = project(&candidate, prices)?;
        if proj.end > until {
            break;
        }
        let better = best
            .as_ref()
            .is_none_or(|(_, b)| proj.income_pln > b.income_pln + 1e-9);
        if !proj.missing_prices && better {
            best = Some((start, proj));
        }
        start += SLOT;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn params() -> ExportParams {
        ExportParams {
            start: utc("2026-11-05T17:00:00Z"),
            soc_start: 80.0,
            target_soc: 30.0,
            capacity_kwh: 16.1,
            power_w: 4000.0,
            home_load_w: 0.0,
            efficiency: 0.95,
            price_factor: 1.0,
        }
    }

    fn flat(price: f64, from: &str, slots: i32) -> HashMap<DateTime<Utc>, f64> {
        let start = utc(from);
        (0..slots).map(|i| (start + SLOT * i, price)).collect()
    }

    #[test]
    fn exports_the_energy_above_target() {
        let prices = flat(500.0, "2026-11-05T17:00:00Z", 16);
        let p = project(&params(), &prices).unwrap();
        // 50% of 16.1 kWh at 95%: 7.6475 kWh, at 4 kW: 1.911875 h
        assert!((p.export_kwh - 7.6475).abs() < 1e-9);
        assert_eq!(
            p.end,
            utc("2026-11-05T17:00:00Z") + Duration::milliseconds(6_882_750)
        );
        assert!((p.income_pln - 7.6475 * 0.5).abs() < 1e-9);
        assert!(!p.missing_prices);
        assert_eq!(p.slots.len(), 8);
        assert!(
            (p.slots[0].kwh - 1.0).abs() < 1e-9,
            "a full slot at 4 kW is 1 kWh"
        );
        let total: f64 = p.slots.iter().map(|s| s.kwh).sum();
        assert!((total - p.export_kwh).abs() < 1e-9);
    }

    #[test]
    fn home_load_shortens_the_export() {
        let p = ExportParams {
            home_load_w: 500.0,
            ..params()
        };
        let proj = project(&p, &HashMap::new()).unwrap();
        // 7.6475 kWh feeds 4.5 kW: 1.6994 h, of which 4 kW is exported.
        assert!((proj.export_kwh - 7.6475 * 4.0 / 4.5).abs() < 1e-9);
        assert!(proj.missing_prices);
        assert_eq!(proj.income_pln, 0.0);
    }

    #[test]
    fn partial_first_slot() {
        let p = ExportParams {
            start: utc("2026-11-05T17:07:00Z"),
            ..params()
        };
        let proj = project(&p, &HashMap::new()).unwrap();
        assert_eq!(proj.slots[0].start, utc("2026-11-05T17:00:00Z"));
        assert!((proj.slots[0].kwh - 4.0 * 8.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn negative_prices_earn_nothing() {
        let mut prices = flat(500.0, "2026-11-05T17:00:00Z", 16);
        prices.insert(utc("2026-11-05T17:00:00Z"), -20.0);
        let proj = project(&params(), &prices).unwrap();
        assert_eq!(proj.slots[0].income_pln, Some(0.0));
        assert!((proj.income_pln - (7.6475 - 1.0) * 0.5).abs() < 1e-9);
    }

    #[test]
    fn nothing_to_export_at_or_below_target() {
        let p = ExportParams {
            soc_start: 30.0,
            ..params()
        };
        assert_eq!(project(&p, &HashMap::new()), None);
        let p = ExportParams {
            power_w: 0.0,
            ..params()
        };
        assert_eq!(project(&p, &HashMap::new()), None);
    }

    #[test]
    fn finds_the_best_window() {
        // Cheap all evening except an expensive hour at 19:00-20:00 UTC.
        let mut prices = flat(300.0, "2026-11-05T15:00:00Z", 32);
        for (t, price) in flat(900.0, "2026-11-05T19:00:00Z", 4) {
            prices.insert(t, price);
        }
        // 1 hour of export: 20% of 20 kWh at 100%, at 4 kW.
        let p = ExportParams {
            soc_start: 50.0,
            capacity_kwh: 20.0,
            efficiency: 1.0,
            ..params()
        };
        let (start, proj) = best_start(
            &p,
            &prices,
            utc("2026-11-05T15:00:00Z"),
            utc("2026-11-05T23:00:00Z"),
        )
        .unwrap();
        assert_eq!(start, utc("2026-11-05T19:00:00Z"));
        assert!((proj.income_pln - 4.0 * 0.9).abs() < 1e-9);
    }

    #[test]
    fn best_window_must_fit() {
        let prices = flat(300.0, "2026-11-05T15:00:00Z", 4);
        let p = params(); // ~1.9 h
        assert!(
            best_start(
                &p,
                &prices,
                utc("2026-11-05T15:00:00Z"),
                utc("2026-11-05T16:00:00Z")
            )
            .is_none()
        );
    }
}
