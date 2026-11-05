//! Energy accounting from cumulative counters (kWh), in local-time buckets.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, Utc};
use serde::Deserialize;

use crate::prices::export_value_pln;
use crate::time::{self, SLOT, TZ};

/// The energy counters the charts use. Grid flows come from the DTSU666
/// meter; everything else from the inverter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    Import,
    Export,
    Pv,
    Charge,
    Discharge,
    Load,
}

impl Metric {
    pub const ALL: [Metric; 6] = [
        Metric::Import,
        Metric::Export,
        Metric::Pv,
        Metric::Charge,
        Metric::Discharge,
        Metric::Load,
    ];

    /// VictoriaMetrics series name (`<device>_<key>`).
    pub fn series(self) -> &'static str {
        match self {
            Metric::Import => "dtsu_grid_import_energy",
            Metric::Export => "dtsu_grid_export_energy",
            Metric::Pv => "deye_pv_energy",
            Metric::Charge => "deye_battery_charge_energy",
            Metric::Discharge => "deye_battery_discharge_energy",
            Metric::Load => "deye_load_energy",
        }
    }

    pub fn from_series(name: &str) -> Option<Metric> {
        Metric::ALL.into_iter().find(|m| m.series() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    pub label: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Period {
    /// Hours of a day, days of a week (Monday first) or month, months of a
    /// year, all in local time.
    pub fn buckets(self, anchor: NaiveDate) -> Vec<Bucket> {
        let day = |date: NaiveDate, label: String| {
            let (start, end) = time::day_bounds(date);
            Bucket { label, start, end }
        };
        match self {
            Period::Day => {
                // Warsaw's offset is whole hours: UTC hours are local hours.
                let (start, end) = time::day_bounds(anchor);
                let mut out = Vec::with_capacity(25);
                let mut t = start;
                while t < end {
                    let label = t.with_timezone(&TZ).format("%H:00").to_string();
                    out.push(Bucket {
                        label,
                        start: t,
                        end: t + Duration::hours(1),
                    });
                    t += Duration::hours(1);
                }
                out
            }
            Period::Week => {
                let monday =
                    anchor - Duration::days(i64::from(anchor.weekday().num_days_from_monday()));
                (0..7)
                    .map(|i| {
                        let date = monday + Duration::days(i);
                        day(date, date.format("%a %-d").to_string())
                    })
                    .collect()
            }
            Period::Month => {
                let first = anchor.with_day(1).expect("day 1 exists");
                first
                    .iter_days()
                    .take_while(|d| d.month() == first.month())
                    .map(|date| day(date, date.day().to_string()))
                    .collect()
            }
            Period::Year => (1..=12)
                .map(|m| {
                    let first = NaiveDate::from_ymd_opt(anchor.year(), m, 1).expect("valid month");
                    let next = first + Months::new(1);
                    Bucket {
                        label: first.format("%b").to_string(),
                        start: time::day_start(first),
                        end: time::day_start(next),
                    }
                })
                .collect(),
        }
    }

    /// Start and end of the whole period.
    pub fn range(self, anchor: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
        let b = self.buckets(anchor);
        (b[0].start, b[b.len() - 1].end)
    }

    /// The anchor `n` periods later (or earlier, for negative `n`).
    pub fn shift(self, anchor: NaiveDate, n: i32) -> NaiveDate {
        let months = |k: i32| {
            let m = Months::new(k.unsigned_abs());
            if k >= 0 { anchor + m } else { anchor - m }
        };
        match self {
            Period::Day => anchor + Duration::days(i64::from(n)),
            Period::Week => anchor + Duration::weeks(i64::from(n)),
            Period::Month => months(n),
            Period::Year => months(n * 12),
        }
    }
}

/// Energy per flow over some time, in kWh, and the export's value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub import: f64,
    pub export: f64,
    pub pv: f64,
    pub charge: f64,
    pub discharge: f64,
    pub load: f64,
    /// Value of the export at RCE prices, PLN.
    pub earnings_pln: f64,
    /// Export in slots without a known price (not in `earnings_pln`).
    pub unpriced_export: f64,
}

/// Below this, ratios are meaningless.
const MIN_KWH: f64 = 0.01;

impl Totals {
    pub fn get(&self, m: Metric) -> f64 {
        match m {
            Metric::Import => self.import,
            Metric::Export => self.export,
            Metric::Pv => self.pv,
            Metric::Charge => self.charge,
            Metric::Discharge => self.discharge,
            Metric::Load => self.load,
        }
    }

    fn add(&mut self, m: Metric, kwh: f64) {
        let slot = match m {
            Metric::Import => &mut self.import,
            Metric::Export => &mut self.export,
            Metric::Pv => &mut self.pv,
            Metric::Charge => &mut self.charge,
            Metric::Discharge => &mut self.discharge,
            Metric::Load => &mut self.load,
        };
        *slot += kwh;
    }

    pub fn merge(&mut self, other: &Totals) {
        for m in Metric::ALL {
            self.add(m, other.get(m));
        }
        self.earnings_pln += other.earnings_pln;
        self.unpriced_export += other.unpriced_export;
    }

    /// The inverter's own use and losses: energy in (solar, battery, grid)
    /// minus energy out (home, battery, grid).
    pub fn own_use(&self) -> f64 {
        let energy_in = self.pv + self.discharge + self.import;
        let energy_out = self.charge + self.export + self.load;
        (energy_in - energy_out).max(0.0)
    }

    /// Share of solar energy used at home (directly or via the battery)
    /// rather than exported.
    pub fn utilization(&self) -> Option<f64> {
        (self.pv >= MIN_KWH).then(|| ((self.pv - self.export) / self.pv).clamp(0.0, 1.0))
    }

    /// Battery round trip: energy out per energy in.
    pub fn efficiency(&self) -> Option<f64> {
        (self.charge >= MIN_KWH).then(|| self.discharge / self.charge)
    }
}

/// Counter readings at `start + i * SLOT` for i = 0..=n, per metric.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    pub start: DateTime<Utc>,
    pub series: HashMap<Metric, Vec<Option<f64>>>,
}

/// Energy per slot from counter readings at slot boundaries. A missing
/// reading carries the last known one forward, so energy across a gap lands
/// in the slot where readings resume. A counter going backwards (reset or
/// replaced device) counts as zero and becomes the new baseline.
pub fn slot_deltas(values: &[Option<f64>]) -> Vec<f64> {
    let mut last = values.first().copied().flatten();
    values
        .iter()
        .skip(1)
        .map(|v| match (*v, last) {
            (Some(now), Some(before)) => {
                last = Some(now);
                (now - before).max(0.0)
            }
            (Some(now), None) => {
                last = Some(now);
                0.0
            }
            (None, _) => 0.0,
        })
        .collect()
}

/// Adds up slot energy into buckets, valuing each slot's export at that
/// slot's price (PLN/MWh, keyed by slot start).
pub fn aggregate(
    counters: &Counters,
    prices: &HashMap<DateTime<Utc>, f64>,
    buckets: &[Bucket],
    price_factor: f64,
) -> Vec<Totals> {
    let mut out = vec![Totals::default(); buckets.len()];
    for m in Metric::ALL {
        let Some(values) = counters.series.get(&m) else {
            continue;
        };
        for (i, kwh) in slot_deltas(values).into_iter().enumerate() {
            if kwh == 0.0 {
                continue;
            }
            let slot = counters.start + SLOT * i as i32;
            let Some(b) = buckets.iter().position(|b| b.start <= slot && slot < b.end) else {
                continue;
            };
            out[b].add(m, kwh);
            if m == Metric::Export {
                match prices.get(&slot) {
                    Some(&price) => {
                        out[b].earnings_pln += export_value_pln(kwh, price, price_factor)
                    }
                    None => out[b].unpriced_export += kwh,
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn deltas_between_readings() {
        let v = [Some(1.0), Some(1.5), Some(1.5), Some(2.0)];
        assert_eq!(slot_deltas(&v), [0.5, 0.0, 0.5]);
    }

    #[test]
    fn deltas_across_gaps_land_where_readings_resume() {
        let v = [Some(1.0), None, Some(2.0)];
        assert_eq!(slot_deltas(&v), [0.0, 1.0]);
        let v = [None, Some(5.0), Some(6.0)];
        assert_eq!(slot_deltas(&v), [0.0, 1.0]);
        assert_eq!(slot_deltas(&[None, None]), [0.0]);
        assert!(slot_deltas(&[Some(1.0)]).is_empty());
        assert!(slot_deltas(&[]).is_empty());
    }

    #[test]
    fn counter_reset_counts_as_zero() {
        let v = [Some(10.0), Some(0.5), Some(1.0)];
        assert_eq!(slot_deltas(&v), [0.0, 0.5]);
    }

    #[test]
    fn day_has_hour_buckets() {
        let b = Period::Day.buckets(d(2026, 11, 5));
        assert_eq!(b.len(), 24);
        assert_eq!(b[0].label, "00:00");
        assert_eq!(b[0].start, utc("2026-11-04T23:00:00Z"));
        assert_eq!(b[23].label, "23:00");
        assert_eq!(b[23].end, utc("2026-11-05T23:00:00Z"));
        assert!(b.windows(2).all(|w| w[0].end == w[1].start));
    }

    #[test]
    fn dst_days_have_23_or_25_hours() {
        let b = Period::Day.buckets(d(2026, 10, 25));
        assert_eq!(b.len(), 25);
        let labels: Vec<&str> = b.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(&labels[..4], ["00:00", "01:00", "02:00", "02:00"]);
        assert_eq!(Period::Day.buckets(d(2026, 3, 29)).len(), 23);
    }

    #[test]
    fn week_starts_on_monday() {
        let b = Period::Week.buckets(d(2026, 11, 5)); // a Thursday
        assert_eq!(b.len(), 7);
        assert_eq!(b[0].label, "Mon 2");
        assert_eq!(b[0].start, time::day_start(d(2026, 11, 2)));
        assert_eq!(b[6].label, "Sun 8");
        assert_eq!(b[6].end, time::day_start(d(2026, 11, 9)));
    }

    #[test]
    fn month_and_year() {
        let b = Period::Month.buckets(d(2028, 2, 10));
        assert_eq!(b.len(), 29);
        assert_eq!(b[0].label, "1");
        assert_eq!(b[28].end, time::day_start(d(2028, 3, 1)));

        let b = Period::Year.buckets(d(2026, 11, 5));
        assert_eq!(b.len(), 12);
        assert_eq!(b[0].label, "Jan");
        assert_eq!(b[0].start, time::day_start(d(2026, 1, 1)));
        assert_eq!(b[11].end, time::day_start(d(2027, 1, 1)));
        assert_eq!(Period::Year.range(d(2026, 5, 5)), (b[0].start, b[11].end));
    }

    #[test]
    fn shifting_periods() {
        assert_eq!(Period::Day.shift(d(2026, 11, 5), 1), d(2026, 11, 6));
        assert_eq!(Period::Week.shift(d(2026, 11, 5), -1), d(2026, 10, 29));
        assert_eq!(Period::Month.shift(d(2026, 3, 31), -1), d(2026, 2, 28));
        assert_eq!(Period::Year.shift(d(2028, 2, 29), 1), d(2029, 2, 28));
    }

    #[test]
    fn derived_figures() {
        let t = Totals {
            import: 2.0,
            export: 3.0,
            pv: 20.0,
            charge: 8.0,
            discharge: 7.2,
            load: 16.0,
            ..Totals::default()
        };
        // in: 20 + 7.2 + 2 = 29.2, out: 8 + 3 + 16 = 27
        assert!((t.own_use() - 2.2).abs() < 1e-9);
        assert!((t.utilization().unwrap() - 0.85).abs() < 1e-9);
        assert!((t.efficiency().unwrap() - 0.9).abs() < 1e-9);

        let night = Totals {
            import: 1.0,
            load: 1.2,
            ..Totals::default()
        };
        assert_eq!(night.utilization(), None);
        assert_eq!(night.efficiency(), None);
        assert_eq!(night.own_use(), 0.0, "never negative");
    }

    #[test]
    fn aggregates_slots_into_buckets_with_earnings() {
        let start = utc("2026-11-04T23:00:00Z");
        let buckets = vec![
            Bucket {
                label: "00:00".into(),
                start,
                end: start + Duration::hours(1),
            },
            Bucket {
                label: "01:00".into(),
                start: start + Duration::hours(1),
                end: start + Duration::hours(2),
            },
        ];
        // 8 slots -> 9 readings.
        let export = vec![
            Some(100.0),
            Some(100.5),
            Some(101.0),
            Some(101.0),
            Some(101.0),
            Some(102.0),
            Some(102.0),
            Some(102.0),
            Some(102.25),
        ];
        let pv: Vec<Option<f64>> = (0..9).map(|i| Some(f64::from(i))).collect();
        let counters = Counters {
            start,
            series: HashMap::from([(Metric::Export, export), (Metric::Pv, pv)]),
        };
        let mut prices = HashMap::new();
        for i in 0..8 {
            prices.insert(start + SLOT * i, 400.0);
        }
        prices.insert(start + SLOT, -50.0); // negative: worth nothing
        prices.remove(&(start + SLOT * 7)); // unknown

        let t = aggregate(&counters, &prices, &buckets, 1.0);
        assert_eq!(t.len(), 2);
        assert!((t[0].export - 1.0).abs() < 1e-9);
        assert!((t[0].pv - 4.0).abs() < 1e-9);
        // slot 0: 0.5 kWh at 400 = 0.2 PLN; slot 1: 0.5 kWh at -50 = 0
        assert!((t[0].earnings_pln - 0.2).abs() < 1e-9);
        assert!((t[1].export - 1.25).abs() < 1e-9);
        // slot 4: 1 kWh at 400 = 0.4; slot 7: 0.25 kWh unpriced
        assert!((t[1].earnings_pln - 0.4).abs() < 1e-9);
        assert!((t[1].unpriced_export - 0.25).abs() < 1e-9);
        assert_eq!(t[1].import, 0.0, "missing series count as zero");

        let mut sum = Totals::default();
        t.iter().for_each(|b| sum.merge(b));
        assert!((sum.export - 2.25).abs() < 1e-9);
        assert!((sum.earnings_pln - 0.6).abs() < 1e-9);
    }
}
