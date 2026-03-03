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
    if snapshots.len() < 2 {
        return TokenStats {
            token_name: token_name.to_string(),
            burn_rate_5h: None,
            burn_rate_7d: None,
            mean_burn_7d: None,
            stddev_burn_7d: None,
            peak_burn_7d: None,
            hours_to_depletion_5h: None,
            hours_to_depletion_7d: None,
            snapshot_count: snapshots.len(),
        };
    }

    let mut burn_rates_7d: Vec<f64> = Vec::new();
    let mut burn_rates_5h: Vec<f64> = Vec::new();

    for pair in snapshots.windows(2) {
        let dt_hours = (pair[1].probed_at - pair[0].probed_at).num_seconds() as f64 / 3600.0;
        if dt_hours <= 0.0 {
            continue;
        }

        if let (Some(u0), Some(u1)) = (pair[0].utilization_7d, pair[1].utilization_7d) {
            let rate = (u1 - u0) / dt_hours;
            if rate.is_finite() {
                burn_rates_7d.push(rate);
            }
        }
        if let (Some(u0), Some(u1)) = (pair[0].utilization_5h, pair[1].utilization_5h) {
            let rate = (u1 - u0) / dt_hours;
            if rate.is_finite() {
                burn_rates_5h.push(rate);
            }
        }
    }

    let latest_burn_5h = burn_rates_5h.last().copied();
    let latest_burn_7d = burn_rates_7d.last().copied();

    let mean_burn_7d = if burn_rates_7d.is_empty() {
        None
    } else {
        Some(burn_rates_7d.iter().sum::<f64>() / burn_rates_7d.len() as f64)
    };

    let stddev_burn_7d = mean_burn_7d.map(|mean| {
        let variance =
            burn_rates_7d.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / burn_rates_7d.len() as f64;
        variance.sqrt()
    });

    let peak_burn_7d = burn_rates_7d.iter().copied().reduce(f64::max);

    let last = snapshots.last().unwrap();

    let hours_to_depletion_5h = latest_burn_5h
        .filter(|&r| r > 0.0)
        .and_then(|rate| {
            last.utilization_5h.map(|u| (1.0 - u) / rate)
        });

    let hours_to_depletion_7d = latest_burn_7d
        .filter(|&r| r > 0.0)
        .and_then(|rate| {
            last.utilization_7d.map(|u| (1.0 - u) / rate)
        });

    TokenStats {
        token_name: token_name.to_string(),
        burn_rate_5h: latest_burn_5h,
        burn_rate_7d: latest_burn_7d,
        mean_burn_7d,
        stddev_burn_7d,
        peak_burn_7d,
        hours_to_depletion_5h,
        hours_to_depletion_7d,
        snapshot_count: snapshots.len(),
    }
}
