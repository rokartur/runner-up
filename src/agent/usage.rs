use serde::{Deserialize, Serialize};

/// Usage is an observation, not a limit or a recommendation. Peaks are
/// sampled. A "process-group" reading adds up a host job's processes; its RSS
/// counts a shared page once per process. A "guest" reading is a whole tart
/// guest's busy CPUs and memory in use, from its kernel, within the guest's
/// size. Missing or failed readings are never represented as measured zero use.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub samples: i64,
    #[serde(default)]
    pub peak_cores: f64,
    #[serde(default, rename = "peakMemoryMB")]
    pub peak_memory_mb: f64,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

pub const USAGE_PREFIX: &str = "runner-up usage: ";

/// Accepts only bounded, finite observations. Runner output is not trusted:
/// these readings inform a person, never change configured sizes.
pub fn parse_usage(line: &str) -> Option<Usage> {
    if line.len() > 4096 {
        return None;
    }
    let u: Usage = serde_json::from_str(line).ok()?;
    let valid = |n: f64| n.is_finite() && n >= 0.0;
    let ok = (u.scope == "process-group" || u.scope == "guest")
        && u.samples > 0
        && valid(u.peak_cores)
        && valid(u.peak_memory_mb)
        && valid(u.seconds);
    ok.then_some(u)
}
