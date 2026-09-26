//! A run's temperature and load curve, resampled for upload (API field
//! `series`; the result page draws it as a chart). Starts a few seconds
//! before the stress so the rise from idle is visible.

use crate::sensors::Sample;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// At most this many points per channel: longer tests get a longer interval.
const MAX_POINTS: u64 = 300;
/// Idle readings before the stress starts that are included.
pub const LEAD_IN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    /// Format version.
    pub v: u8,
    /// Seconds between points.
    pub interval: u64,
    /// Time of the first point relative to the stress start (negative = idle lead-in).
    pub start: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_temp: Option<Vec<Option<f64>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_temp: Option<Vec<Option<f64>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_usage: Option<Vec<Option<f64>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_usage: Option<Vec<Option<f64>>>,
}

/// Temperature and usage samples of one part.
pub struct PartSamples<'a> {
    pub temp: &'a VecDeque<Sample>,
    pub usage: &'a VecDeque<Sample>,
}

impl Series {
    /// Resample the readings between `LEAD_IN` before `started` and
    /// `stopped`. None if the run was too short to draw.
    pub fn build(cpu: Option<PartSamples>, gpu: Option<PartSamples>, started: Instant, stopped: Instant) -> Option<Series> {
        let from = started.checked_sub(LEAD_IN).unwrap_or(started);
        let total = stopped.saturating_duration_since(from).as_secs();
        if total < 2 || (cpu.is_none() && gpu.is_none()) {
            return None;
        }
        let interval = total.div_ceil(MAX_POINTS).max(1);
        let bins = total.div_ceil(interval) as usize;
        let step = Duration::from_secs(interval);
        let temps = |p: &Option<PartSamples>| p.as_ref().map(|p| resample(p.temp, from, bins, step, Fold::Max));
        let usage = |p: &Option<PartSamples>| p.as_ref().map(|p| resample(p.usage, from, bins, step, Fold::Mean));
        let mut series = Series {
            v: 1,
            interval,
            start: -(started - from).as_secs_f64().round(),
            cpu_temp: temps(&cpu),
            gpu_temp: temps(&gpu),
            cpu_usage: usage(&cpu),
            gpu_usage: usage(&gpu),
        };
        // Skip the lead-in the sensors hadn't reached yet (plain mode starts
        // sampling just before the test). Nothing to draw without a reading.
        let first = (0..bins).find(|&i| {
            [&series.cpu_temp, &series.gpu_temp]
                .iter()
                .any(|c| c.as_ref().is_some_and(|v| v[i].is_some()))
        })?;
        if first > 0 {
            for v in [&mut series.cpu_temp, &mut series.gpu_temp, &mut series.cpu_usage, &mut series.gpu_usage]
                .into_iter()
                .flatten()
            {
                v.drain(..first);
            }
            series.start += (first as u64 * interval) as f64;
        }
        (bins - first >= 2).then_some(series)
    }

    /// The same curve with only the channels of the parts being submitted.
    pub fn only(&self, cpu: bool, gpu: bool) -> Series {
        Series {
            cpu_temp: self.cpu_temp.clone().filter(|_| cpu),
            cpu_usage: self.cpu_usage.clone().filter(|_| cpu),
            gpu_temp: self.gpu_temp.clone().filter(|_| gpu),
            gpu_usage: self.gpu_usage.clone().filter(|_| gpu),
            ..self.clone()
        }
    }
}

#[derive(Clone, Copy)]
enum Fold {
    /// Temperatures keep each interval's highest reading, so the curve's peak
    /// matches the peak reported with the result.
    Max,
    Mean,
}

fn resample(samples: &VecDeque<Sample>, from: Instant, bins: usize, step: Duration, fold: Fold) -> Vec<Option<f64>> {
    let mut acc: Vec<Option<(f64, u32)>> = vec![None; bins];
    for s in samples {
        let Some(offset) = s.at.checked_duration_since(from) else { continue };
        let i = (offset.as_secs_f64() / step.as_secs_f64()) as usize;
        let Some(slot) = acc.get_mut(i) else { continue };
        *slot = Some(match (*slot, fold) {
            (None, _) => (s.value, 1),
            (Some((v, n)), Fold::Max) => (v.max(s.value), n + 1),
            (Some((v, n)), Fold::Mean) => (v + s.value, n + 1),
        });
    }
    acc.into_iter()
        .map(|slot| {
            slot.map(|(v, n)| {
                let v = match fold {
                    Fold::Max => v,
                    Fold::Mean => v / n as f64,
                };
                (v * 10.0).round() / 10.0
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(from: Instant, secs: u64, value: impl Fn(u64) -> f64) -> VecDeque<Sample> {
        (0..secs).map(|s| Sample { at: from + Duration::from_secs(s), value: value(s) }).collect()
    }

    #[test]
    fn two_minute_run_keeps_one_second_points() {
        let t0 = Instant::now();
        let started = t0 + Duration::from_secs(10);
        let stopped = started + Duration::from_secs(120);
        let temp = samples(t0, 130, |s| if s < 10 { 40.0 } else { 40.0 + (s - 10) as f64 * 0.3 });
        let usage = samples(t0, 130, |s| if s < 10 { 5.0 } else { 100.0 });
        let series = Series::build(Some(PartSamples { temp: &temp, usage: &usage }), None, started, stopped).unwrap();
        assert_eq!(series.interval, 1);
        assert_eq!(series.start, -10.0);
        let cpu = series.cpu_temp.as_ref().unwrap();
        assert_eq!(cpu.len(), 130);
        assert_eq!(cpu[0], Some(40.0));
        assert_eq!(cpu[129], Some(75.7));
        assert!(series.gpu_temp.is_none());
    }

    #[test]
    fn long_runs_are_capped_and_keep_the_peak() {
        let t0 = Instant::now();
        let started = t0 + Duration::from_secs(10);
        let stopped = started + Duration::from_secs(1800);
        // One spike at 1000 s must survive resampling.
        let temp = samples(t0, 1810, |s| if s == 1000 { 99.0 } else { 70.0 });
        let usage = samples(t0, 1810, |_| 100.0);
        let series = Series::build(None, Some(PartSamples { temp: &temp, usage: &usage }), started, stopped).unwrap();
        let gpu = series.gpu_temp.as_ref().unwrap();
        assert!(gpu.len() as u64 <= MAX_POINTS);
        assert_eq!(series.interval, 7);
        assert!(gpu.contains(&Some(99.0)));
        assert_eq!(series.gpu_usage.as_ref().unwrap()[3], Some(100.0));
    }

    #[test]
    fn gaps_stay_empty_and_empty_runs_give_nothing() {
        let t0 = Instant::now();
        let started = t0 + Duration::from_secs(10);
        let stopped = started + Duration::from_secs(60);
        let temp: VecDeque<Sample> = samples(t0, 70, |_| 50.0).into_iter().filter(|s| s.at != t0 + Duration::from_secs(30)).collect();
        let usage = VecDeque::new();
        let series = Series::build(Some(PartSamples { temp: &temp, usage: &usage }), None, started, stopped).unwrap();
        assert_eq!(series.cpu_temp.as_ref().unwrap()[30], None);
        assert!(series.cpu_usage.as_ref().unwrap().iter().all(Option::is_none));

        let none = VecDeque::new();
        assert!(Series::build(Some(PartSamples { temp: &none, usage: &none }), None, started, stopped).is_none());
    }

    #[test]
    fn empty_lead_in_is_trimmed() {
        let t0 = Instant::now();
        let started = t0 + Duration::from_secs(10);
        let stopped = started + Duration::from_secs(60);
        // First reading 3 s before the stress.
        let temp = samples(t0 + Duration::from_secs(7), 63, |_| 45.0);
        let usage = samples(t0, 70, |_| 50.0);
        let series = Series::build(Some(PartSamples { temp: &temp, usage: &usage }), None, started, stopped).unwrap();
        assert_eq!(series.start, -3.0);
        assert_eq!(series.cpu_temp.as_ref().unwrap()[0], Some(45.0));
        assert_eq!(series.cpu_temp.as_ref().unwrap().len(), series.cpu_usage.as_ref().unwrap().len());
    }

    #[test]
    fn serializes_like_the_api_expects() {
        let s = Series { v: 1, interval: 2, start: -10.0, cpu_temp: Some(vec![Some(40.0), None]), gpu_temp: None, cpu_usage: None, gpu_usage: None };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"v":1,"interval":2,"start":-10.0,"cpuTemp":[40.0,null]}"#);
        let only = Series { gpu_temp: Some(vec![Some(1.0), Some(2.0)]), ..s.clone() }.only(false, true);
        assert!(only.cpu_temp.is_none() && only.gpu_temp.is_some());
    }
}
