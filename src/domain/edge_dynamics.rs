//! K9 edge "living weight" dynamics — pure Hebbian potentiation,
//! Ebbinghaus decay, and the Cepeda spacing effect. Ported from
//! mempalace `dynamics.py` (v3.3.6). No I/O: every function operates on
//! a [`GraphEdge`]'s K9 fields, treating `None` as the documented
//! defaults.
//!
//! - [`potentiate`] **mutates** the edge on a co-access event — called
//!   by the potentiation worker (K9 phase 3).
//! - [`decayed_strength`] is **read-only** — the time-decayed strength
//!   used by retrieve scoring (K9 phase 4); decay is never persisted.
//!
//! Timestamps are mem's 20-digit zero-padded ms-since-epoch strings
//! (`current_timestamp()`). Decay is measured in **days**, the spacing
//! gate in **hours** — matching `dynamics.py`.

use crate::domain::capability_capsule::GraphEdge;

/// Lower bound on strength; connections fade but never vanish.
pub const STRENGTH_FLOOR: f32 = 0.05;
/// Upper bound; potentiation past this is a no-op.
pub const MAX_STRENGTH: f32 = 5.0;
/// Initial / unspecified strength ("normally present").
pub const DEFAULT_STRENGTH: f32 = 1.0;
/// Initial / unspecified stability (decay resistance).
pub const DEFAULT_STABILITY: f32 = 1.0;
/// Strength added per co-access event (~20 to reach `MAX_STRENGTH`).
pub const POTENTIATION_INCREMENT: f32 = 0.05;
/// Minimum gap (hours) for a reinforcement to count as "spaced".
pub const SPACED_INTERVAL_HOURS: f64 = 1.0;
/// Stability added per spaced reinforcement (Cepeda spacing effect).
pub const STABILITY_INCREMENT: f32 = 0.1;

/// Parse a 20-digit zero-padded ms-since-epoch timestamp to `u64` ms.
fn parse_ts_ms(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// Hebbian potentiation on a co-access event. Grows `strength` by
/// [`POTENTIATION_INCREMENT`] (capped at [`MAX_STRENGTH`]); grows
/// `stability` by [`STABILITY_INCREMENT`] **only** when the gap since the
/// last activation is at least [`SPACED_INTERVAL_HOURS`] (the Cepeda
/// spacing effect — rapid bursts don't build durability, distributed
/// practice does). Always stamps `last_activated = now` and bumps
/// `access_count`. `None` fields read as their defaults; the spacing gap
/// falls back to `valid_from` (creation) when the edge has never been
/// potentiated. Mutates `edge` in place.
pub fn potentiate(edge: &mut GraphEdge, now: &str) {
    let hours_since = {
        let baseline = edge.last_activated.as_deref().unwrap_or(&edge.valid_from);
        match (parse_ts_ms(now), parse_ts_ms(baseline)) {
            (Some(n), Some(l)) if n >= l => (n - l) as f64 / 3_600_000.0,
            _ => 0.0,
        }
    };

    let strength = edge.strength.unwrap_or(DEFAULT_STRENGTH);
    edge.strength = Some((strength + POTENTIATION_INCREMENT).min(MAX_STRENGTH));

    if hours_since >= SPACED_INTERVAL_HOURS {
        let stability = edge.stability.unwrap_or(DEFAULT_STABILITY);
        edge.stability = Some(stability + STABILITY_INCREMENT);
    }

    edge.last_activated = Some(now.to_string());
    edge.access_count = Some(edge.access_count.unwrap_or(0) + 1);
}

/// Ebbinghaus decay applied at **read time** (never persisted): the
/// time-decayed strength `stored * exp(-days_since / stability)`, floored
/// at [`STRENGTH_FLOOR`]. Higher stability = slower decay. Returns the
/// stored (default-applied) strength when no time has elapsed, the clock
/// can't be parsed, or the edge was **never potentiated**
/// (`last_activated` None). Decay only tracks time since an actual
/// potentiation, so pre-existing edges are not penalised for their age
/// the moment dynamics is switched on — they stay neutral until they
/// first enter the loop. `None` fields read as their defaults.
pub fn decayed_strength(edge: &GraphEdge, now: &str) -> f32 {
    let strength = edge.strength.unwrap_or(DEFAULT_STRENGTH);
    let Some(last) = edge.last_activated.as_deref() else {
        return strength;
    };
    let (Some(now_ms), Some(last_ms)) = (parse_ts_ms(now), parse_ts_ms(last)) else {
        return strength;
    };
    if now_ms <= last_ms {
        return strength;
    }
    let days_since = (now_ms - last_ms) as f64 / 86_400_000.0;
    let mut stability = edge.stability.unwrap_or(DEFAULT_STABILITY);
    if stability <= 0.0 {
        stability = DEFAULT_STABILITY;
    }
    let decayed = strength as f64 * (-days_since / stability as f64).exp();
    (decayed as f32).max(STRENGTH_FLOOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1_780_000_000_000 ms and offsets, in the 20-digit string form.
    const T0: &str = "00000001780000000000";
    const T0_30MIN: &str = "00000001780001800000"; // +1_800_000 ms
    const T0_2H: &str = "00000001780007200000"; // +7_200_000 ms
    const T0_1DAY: &str = "00000001780086400000"; // +86_400_000 ms
    const T0_100DAY: &str = "00000001788640000000"; // +8_640_000_000 ms

    fn edge(
        strength: Option<f32>,
        stability: Option<f32>,
        last_activated: Option<&str>,
    ) -> GraphEdge {
        GraphEdge {
            from_node_id: "entity:a".into(),
            to_node_id: "entity:b".into(),
            relation: "rel".into(),
            valid_from: T0.into(),
            valid_to: None,
            confidence: None,
            extractor: None,
            strength,
            stability,
            last_activated: last_activated.map(|s| s.to_string()),
            access_count: None,
        }
    }

    #[test]
    fn potentiate_grows_strength_count_and_spaced_stability() {
        let mut e = edge(Some(1.0), Some(1.0), Some(T0));
        potentiate(&mut e, T0_2H); // 2h gap >= 1h → spaced
        assert!((e.strength.unwrap() - 1.05).abs() < 1e-6);
        assert!(
            (e.stability.unwrap() - 1.1).abs() < 1e-6,
            "spaced reinforcement grows stability"
        );
        assert_eq!(e.access_count, Some(1));
        assert_eq!(e.last_activated.as_deref(), Some(T0_2H));
    }

    #[test]
    fn potentiate_massed_does_not_grow_stability() {
        let mut e = edge(Some(1.0), Some(1.0), Some(T0));
        potentiate(&mut e, T0_30MIN); // 30min < 1h → not spaced
        assert!(
            (e.strength.unwrap() - 1.05).abs() < 1e-6,
            "strength still grows"
        );
        assert!(
            (e.stability.unwrap() - 1.0).abs() < 1e-6,
            "massed reinforcement does NOT grow stability"
        );
    }

    #[test]
    fn potentiate_caps_at_max_strength() {
        let mut e = edge(Some(4.99), Some(1.0), Some(T0));
        potentiate(&mut e, T0_2H);
        assert!((e.strength.unwrap() - MAX_STRENGTH).abs() < 1e-6);
    }

    #[test]
    fn potentiate_defaults_none_fields_via_valid_from_baseline() {
        let mut e = edge(None, None, None); // last_activated None → valid_from (T0) baseline
        potentiate(&mut e, T0_1DAY);
        assert!((e.strength.unwrap() - 1.05).abs() < 1e-6);
        assert_eq!(e.access_count, Some(1));
    }

    #[test]
    fn decayed_strength_one_e_fold_per_day_at_unit_stability() {
        let e = edge(Some(2.0), Some(1.0), Some(T0));
        let d = decayed_strength(&e, T0_1DAY); // 1 day / stability 1 → *e^-1
        assert!((d - 2.0 * (-1.0f32).exp()).abs() < 1e-4, "got {d}");
    }

    #[test]
    fn decayed_strength_floors_after_long_neglect() {
        let e = edge(Some(1.0), Some(1.0), Some(T0));
        let d = decayed_strength(&e, T0_100DAY);
        assert!(
            (d - STRENGTH_FLOOR).abs() < 1e-6,
            "100 days → floored, got {d}"
        );
    }

    #[test]
    fn decayed_strength_no_elapsed_time_returns_stored() {
        let e = edge(Some(2.0), Some(1.0), Some(T0));
        assert!((decayed_strength(&e, T0) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn decayed_strength_never_potentiated_does_not_decay() {
        // last_activated None (a pre-dynamics edge) → neutral, no decay
        // no matter how old, so enabling dynamics doesn't tank the boost
        // of the existing graph.
        let e = edge(Some(1.0), Some(1.0), None);
        assert!((decayed_strength(&e, T0_100DAY) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn decayed_strength_unset_reads_as_default() {
        let e = edge(None, None, Some(T0));
        assert!((decayed_strength(&e, T0) - DEFAULT_STRENGTH).abs() < 1e-6);
    }
}
