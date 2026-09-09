//! Cross-device coordination helpers built on Drive `appProperties`.
//!
//! Version vectors track causal history as `device_id -> counter` pairs.

use std::collections::BTreeMap;

const APP_PROP_VERSION_VECTOR: &str = "ox_vv";
const APP_PROP_ORIGIN: &str = "ox_origin";
const MAX_APP_PROPERTY_VALUE_BYTES: usize = 124;

/// Three-way ordering for partial orders such as version-vector dominance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering3 {
    /// Both vectors are identical.
    Equal,
    /// Left side is greater-or-equal on every component and strictly greater on at least one.
    Dominates,
    /// Left side is lower-or-equal on every component and strictly lower on at least one.
    DominatedBy,
    /// Neither side dominates the other (concurrent edits).
    Concurrent,
}

/// Compact wrapper around a causal version vector (`device_id -> counter`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VersionVector {
    entries: BTreeMap<String, u64>,
}

impl VersionVector {
    /// Parses `device:count;device2:count2` (invalid segments are ignored).
    #[must_use]
    pub fn parse(input: &str) -> Self {
        let mut entries = BTreeMap::new();
        for segment in input.split(';') {
            let trimmed = segment.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some((device_raw, count_raw)) = trimmed.split_once(':') else {
                continue;
            };
            let device = device_raw.trim();
            if device.is_empty() {
                continue;
            }
            let Ok(count) = count_raw.trim().parse::<u64>() else {
                continue;
            };
            entries
                .entry(device.to_string())
                .and_modify(|existing: &mut u64| *existing = (*existing).max(count))
                .or_insert(count);
        }
        Self { entries }
    }

    /// Builds a vector from a persisted sync-record map.
    #[must_use]
    pub fn from_map(entries: &BTreeMap<String, u64>) -> Self {
        Self {
            entries: entries.clone(),
        }
    }

    /// Returns true when the vector has no usable component.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Consumes the wrapper and returns the underlying map.
    #[must_use]
    pub fn into_map(self) -> BTreeMap<String, u64> {
        self.entries
    }

    /// Parses the shared vector from Drive app properties (`ox_vv`).
    #[must_use]
    pub fn from_app_properties(props: &BTreeMap<String, String>) -> Self {
        props
            .get(APP_PROP_VERSION_VECTOR)
            .map_or_else(Self::default, |raw| Self::parse(raw))
    }

    /// Writes this vector back to Drive app properties.
    ///
    /// Always writes:
    /// - `ox_vv`: serialized version vector
    /// - `ox_origin`: device id that produced this write
    pub fn write_into_app_properties(
        &self,
        props: &mut BTreeMap<String, String>,
        origin_device: &str,
    ) {
        let bounded = self.bounded_app_property_value(origin_device);
        if bounded.is_empty() {
            // Never push an empty `ox_vv`: PATCHing it would erase the remote
            // causal vector for every client. Omit the key so Drive keeps its
            // existing value (merge-by-key semantics).
            props.remove(APP_PROP_VERSION_VECTOR);
        } else {
            props.insert(APP_PROP_VERSION_VECTOR.to_string(), bounded);
        }
        props.insert(APP_PROP_ORIGIN.to_string(), origin_device.to_string());
    }

    /// Increments the counter for `device`.
    pub fn increment(&mut self, device: &str) {
        if device.trim().is_empty() {
            return;
        }
        let counter = self.entries.entry(device.to_string()).or_insert(0);
        *counter = counter.saturating_add(1);
    }

    /// Returns the pointwise-maximum merge of two vectors.
    #[must_use]
    pub fn merge(&self, other: &VersionVector) -> VersionVector {
        let mut merged = self.entries.clone();
        for (device, counter) in &other.entries {
            merged
                .entry(device.clone())
                .and_modify(|existing: &mut u64| *existing = (*existing).max(*counter))
                .or_insert(*counter);
        }
        VersionVector { entries: merged }
    }

    /// Computes causal ordering between `self` and `other`.
    ///
    /// - `Dominates`: `self >= other` component-wise and strictly greater somewhere.
    /// - `DominatedBy`: symmetric case.
    /// - `Equal`: vectors are identical.
    /// - `Concurrent`: incomparable.
    #[must_use]
    pub fn dominance(&self, other: &VersionVector) -> Ordering3 {
        let mut self_ge_other = true;
        let mut self_le_other = true;
        let mut strictly_greater = false;
        let mut strictly_lower = false;

        for key in self.entries.keys().chain(other.entries.keys()) {
            let left = self.entries.get(key).copied().unwrap_or(0);
            let right = other.entries.get(key).copied().unwrap_or(0);
            if left < right {
                self_ge_other = false;
                strictly_lower = true;
            }
            if left > right {
                self_le_other = false;
                strictly_greater = true;
            }
        }

        if !strictly_greater && !strictly_lower {
            Ordering3::Equal
        } else if self_ge_other {
            Ordering3::Dominates
        } else if self_le_other {
            Ordering3::DominatedBy
        } else {
            Ordering3::Concurrent
        }
    }

    /// Serializes a bounded value for Drive appProperties (`<= 124 bytes`).
    ///
    /// When trimming is required we drop the least active entries first
    /// (`count` ascending, then `device_id` lexicographic), while preserving
    /// the local origin device when present. Trimming makes dominance checks
    /// more conservative (`Concurrent` more often), which is a safe conflict
    /// behavior (extra conflict copies) compared to silent data loss.
    fn bounded_app_property_value(&self, origin_device: &str) -> String {
        let mut kept = self.entries.clone();
        let origin_device = origin_device.trim();
        let keep_origin = !origin_device.is_empty() && kept.contains_key(origin_device);
        let mut serialized = VersionVector {
            entries: kept.clone(),
        }
        .to_string();
        if serialized.len() <= MAX_APP_PROPERTY_VALUE_BYTES {
            return serialized;
        }

        let mut removable: Vec<(String, u64)> = kept
            .iter()
            .filter(|(device, _)| !(keep_origin && device.as_str() == origin_device))
            .map(|(device, count)| (device.clone(), *count))
            .collect();
        removable.sort_by(|(device_a, count_a), (device_b, count_b)| {
            count_a.cmp(count_b).then_with(|| device_a.cmp(device_b))
        });

        for (device, _) in removable {
            kept.remove(&device);
            serialized = VersionVector {
                entries: kept.clone(),
            }
            .to_string();
            if serialized.len() <= MAX_APP_PROPERTY_VALUE_BYTES {
                return serialized;
            }
        }

        // Last resort: even the preserved origin entry exceeds the limit (e.g. a
        // pathologically long device id). Drop the vector entirely rather than
        // panicking or emitting a value Drive would reject. An absent vector is
        // handled conservatively downstream (more conflict copies), never as
        // silent data loss.
        tracing::warn!(
            origin_device = origin_device,
            max_bytes = MAX_APP_PROPERTY_VALUE_BYTES,
            "version vector cannot fit in Drive metadata; writing an empty vector"
        );
        String::new()
    }
}

impl std::fmt::Display for VersionVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (device, counter) in &self.entries {
            if !first {
                f.write_str(";")?;
            }
            first = false;
            write!(f, "{device}:{counter}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Ordering3, VersionVector, MAX_APP_PROPERTY_VALUE_BYTES};
    use std::collections::BTreeMap;

    fn write_vv(vv: &VersionVector, origin_device: &str) -> String {
        let mut props = BTreeMap::new();
        vv.write_into_app_properties(&mut props, origin_device);
        props.get("ox_vv").cloned().unwrap_or_default()
    }

    #[test]
    fn parse_round_trip_is_stable() {
        let vv = VersionVector::parse("alice:1;bob:5");
        assert_eq!(vv.to_string(), "alice:1;bob:5");
        let back = VersionVector::parse(&vv.to_string());
        assert_eq!(back, vv);
    }

    #[test]
    fn parse_ignores_invalid_segments() {
        let vv = VersionVector::parse("alice:2;bad;:3;bob:x;carol:7;alice:4");
        assert_eq!(vv.to_string(), "alice:4;carol:7");
    }

    #[test]
    fn from_and_write_app_properties() {
        let mut props = BTreeMap::new();
        props.insert("ox_vv".to_string(), "dev-a:2".to_string());
        let vv = VersionVector::from_app_properties(&props);
        assert_eq!(vv.to_string(), "dev-a:2");

        let mut out = BTreeMap::new();
        vv.write_into_app_properties(&mut out, "dev-a");
        assert_eq!(out.get("ox_vv").map(String::as_str), Some("dev-a:2"));
        assert_eq!(out.get("ox_origin").map(String::as_str), Some("dev-a"));
    }

    #[test]
    fn write_app_properties_bounds_large_vectors() {
        let local_device = "hostname-local-3f9a2c";
        let mut entries = BTreeMap::new();
        for i in 0..20 {
            entries.insert(format!("hostname-{i:02}-3f9a2c"), (i + 1) as u64);
        }
        entries.insert(local_device.to_string(), 1);
        let vv = VersionVector::from_map(&entries);

        let serialized = write_vv(&vv, local_device);
        assert!(serialized.len() <= MAX_APP_PROPERTY_VALUE_BYTES);
    }

    #[test]
    fn write_app_properties_keeps_local_entry_when_pruned() {
        let local_device = "hostname-local-3f9a2c";
        let mut entries = BTreeMap::new();
        entries.insert(local_device.to_string(), 1);
        for i in 0..20 {
            entries.insert(format!("hostname-{i:02}-3f9a2c"), (i + 100) as u64);
        }
        let vv = VersionVector::from_map(&entries);

        let serialized = write_vv(&vv, local_device);
        assert!(serialized.split(';').any(|segment| {
            segment
                .split_once(':')
                .is_some_and(|(device, _)| device == local_device)
        }));
        assert!(serialized.len() <= MAX_APP_PROPERTY_VALUE_BYTES);
    }

    #[test]
    fn write_app_properties_value_round_trips_after_bounding() {
        let local_device = "hostname-local-3f9a2c";
        let mut entries = BTreeMap::new();
        entries.insert(local_device.to_string(), 5);
        for i in 0..20 {
            entries.insert(format!("hostname-{i:02}-3f9a2c"), (i + 1) as u64);
        }
        let vv = VersionVector::from_map(&entries);

        let serialized = write_vv(&vv, local_device);
        let parsed_back = VersionVector::parse(&serialized);
        assert_eq!(parsed_back.to_string(), serialized);
    }

    #[test]
    fn write_app_properties_keeps_nominal_serialization_unchanged() {
        let vv = VersionVector::parse("dev-a:2;dev-b:5;dev-c:9");
        let serialized = write_vv(&vv, "dev-b");
        assert_eq!(serialized, "dev-a:2;dev-b:5;dev-c:9");
    }

    #[test]
    fn write_app_properties_omits_key_instead_of_writing_empty() {
        // A single device id longer than the byte budget cannot fit. The fallback
        // must NOT push an empty `ox_vv` (which would erase the remote causal
        // vector); instead it drops the key from the PATCH map so Drive's
        // merge-by-key semantics preserve the existing remote value.
        let huge_device = "d".repeat(MAX_APP_PROPERTY_VALUE_BYTES + 10);
        let mut entries = BTreeMap::new();
        entries.insert(huge_device.clone(), 1);
        let vv = VersionVector::from_map(&entries);

        let mut props = BTreeMap::new();
        props.insert("ox_vv".to_string(), "preexisting:7".to_string());
        vv.write_into_app_properties(&mut props, &huge_device);

        assert!(
            !props.contains_key("ox_vv"),
            "ox_vv must be omitted from the PATCH map, never written as empty"
        );
        assert_eq!(
            props.get("ox_origin").map(String::as_str),
            Some(&huge_device[..])
        );
    }

    #[test]
    fn increment_increases_counter() {
        let mut vv = VersionVector::default();
        vv.increment("dev-a");
        vv.increment("dev-a");
        vv.increment("dev-b");
        assert_eq!(vv.to_string(), "dev-a:2;dev-b:1");
    }

    #[test]
    fn merge_uses_componentwise_maximum() {
        let left = VersionVector::parse("a:1;b:3");
        let right = VersionVector::parse("a:4;c:2");
        let merged = left.merge(&right);
        assert_eq!(merged.to_string(), "a:4;b:3;c:2");
    }

    #[test]
    fn dominance_equal() {
        let left = VersionVector::parse("a:2;b:1");
        let right = VersionVector::parse("a:2;b:1");
        assert_eq!(left.dominance(&right), Ordering3::Equal);
    }

    #[test]
    fn dominance_dominates() {
        let left = VersionVector::parse("a:3;b:2");
        let right = VersionVector::parse("a:2;b:2");
        assert_eq!(left.dominance(&right), Ordering3::Dominates);
    }

    #[test]
    fn dominance_dominated_by() {
        let left = VersionVector::parse("a:1");
        let right = VersionVector::parse("a:1;b:1");
        assert_eq!(left.dominance(&right), Ordering3::DominatedBy);
    }

    #[test]
    fn dominance_concurrent() {
        let left = VersionVector::parse("a:2;b:1");
        let right = VersionVector::parse("a:1;b:2");
        assert_eq!(left.dominance(&right), Ordering3::Concurrent);
    }
}
