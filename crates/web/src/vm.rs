//! Energy counters from VictoriaMetrics (Prometheus query API).

use std::time::Duration as StdDuration;

use anyhow::{Context, bail};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use crate::energy::{Counters, Metric};
use crate::time::SLOT;

/// VictoriaMetrics returns at most 30000 points per series and query.
const MAX_CHUNK: Duration = Duration::days(30);

/// Latest counter value at each slot boundary, for every metric.
pub fn counters_query() -> String {
    let names: Vec<&str> = Metric::ALL.iter().map(|m| m.series()).collect();
    format!(
        "last_over_time({{__name__=~\"{}\"}}[{}m]) keep_metric_names",
        names.join("|"),
        SLOT.num_minutes()
    )
}

/// `[start, end]` split into chunks of at most [`MAX_CHUNK`]; each chunk
/// starts where the previous one ended.
pub fn chunks(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    let mut from = start;
    while from < end {
        let to = (from + MAX_CHUNK).min(end);
        out.push((from, to));
        from = to;
    }
    out
}

/// Series name and `(unix seconds, value)` samples.
pub type Samples = Vec<(String, Vec<(i64, f64)>)>;

/// Parses a `query_range` response into series name and `(unix seconds,
/// value)` samples.
pub fn parse_range(json: &[u8]) -> anyhow::Result<Samples> {
    #[derive(Deserialize)]
    struct Response {
        status: String,
        error: Option<String>,
        data: Option<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        result: Vec<Series>,
    }
    #[derive(Deserialize)]
    struct Series {
        metric: std::collections::HashMap<String, String>,
        values: Vec<(f64, String)>,
    }

    let response: Response =
        serde_json::from_slice(json).context("parsing the VictoriaMetrics response")?;
    if response.status != "success" {
        bail!(
            "VictoriaMetrics: {}",
            response.error.as_deref().unwrap_or(&response.status)
        );
    }
    let Some(data) = response.data else {
        bail!("VictoriaMetrics: response without data");
    };
    data.result
        .into_iter()
        .map(|s| {
            let name = s.metric.get("__name__").cloned().unwrap_or_default();
            let values = s
                .values
                .into_iter()
                .map(|(t, v)| {
                    Ok((
                        t as i64,
                        v.parse::<f64>()
                            .with_context(|| format!("bad value {v:?}"))?,
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok((name, values))
        })
        .collect()
}

/// Puts samples on the slot grid from `start` (n slots, n + 1 readings).
pub fn to_counters(
    start: DateTime<Utc>,
    slots: usize,
    series: &[(String, Vec<(i64, f64)>)],
) -> Counters {
    let mut counters = Counters {
        start,
        series: Default::default(),
    };
    let step = SLOT.num_seconds();
    for (name, samples) in series {
        let Some(metric) = Metric::from_series(name) else {
            continue;
        };
        let values = counters
            .series
            .entry(metric)
            .or_insert_with(|| vec![None; slots + 1]);
        for &(t, v) in samples {
            let offset = t - start.timestamp();
            if offset >= 0
                && offset % step == 0
                && let Some(slot) = values.get_mut((offset / step) as usize)
            {
                *slot = Some(v);
            }
        }
    }
    counters
}

pub struct Vm {
    base_url: String,
    timeout: StdDuration,
}

impl Vm {
    pub fn new(base_url: &str, timeout: StdDuration) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            timeout,
        }
    }

    /// Counter readings on the slot grid for `[start, end]`.
    pub async fn counters(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> anyhow::Result<Counters> {
        let slots = ((end - start).num_seconds() / SLOT.num_seconds()).max(0) as usize;
        let mut all = Vec::new();
        for (from, to) in chunks(start, end) {
            let url = format!("{}/api/v1/query_range", self.base_url);
            let query = counters_query();
            let timeout = self.timeout;
            let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(timeout))
                    .build()
                    .into();
                let mut response = agent
                    .get(&url)
                    .query("query", &query)
                    .query("start", from.timestamp().to_string())
                    .query("end", to.timestamp().to_string())
                    .query("step", format!("{}s", SLOT.num_seconds()))
                    .call()
                    .with_context(|| format!("querying {url}"))?;
                Ok(response.body_mut().read_to_vec()?)
            })
            .await??;
            all.extend(parse_range(&body)?);
        }
        Ok(to_counters(start, slots, &all))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn query_covers_every_metric() {
        let q = counters_query();
        assert!(q.starts_with("last_over_time("), "{q}");
        assert!(q.contains("[15m]"), "{q}");
        assert!(q.ends_with("keep_metric_names"), "{q}");
        for m in Metric::ALL {
            assert!(q.contains(m.series()), "{q}");
        }
    }

    #[test]
    fn chunking() {
        let s = utc("2026-01-01T00:00:00Z");
        assert_eq!(
            chunks(s, s + Duration::days(1)),
            [(s, s + Duration::days(1))]
        );
        let c = chunks(s, s + Duration::days(65));
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], (s, s + Duration::days(30)));
        assert_eq!(c[1].0, c[0].1);
        assert_eq!(c[2].1, s + Duration::days(65));
        assert!(chunks(s, s).is_empty());
    }

    const RESPONSE: &[u8] = br#"{"status":"success","data":{"resultType":"matrix","result":[
        {"metric":{"__name__":"deye_pv_energy"},"values":[[1793833200,"100.5"],[1793834100,"101"]]},
        {"metric":{"__name__":"dtsu_grid_export_energy"},"values":[[1793834100,"7.25"]]},
        {"metric":{"__name__":"something_else"},"values":[[1793833200,"1"]]}
    ]}}"#;

    #[test]
    fn parses_matrix() {
        let series = parse_range(RESPONSE).unwrap();
        assert_eq!(series.len(), 3);
        assert_eq!(series[0].0, "deye_pv_energy");
        assert_eq!(
            series[0].1,
            [(1_793_833_200, 100.5), (1_793_834_100, 101.0)]
        );
    }

    #[test]
    fn rejects_errors() {
        assert!(
            parse_range(br#"{"status":"error","error":"bad query"}"#)
                .unwrap_err()
                .to_string()
                .contains("bad query")
        );
        assert!(parse_range(b"oops").is_err());
    }

    #[test]
    fn places_samples_on_the_grid() {
        let start = DateTime::from_timestamp(1_793_833_200, 0).unwrap();
        let counters = to_counters(start, 2, &parse_range(RESPONSE).unwrap());
        assert_eq!(counters.start, start);
        assert_eq!(
            counters.series[&Metric::Pv],
            [Some(100.5), Some(101.0), None]
        );
        assert_eq!(counters.series[&Metric::Export], [None, Some(7.25), None]);
        assert_eq!(counters.series.len(), 2, "unknown series are dropped");
    }
}
