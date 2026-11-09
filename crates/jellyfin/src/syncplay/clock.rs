//! Measuring the server's clock, which group commands are timed by.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::client::{Client, HttpError};
use crate::time::parse_utc_ms;

/// NTP-style `(offset, rtt)` in ms from local send `t0`, server receive `t1`,
/// server send `t2` and local receive `t3`.
#[must_use]
pub fn clock_sample(t0_ms: i64, t1_ms: i64, t2_ms: i64, t3_ms: i64) -> (i64, i64) {
    (
        i64::midpoint(t1_ms - t0_ms, t2_ms - t3_ms),
        (t3_ms - t0_ms) - (t2_ms - t1_ms),
    )
}

/// Milliseconds since the Unix epoch by the local clock, the one clock samples measure.
#[must_use]
pub fn local_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl Client {
    /// One `/GetUtcTime` round trip as `(offset, rtt)` in ms; see [`clock_sample`].
    ///
    /// # Errors
    /// The request failed, or the reply has no times.
    pub fn clock_probe(&self) -> Result<(i64, i64), HttpError> {
        let t0 = local_now_ms();
        let reply = self.get("/GetUtcTime")?;
        let t3 = local_now_ms();
        let stamp = |key| {
            reply
                .get(key)
                .and_then(Value::as_str)
                .and_then(parse_utc_ms)
        };
        match (
            stamp("RequestReceptionTime"),
            stamp("ResponseTransmissionTime"),
        ) {
            (Some(t1), Some(t2)) => Ok(clock_sample(t0, t1, t2, t3)),
            _ => Err(HttpError::transport("bad /GetUtcTime reply")),
        }
    }

    /// Tells the group this client's one-way delay; the server doubles it for its start delay.
    ///
    /// # Errors
    /// The request failed.
    pub fn ping(&self, one_way_ms: i64) -> Result<(), HttpError> {
        self.post("/SyncPlay/Ping", &json!({ "Ping": one_way_ms }))
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_sample_matches_ntp_formula() {
        // Server clock 1000 ahead; 40 ms each way, 10 ms server hold.
        assert_eq!(clock_sample(0, 1040, 1050, 90), (1000, 80));
        assert_eq!(clock_sample(100, 100, 100, 100), (0, 0));
    }
}
