use crate::store::Snapshot;

#[derive(Debug, Clone)]
pub struct TokenStats {
    pub token_name: String,
    /// Burn rate for 5h utilization: Δ(utilization) / Δ(hours)
    pub burn_rate_5h: Option<f64>,
    /// Burn rate for 7d utilization: Δ(utilization) / Δ(hours)
    pub burn_rate_7d: Option<f64>,
    /// Mean burn rate (7d) over all snapshot pairs
    pub mean_burn_7d: Option<f64>,
    /// Standard deviation of burn rate (7d)
    pub stddev_burn_7d: Option<f64>,
    /// Peak burn rate (7d) observed
    pub peak_burn_7d: Option<f64>,
    /// Estimated hours until 5h window hits 100%, at current burn rate
    pub hours_to_depletion_5h: Option<f64>,
    /// Estimated hours until 7d window hits 100%, at current burn rate
    pub hours_to_depletion_7d: Option<f64>,
    /// Number of snapshots used for computation
    pub snapshot_count: usize,
}

pub fn compute_stats(token_name: &str, snapshots: &[Snapshot]) -> TokenStats {
    // Burn rates between consecutive snapshots within the same reset window.
    let burn_rates = |series: fn(&Snapshot) -> (Option<f64>, Option<i64>)| -> Vec<f64> {
        snapshots
            .windows(2)
            .filter_map(|pair| {
                let dt_hours =
                    (pair[1].probed_at - pair[0].probed_at).num_seconds() as f64 / 3600.0;
                let ((u0, r0), (u1, r1)) = (series(&pair[0]), series(&pair[1]));
                let (u0, u1) = (u0?, u1?);
                let rate = (u1 - u0) / dt_hours;
                (dt_hours > 0.0 && r0 == r1 && u1 >= u0 && rate.is_finite()).then_some(rate)
            })
            .collect()
    };
    let series_5h = |s: &Snapshot| (s.utilization_5h, s.reset_5h);
    let series_7d = |s: &Snapshot| (s.utilization_7d, s.reset_7d);
    let burn_rates_5h = burn_rates(series_5h);
    let burn_rates_7d = burn_rates(series_7d);

    let latest_burn_5h = burn_rates_5h.last().copied();
    let latest_burn_7d = burn_rates_7d.last().copied();

    let mean_burn_7d = if burn_rates_7d.is_empty() {
        None
    } else {
        Some(burn_rates_7d.iter().sum::<f64>() / burn_rates_7d.len() as f64)
    };

    let stddev_burn_7d = mean_burn_7d.map(|mean| {
        let variance = burn_rates_7d
            .iter()
            .map(|r| (r - mean).powi(2))
            .sum::<f64>()
            / burn_rates_7d.len() as f64;
        variance.sqrt()
    });

    let peak_burn_7d = burn_rates_7d.iter().copied().reduce(f64::max);

    let depletion = |latest: Option<f64>, series: fn(&Snapshot) -> (Option<f64>, Option<i64>)| {
        let rate = latest.filter(|&r| r > 0.0)?;
        let u = series(snapshots.last()?).0?;
        Some((1.0 - u) / rate)
    };

    TokenStats {
        token_name: token_name.to_string(),
        burn_rate_5h: latest_burn_5h,
        burn_rate_7d: latest_burn_7d,
        mean_burn_7d,
        stddev_burn_7d,
        peak_burn_7d,
        hours_to_depletion_5h: depletion(latest_burn_5h, series_5h),
        hours_to_depletion_7d: depletion(latest_burn_7d, series_7d),
        snapshot_count: snapshots.len(),
    }
}
