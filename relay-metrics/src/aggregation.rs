use std::collections::{btree_map, hash_map::Entry, BTreeMap, BTreeSet, HashMap};

use std::fmt;
use std::iter::FromIterator;
use std::mem;
use std::time::{Duration, Instant};

use actix::prelude::*;

use failure::Fail;
use float_ord::FloatOrd;
use hash32::{FnvHasher, Hasher};
use serde::{Deserialize, Serialize};

use relay_common::{MonotonicResult, ProjectKey, UnixTimestamp};
use relay_system::{Controller, Shutdown};

use crate::statsd::{MetricCounters, MetricGauges, MetricHistograms, MetricSets, MetricTimers};
use crate::{
    protocol, CounterType, DistributionType, GaugeType, Metric, MetricType, MetricUnit,
    MetricValue, SetType,
};

/// Interval for the flush cycle of the [`Aggregator`].
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// A snapshot of values within a [`Bucket`].
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub struct GaugeValue {
    /// The maximum value reported in the bucket.
    pub max: GaugeType,
    /// The minimum value reported in the bucket.
    pub min: GaugeType,
    /// The sum of all values reported in the bucket.
    pub sum: GaugeType,
    /// The last value reported in the bucket.
    ///
    /// This aggregation is not commutative.
    pub last: GaugeType,
    /// The number of times this bucket was updated with a new value.
    pub count: u64,
}

impl GaugeValue {
    /// Creates a gauge snapshot from a single value.
    pub fn single(value: GaugeType) -> Self {
        Self {
            max: value,
            min: value,
            sum: value,
            last: value,
            count: 1,
        }
    }

    /// Inserts a new value into the gauge.
    pub fn insert(&mut self, value: GaugeType) {
        self.max = self.max.max(value);
        self.min = self.min.min(value);
        self.sum += value;
        self.last = value;
        self.count += 1;
    }

    /// Merges two gauge snapshots.
    pub fn merge(&mut self, other: Self) {
        self.max = self.max.max(other.max);
        self.min = self.min.min(other.min);
        self.sum += other.sum;
        self.last = other.last;
        self.count += other.count;
    }

    /// Returns the average of all values reported in this bucket.
    pub fn avg(&self) -> GaugeType {
        if self.count > 0 {
            self.sum / (self.count as GaugeType)
        } else {
            0.0
        }
    }
}

/// Type for counting duplicates in distributions.
type Count = u32;

/// A distribution of values within a [`Bucket`].
///
/// Distributions store a histogram of values. It allows to iterate both the distribution with
/// [`iter`](Self::iter) and individual values with [`iter_values`](Self::iter_values).
///
/// Based on individual reported values, distributions allow to query the maximum, minimum, or
/// average of the reported values, as well as statistical quantiles.
///
/// # Example
///
/// ```rust
/// use relay_metrics::dist;
///
/// let mut dist = dist![1.0, 1.0, 1.0, 2.0];
/// dist.insert(5.0);
/// dist.insert_multi(3.0, 7);
/// ```
///
/// Logically, this distribution is equivalent to this visualization:
///
/// ```plain
/// value | count
/// 1.0   | ***
/// 2.0   | *
/// 3.0   | *******
/// 4.0   |
/// 5.0   | *
/// ```
///
/// # Serialization
///
/// Distributions serialize as sorted lists of floating point values. The list contains one entry
/// for each value in the distribution, including duplicates.
#[derive(Clone, Default, PartialEq)]
pub struct DistributionValue {
    values: BTreeMap<FloatOrd<DistributionType>, Count>,
    length: Count,
}

impl DistributionValue {
    /// Makes a new, empty `DistributionValue`.
    ///
    /// Does not allocate anything on its own.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of values in the map.
    ///
    /// # Examples
    ///
    /// ```
    /// use relay_metrics::DistributionValue;
    ///
    /// let mut dist = DistributionValue::new();
    /// assert_eq!(dist.len(), 0);
    /// dist.insert(1.0);
    /// dist.insert(1.0);
    /// assert_eq!(dist.len(), 2);
    /// ```
    pub fn len(&self) -> Count {
        self.length
    }

    /// Returns `true` if the map contains no elements.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Adds a value to the distribution.
    ///
    /// Returns the number this value occurs in the distribution after inserting.
    ///
    /// # Examples
    ///
    /// ```
    /// use relay_metrics::DistributionValue;
    ///
    /// let mut dist = DistributionValue::new();
    /// assert_eq!(dist.insert(1.0), 1);
    /// assert_eq!(dist.insert(1.0), 2);
    /// assert_eq!(dist.insert(2.0), 1);
    /// ```
    pub fn insert(&mut self, value: DistributionType) -> Count {
        self.insert_multi(value, 1)
    }

    /// Adds a value multiple times to the distribution.
    ///
    /// Returns the number this value occurs in the distribution after inserting.
    ///
    /// # Examples
    ///
    /// ```
    /// use relay_metrics::DistributionValue;
    ///
    /// let mut dist = DistributionValue::new();
    /// assert_eq!(dist.insert_multi(1.0, 2), 2);
    /// assert_eq!(dist.insert_multi(1.0, 3), 5);
    /// ```
    pub fn insert_multi(&mut self, value: DistributionType, count: Count) -> Count {
        self.length += count;
        if count == 0 {
            return 0;
        }

        *self
            .values
            .entry(FloatOrd(value))
            .and_modify(|c| *c += count)
            .or_insert(count)
    }

    /// Returns `true` if the set contains a value.
    ///
    /// # Examples
    ///
    /// ```
    /// use relay_metrics::dist;
    ///
    /// let dist = dist![1.0];
    ///
    /// assert_eq!(dist.contains(1.0), true);
    /// assert_eq!(dist.contains(2.0), false);
    /// ```
    pub fn contains(&self, value: impl std::borrow::Borrow<DistributionType>) -> bool {
        self.values.contains_key(&FloatOrd(*value.borrow()))
    }

    /// Returns how often the given value occurs in the distribution.
    ///
    /// # Examples
    ///
    /// ```
    /// use relay_metrics::dist;
    ///
    /// let dist = dist![1.0, 1.0];
    ///
    /// assert_eq!(dist.get(1.0), 2);
    /// assert_eq!(dist.get(2.0), 0);
    /// ```
    pub fn get(&self, value: impl std::borrow::Borrow<DistributionType>) -> Count {
        let value = &FloatOrd(*value.borrow());
        self.values.get(value).copied().unwrap_or(0)
    }

    /// Gets an iterator that visits unique values in the `DistributionValue` in ascending order.
    ///
    /// The iterator yields pairs of values and their count in the distribution.
    ///
    /// # Examples
    ///
    /// ```
    /// use relay_metrics::dist;
    ///
    /// let dist = dist![2.0, 1.0, 3.0, 2.0];
    ///
    /// let mut iter = dist.iter();
    /// assert_eq!(iter.next(), Some((1.0, 1)));
    /// assert_eq!(iter.next(), Some((2.0, 2)));
    /// assert_eq!(iter.next(), Some((3.0, 1)));
    /// assert_eq!(iter.next(), None);
    /// ```
    pub fn iter(&self) -> DistributionIter<'_> {
        DistributionIter {
            inner: self.values.iter(),
        }
    }

    /// Gets an iterator that visits the values in the `DistributionValue` in ascending order.
    ///
    /// # Examples
    ///
    /// ```
    /// use relay_metrics::dist;
    ///
    /// let dist = dist![2.0, 1.0, 3.0, 2.0];
    ///
    /// let mut iter = dist.iter_values();
    /// assert_eq!(iter.next(), Some(1.0));
    /// assert_eq!(iter.next(), Some(2.0));
    /// assert_eq!(iter.next(), Some(2.0));
    /// assert_eq!(iter.next(), Some(3.0));
    /// assert_eq!(iter.next(), None);
    /// ```
    pub fn iter_values(&self) -> DistributionValuesIter<'_> {
        DistributionValuesIter {
            inner: self.iter(),
            current: 0f64,
            remaining: 0,
            total: self.length,
        }
    }
}

impl<'a> IntoIterator for &'a DistributionValue {
    type Item = (DistributionType, Count);
    type IntoIter = DistributionIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for DistributionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl Extend<f64> for DistributionValue {
    fn extend<T: IntoIterator<Item = f64>>(&mut self, iter: T) {
        for value in iter.into_iter() {
            self.insert(value);
        }
    }
}

impl Extend<(f64, Count)> for DistributionValue {
    fn extend<T: IntoIterator<Item = (DistributionType, Count)>>(&mut self, iter: T) {
        for (value, count) in iter.into_iter() {
            self.insert_multi(value, count);
        }
    }
}

impl Serialize for DistributionValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_seq(self.iter_values())
    }
}

impl<'de> Deserialize<'de> for DistributionValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DistributionVisitor;

        impl<'d> serde::de::Visitor<'d> for DistributionVisitor {
            type Value = DistributionValue;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "a list of floating point values")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'d>,
            {
                let mut distribution = DistributionValue::new();

                while let Some(value) = seq.next_element()? {
                    distribution.insert(value);
                }

                Ok(distribution)
            }
        }

        deserializer.deserialize_seq(DistributionVisitor)
    }
}

/// An iterator over distribution entries in a [`DistributionValue`].
///
/// This struct is created by the [`iter`](DistributionValue::iter) method on
/// `DistributionValue`. See its documentation for more.
#[derive(Clone)]
pub struct DistributionIter<'a> {
    inner: btree_map::Iter<'a, FloatOrd<f64>, Count>,
}

impl Iterator for DistributionIter<'_> {
    type Item = (DistributionType, Count);

    fn next(&mut self) -> Option<Self::Item> {
        let (value, count) = self.inner.next()?;
        Some((value.0, *count))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for DistributionIter<'_> {}

impl fmt::Debug for DistributionIter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

/// An iterator over all individual values in a [`DistributionValue`].
///
/// This struct is created by the [`iter_values`](DistributionValue::iter_values) method on
/// `DistributionValue`. See its documentation for more.
#[derive(Clone)]
pub struct DistributionValuesIter<'a> {
    inner: DistributionIter<'a>,
    current: DistributionType,
    remaining: Count,
    total: Count,
}

impl Iterator for DistributionValuesIter<'_> {
    type Item = DistributionType;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining > 0 {
            self.remaining -= 1;
            self.total -= 1;
            return Some(self.current);
        }

        let (value, count) = self.inner.next()?;

        self.current = value;
        self.remaining = count - 1;
        self.total -= 1;
        Some(self.current)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.total as usize;
        (len, Some(len))
    }
}

impl ExactSizeIterator for DistributionValuesIter<'_> {}

impl fmt::Debug for DistributionValuesIter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

/// Creates a [`DistributionValue`] containing the given arguments.
///
/// `dist!` allows `DistributionValue` to be defined with the same syntax as array expressions.
///
/// # Example
///
/// ```
/// let dist = relay_metrics::dist![1.0, 2.0];
/// ```
#[macro_export]
macro_rules! dist {
    () => {
        $crate::DistributionValue::new()
    };
    ($($x:expr),+ $(,)?) => {{
        let mut distribution = $crate::DistributionValue::new();
        $( distribution.insert($x); )*
        distribution
    }};
}

/// The [aggregated value](Bucket::value) of a metric bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum BucketValue {
    /// Aggregates [`MetricValue::Counter`] values by adding them into a single value.
    ///
    /// ```text
    /// 2, 1, 3, 2 => 8
    /// ```
    ///
    /// This variant serializes to a double precision float.
    #[serde(rename = "c")]
    Counter(CounterType),
    /// Aggregates [`MetricValue::Distribution`] values by collecting their values.
    ///
    /// ```text
    /// 2, 1, 3, 2 => [1, 2, 2, 3]
    /// ```
    ///
    /// This variant serializes to a list of double precision floats, see [`DistributionValue`].
    #[serde(rename = "d")]
    Distribution(DistributionValue),
    /// Aggregates [`MetricValue::Set`] values by storing their hash values in a set.
    ///
    /// ```text
    /// 2, 1, 3, 2 => {1, 2, 3}
    /// ```
    ///
    /// This variant serializes to a list of 32-bit integers.
    #[serde(rename = "s")]
    Set(BTreeSet<SetType>),
    /// Aggregates [`MetricValue::Gauge`] values always retaining the maximum, minimum, and last
    /// value, as well as the sum and count of all values.
    ///
    /// **Note**: The "last" component of this aggregation is not commutative.
    ///
    /// ```text
    /// 1, 2, 3, 2 => {
    ///   max: 3,
    ///   min: 1,
    ///   sum: 8,
    ///   last: 2
    ///   count: 4,
    /// }
    /// ```
    ///
    /// This variant serializes to a structure, see [`GaugeValue`].
    #[serde(rename = "g")]
    Gauge(GaugeValue),
}

impl BucketValue {
    /// Returns the type of this value.
    pub fn ty(&self) -> MetricType {
        match self {
            Self::Counter(_) => MetricType::Counter,
            Self::Distribution(_) => MetricType::Distribution,
            Self::Set(_) => MetricType::Set,
            Self::Gauge(_) => MetricType::Gauge,
        }
    }

    /// Estimates the number of bytes needed to encode the bucket value.
    /// Note that this does not necessarily match the exact memory footprint of the value,
    /// because datastructures might have a memory overhead.
    pub fn cost(&self) -> usize {
        // Beside the size of [`BucketValue`], we also need to account for the cost of values
        // allocated dynamically.
        let allocated_cost = match self {
            Self::Counter(_) => 0,
            Self::Set(s) => mem::size_of::<SetType>() * s.len(),
            Self::Gauge(_) => 0,
            Self::Distribution(m) => {
                m.values.len() * (mem::size_of::<DistributionType>() + mem::size_of::<Count>())
            }
        };

        mem::size_of::<Self>() + allocated_cost
    }
}

impl From<MetricValue> for BucketValue {
    fn from(value: MetricValue) -> Self {
        match value {
            MetricValue::Counter(value) => Self::Counter(value),
            MetricValue::Distribution(value) => Self::Distribution(dist![value]),
            MetricValue::Set(value) => Self::Set(std::iter::once(value).collect()),
            MetricValue::Gauge(value) => Self::Gauge(GaugeValue::single(value)),
        }
    }
}

/// A value that can be merged into a [`BucketValue`].
///
/// Currently either a [`MetricValue`] or another `BucketValue`.
trait MergeValue: Into<BucketValue> {
    /// Merges `self` into the given `bucket_value` and returns the additional cost for storing this value.
    ///
    /// Aggregation is performed according to the rules documented in [`BucketValue`].
    fn merge_into(self, bucket_value: &mut BucketValue) -> Result<(), AggregateMetricsError>;
}

impl MergeValue for BucketValue {
    fn merge_into(self, bucket_value: &mut BucketValue) -> Result<(), AggregateMetricsError> {
        match (bucket_value, self) {
            (BucketValue::Counter(lhs), BucketValue::Counter(rhs)) => *lhs += rhs,
            (BucketValue::Distribution(lhs), BucketValue::Distribution(rhs)) => lhs.extend(&rhs),
            (BucketValue::Set(lhs), BucketValue::Set(rhs)) => lhs.extend(rhs),
            (BucketValue::Gauge(lhs), BucketValue::Gauge(rhs)) => lhs.merge(rhs),
            _ => return Err(AggregateMetricsErrorKind::InvalidTypes.into()),
        }

        Ok(())
    }
}

impl MergeValue for MetricValue {
    fn merge_into(self, bucket_value: &mut BucketValue) -> Result<(), AggregateMetricsError> {
        match (bucket_value, self) {
            (BucketValue::Counter(counter), MetricValue::Counter(value)) => {
                *counter += value;
            }
            (BucketValue::Distribution(distribution), MetricValue::Distribution(value)) => {
                distribution.insert(value);
            }
            (BucketValue::Set(set), MetricValue::Set(value)) => {
                set.insert(value);
            }
            (BucketValue::Gauge(gauge), MetricValue::Gauge(value)) => {
                gauge.insert(value);
            }
            _ => {
                return Err(AggregateMetricsErrorKind::InvalidTypes.into());
            }
        }

        Ok(())
    }
}

/// Error returned when parsing or serializing a [`Bucket`].
#[derive(Debug, Fail)]
#[fail(display = "failed to parse metric bucket")]
pub struct ParseBucketError(#[cause] serde_json::Error);

/// An aggregation of metric values by the [`Aggregator`].
///
/// As opposed to single metric values, bucket aggregations can carry multiple values. See
/// [`MetricType`] for a description on how values are aggregated in buckets. Values are aggregated
/// by metric name, type, time window, and all tags. Particularly, this allows metrics to have the
/// same name even if their types differ.
///
/// See the [crate documentation](crate) for general information on Metrics.
///
/// # Values
///
/// The contents of a bucket, especially their representation and serialization, depend on the
/// metric type:
///
/// - [Counters](BucketValue::Counter) store a single value, serialized as floating point.
/// - [Distributions](MetricType::Distribution) and [sets](MetricType::Set) store the full set of
///   reported values.
/// - [Gauges](BucketValue::Gauge) store a snapshot of reported values, see [`GaugeValue`].
///
/// # Submission Protocol
///
/// Buckets are always represented as JSON. The data type of the `value` field is determined by the
/// metric type.
///
/// ```json
/// [
///   {
///     "timestamp": 1615889440,
///     "name": "endpoint.response_time",
///     "type": "d",
///     "unit": "millisecond",
///     "value": [36, 49, 57, 68],
///     "tags": {
///       "route": "user_index"
///     }
///   },
///   {
///     "timestamp": 1615889440,
///     "name": "endpoint.hits",
///     "type": "c",
///     "value": 4,
///     "tags": {
///       "route": "user_index"
///     }
///   },
///   {
///     "timestamp": 1615889440,
///     "name": "endpoint.parallel_requests",
///     "type": "g",
///     "value": {
///       "max": 42.0,
///       "min": 17.0,
///       "sum": 2210.0,
///       "last": 25.0,
///       "count": 85
///     }
///   },
///   {
///     "timestamp": 1615889440,
///     "name": "endpoint.users",
///     "type": "s",
///     "value": [
///       3182887624,
///       4267882815
///     ],
///     "tags": {
///       "route": "user_index"
///     }
///   }
/// ]
/// ```
///
/// To parse a submission payload, use [`Bucket::parse_all`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Bucket {
    /// The start time of the time window.
    pub timestamp: UnixTimestamp,
    /// The length of the time window in seconds.
    pub width: u64,
    /// The name of the metric without its unit.
    ///
    /// See [`Metric::name`].
    pub name: String,
    /// The unit of the metric value.
    ///
    /// See [`Metric::unit`].
    #[serde(default, skip_serializing_if = "MetricUnit::is_none")]
    pub unit: MetricUnit,
    /// The type and aggregated values of this bucket.
    ///
    /// See [`Metric::value`] for a mapping to inbound data.
    #[serde(flatten)]
    pub value: BucketValue,
    /// A list of tags adding dimensions to the metric for filtering and aggregation.
    ///
    /// See [`Metric::tags`]. Every combination of tags results in a different bucket.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl Bucket {
    fn from_parts(key: BucketKey, bucket_interval: u64, value: BucketValue) -> Self {
        Self {
            timestamp: key.timestamp,
            width: bucket_interval,
            name: key.metric_name,
            unit: key.metric_unit,
            value,
            tags: key.tags,
        }
    }

    /// Parses a single metric bucket from the JSON protocol.
    pub fn parse(slice: &[u8]) -> Result<Self, ParseBucketError> {
        serde_json::from_slice(slice).map_err(ParseBucketError)
    }

    /// Parses a set of metric bucket from the JSON protocol.
    pub fn parse_all(slice: &[u8]) -> Result<Vec<Bucket>, ParseBucketError> {
        serde_json::from_slice(slice).map_err(ParseBucketError)
    }

    /// Serializes the given buckets to the JSON protocol.
    pub fn serialize_all(buckets: &[Self]) -> Result<String, ParseBucketError> {
        serde_json::to_string(&buckets).map_err(ParseBucketError)
    }
}

/// Any error that may occur during aggregation.
#[derive(Debug, Fail, PartialEq)]
#[fail(display = "failed to aggregate metrics: {}", kind)]
pub struct AggregateMetricsError {
    kind: AggregateMetricsErrorKind,
}

impl From<AggregateMetricsErrorKind> for AggregateMetricsError {
    fn from(kind: AggregateMetricsErrorKind) -> Self {
        AggregateMetricsError { kind }
    }
}

#[derive(Debug, Fail, PartialEq)]
#[allow(clippy::enum_variant_names)]
enum AggregateMetricsErrorKind {
    /// A metric bucket had invalid characters in the metric name.
    #[fail(display = "found invalid characters")]
    InvalidCharacters,
    /// A metric bucket's timestamp was out of the configured acceptable range.
    #[fail(display = "found invalid timestamp")]
    InvalidTimestamp,
    /// Internal error: Attempted to merge two metric buckets of different types.
    #[fail(display = "found incompatible metric types")]
    InvalidTypes,
    /// A metric bucket had a too long string (metric name or a tag key/value).
    #[fail(display = "found invalid string")]
    InvalidStringLength,
    /// A metric bucket is too large for the global bytes limit.
    #[fail(display = "total metrics limit exceeded")]
    TotalLimitExceeded,
    /// A metric bucket is too large for the per-project bytes limit.
    #[fail(display = "project metrics limit exceeded")]
    ProjectLimitExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BucketKey {
    project_key: ProjectKey,
    timestamp: UnixTimestamp,
    metric_name: String,
    metric_type: MetricType,
    metric_unit: MetricUnit,
    tags: BTreeMap<String, String>,
}

impl BucketKey {
    /// An extremely hamfisted way to hash a bucket key into an integer.
    ///
    /// This is necessary for (and probably only useful for) reporting unique bucket keys in a
    /// cadence set metric, as cadence set metrics can only be constructed from values that
    /// implement [`cadence::ext::ToSetValue`].  This trait is only implemented for [`i64`], and
    /// while we could implement it directly for [`BucketKey`] the documentation advises us not to
    /// interact with this trait.
    ///
    /// [`cadence::ext::ToSetValue`]: https://docs.rs/cadence/*/cadence/ext/trait.ToSetValue.html
    fn as_integer_lossy(&self) -> i64 {
        // XXX: The way this hasher is used may be platform-dependent. If we want to produce the
        // same hash across platforms, the `deterministic_hash` crate may be useful.
        let mut hasher = crc32fast::Hasher::new();
        std::hash::Hash::hash(self, &mut hasher);
        hasher.finalize() as i64
    }

    /// Estimates the number of bytes needed to encode the bucket key.
    /// Note that this does not necessarily match the exact memory footprint of the key,
    /// because datastructures might have a memory overhead.
    fn cost(&self) -> usize {
        mem::size_of::<Self>()
            + self.metric_name.capacity()
            + self
                .tags
                .iter()
                .fold(0, |acc, (k, v)| acc + k.capacity() + v.capacity())
    }
}

/// Parameters used by the [`Aggregator`].
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AggregatorConfig {
    /// Determines the wall clock time interval for buckets in seconds.
    ///
    /// Defaults to `10` seconds. Every metric is sorted into a bucket of this size based on its
    /// timestamp. This defines the minimum granularity with which metrics can be queried later.
    pub bucket_interval: u64,

    /// The initial delay in seconds to wait before flushing a bucket.
    ///
    /// Defaults to `30` seconds. Before sending an aggregated bucket, this is the time Relay waits
    /// for buckets that are being reported in real time. This should be higher than the
    /// `debounce_delay`.
    ///
    /// Relay applies up to a full `bucket_interval` of additional jitter after the initial delay to spread out flushing real time buckets.
    pub initial_delay: u64,

    /// The delay in seconds to wait before flushing a backdated buckets.
    ///
    /// Defaults to `10` seconds. Metrics can be sent with a past timestamp. Relay wait this time
    /// before sending such a backdated bucket to the upsteam. This should be lower than
    /// `initial_delay`.
    ///
    /// Unlike `initial_delay`, the debounce delay starts with the exact moment the first metric
    /// is added to a backdated bucket.
    pub debounce_delay: u64,

    /// The age in seconds of the oldest allowed bucket timestamp.
    ///
    /// Defaults to 5 days.
    pub max_secs_in_past: u64,

    /// The time in seconds that a timestamp may be in the future.
    ///
    /// Defaults to 1 minute.
    pub max_secs_in_future: u64,

    /// The length the name of a metric is allowed to be.
    ///
    /// Defaults to `200` bytes.
    pub max_name_length: usize,

    /// The length the tag key is allowed to be.
    ///
    /// Defaults to `200` bytes.
    pub max_tag_key_length: usize,

    /// The length the tag value is allowed to be.
    ///
    /// Defaults to `200` bytes.
    pub max_tag_value_length: usize,

    /// Maximum amount of bytes used for metrics aggregation.
    ///
    /// When aggregating metrics, Relay keeps track of how many bytes a metric takes in memory.
    /// This is only an approximation and does not take into account things such as pre-allocation
    /// in hashmaps.
    ///
    /// Defaults to `None`, i.e. no limit.
    pub max_total_bucket_bytes: Option<usize>,

    /// Maximum amount of bytes used for metrics aggregation per project key.
    ///
    /// Similar measuring technique to `max_total_bucket_bytes`, but instead of a
    /// global/process-wide limit, it is enforced per project key.
    ///
    /// Defaults to `None`, i.e. no limit.
    pub max_project_key_bucket_bytes: Option<usize>,
}

impl AggregatorConfig {
    /// Returns the time width buckets.
    fn bucket_interval(&self) -> Duration {
        Duration::from_secs(self.bucket_interval)
    }

    /// Returns the initial flush delay after the end of a bucket's original time window.
    fn initial_delay(&self) -> Duration {
        Duration::from_secs(self.initial_delay)
    }

    /// The delay to debounce backdated flushes.
    fn debounce_delay(&self) -> Duration {
        Duration::from_secs(self.debounce_delay)
    }

    /// Determines the target bucket for an incoming bucket timestamp and bucket width.
    ///
    /// We select the output bucket which overlaps with the center of the incoming bucket.
    /// Fails if timestamp is too old or too far into the future.
    fn get_bucket_timestamp(
        &self,
        timestamp: UnixTimestamp,
        bucket_width: u64,
    ) -> Result<UnixTimestamp, AggregateMetricsError> {
        // We know this must be UNIX timestamp because we need reliable match even with system
        // clock skew over time.

        let now = UnixTimestamp::now().as_secs();
        let min_timestamp = UnixTimestamp::from_secs(now.saturating_sub(self.max_secs_in_past));
        let max_timestamp = UnixTimestamp::from_secs(now.saturating_add(self.max_secs_in_future));

        // Find middle of the input bucket to select a target
        let ts = timestamp.as_secs().saturating_add(bucket_width / 2);

        // Align target_timestamp to output bucket width
        let ts = (ts / self.bucket_interval) * self.bucket_interval;

        let output_timestamp = UnixTimestamp::from_secs(ts);

        if output_timestamp < min_timestamp || output_timestamp > max_timestamp {
            return Err(AggregateMetricsErrorKind::InvalidTimestamp.into());
        }

        Ok(output_timestamp)
    }

    /// Returns the instant at which a bucket should be flushed.
    ///
    /// Recent buckets are flushed after a grace period of `initial_delay`. Backdated buckets, that
    /// is, buckets that lie in the past, are flushed after the shorter `debounce_delay`.
    fn get_flush_time(&self, bucket_timestamp: UnixTimestamp, project_key: ProjectKey) -> Instant {
        let now = Instant::now();
        let mut flush = None;

        if let MonotonicResult::Instant(instant) = bucket_timestamp.to_instant() {
            let bucket_end = instant + self.bucket_interval();
            let initial_flush = bucket_end + self.initial_delay();
            // If the initial flush is still pending, use that.
            if initial_flush > now {
                // Shift deterministically within one bucket interval based on the project key. This
                // distributes buckets over time while also flushing all buckets of the same project
                // key together.
                let mut hasher = FnvHasher::default();
                hasher.write(project_key.as_str().as_bytes());
                let shift_millis = u64::from(hasher.finish()) % (self.bucket_interval * 1000);

                flush = Some(initial_flush + Duration::from_millis(shift_millis));
            }
        }

        let delay = UnixTimestamp::now().as_secs() as i64 - bucket_timestamp.as_secs() as i64;
        relay_statsd::metric!(
            histogram(MetricHistograms::BucketsDelay) = delay as f64,
            backedated = if flush.is_none() { "true" } else { "false" },
        );

        // If the initial flush time has passed or cannot be represented, debounce future flushes
        // with the `debounce_delay` starting now.
        match flush {
            Some(initial_flush) => initial_flush,
            None => now + self.debounce_delay(),
        }
    }
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        Self {
            bucket_interval: 10,
            initial_delay: 30,
            debounce_delay: 10,
            max_secs_in_past: 5 * 24 * 60 * 60, // 5 days, as for sessions
            max_secs_in_future: 60,             // 1 minute
            max_name_length: 200,
            max_tag_key_length: 200,
            max_tag_value_length: 200,
            max_total_bucket_bytes: None,
            max_project_key_bucket_bytes: None,
        }
    }
}

/// Bucket in the [`Aggregator`] with a defined flush time.
///
/// This type implements an inverted total ordering. The maximum queued bucket has the lowest flush
/// time, which is suitable for using it in a [`BinaryHeap`].
///
/// [`BinaryHeap`]: std::collections::BinaryHeap
#[derive(Debug)]
struct QueuedBucket {
    flush_at: Instant,
    value: BucketValue,
}

impl QueuedBucket {
    /// Creates a new `QueuedBucket` with a given flush time.
    fn new(flush_at: Instant, value: BucketValue) -> Self {
        Self { flush_at, value }
    }

    /// Returns `true` if the flush time has elapsed.
    fn elapsed(&self) -> bool {
        Instant::now() > self.flush_at
    }
}

impl PartialEq for QueuedBucket {
    fn eq(&self, other: &Self) -> bool {
        self.flush_at.eq(&other.flush_at)
    }
}

impl Eq for QueuedBucket {}

impl PartialOrd for QueuedBucket {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // Comparing order is reversed to convert the max heap into a min heap
        other.flush_at.partial_cmp(&self.flush_at)
    }
}

impl Ord for QueuedBucket {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Comparing order is reversed to convert the max heap into a min heap
        other.flush_at.cmp(&self.flush_at)
    }
}

/// A message containing a vector of buckets to be flushed.
///
/// Use [`into_buckets`](Self::into_buckets) to access the raw [`Bucket`]s. Handlers must respond to
/// this message with a `Result`:
/// - If flushing has succeeded or the buckets should be dropped for any reason, respond with `Ok`.
/// - If flushing fails and should be retried at a later time, respond with `Err` containing the
///   failed buckets. They will be merged back into the aggregator and flushed at a later time.
#[derive(Clone, Debug)]
pub struct FlushBuckets {
    /// the project key
    project_key: ProjectKey,
    buckets: Vec<Bucket>,
}

impl FlushBuckets {
    /// Creates a new message by consuming a vector of buckets.
    pub fn new(project_key: ProjectKey, buckets: Vec<Bucket>) -> Self {
        Self {
            project_key,
            buckets,
        }
    }

    /// Consumes the buckets contained in this message.
    pub fn into_buckets(self) -> Vec<Bucket> {
        self.buckets
    }

    /// Returns the project key (formally project public key)
    pub fn project_key(&self) -> ProjectKey {
        self.project_key
    }
}

impl Message for FlushBuckets {
    type Result = Result<(), Vec<Bucket>>;
}

/// Check whether the aggregator has not (yet) exceeded its total limits. Used for healthchecks.
pub struct AcceptsMetrics;

impl Message for AcceptsMetrics {
    type Result = bool;
}

impl Handler<AcceptsMetrics> for Aggregator {
    type Result = bool;

    fn handle(&mut self, _msg: AcceptsMetrics, _ctx: &mut Self::Context) -> Self::Result {
        !self
            .cost_tracker
            .totals_cost_exceeded(self.config.max_total_bucket_bytes)
    }
}

enum AggregatorState {
    Running,
    ShuttingDown,
}

#[derive(Default)]
struct CostTracker {
    total_cost: usize,
    cost_per_project_key: HashMap<ProjectKey, usize>,
}

impl CostTracker {
    fn totals_cost_exceeded(&self, max_total_cost: Option<usize>) -> bool {
        if let Some(max_total_cost) = max_total_cost {
            if self.total_cost >= max_total_cost {
                return true;
            }
        }

        false
    }

    fn check_limits_exceeded(
        &self,
        project_key: ProjectKey,
        max_total_cost: Option<usize>,
        max_project_cost: Option<usize>,
    ) -> Result<(), AggregateMetricsError> {
        if self.totals_cost_exceeded(max_total_cost) {
            relay_log::configure_scope(|scope| {
                scope.set_extra("bucket.project_key", project_key.as_str().to_owned().into());
            });
            return Err(AggregateMetricsErrorKind::TotalLimitExceeded.into());
        }

        if let Some(max_project_cost) = max_project_cost {
            let project_cost = self
                .cost_per_project_key
                .get(&project_key)
                .cloned()
                .unwrap_or(0);
            if project_cost >= max_project_cost {
                relay_log::configure_scope(|scope| {
                    scope.set_extra("bucket.project_key", project_key.as_str().to_owned().into());
                });
                return Err(AggregateMetricsErrorKind::ProjectLimitExceeded.into());
            }
        }

        Ok(())
    }

    fn add_cost(&mut self, project_key: ProjectKey, cost: usize) {
        self.total_cost += cost;
        let project_cost = self.cost_per_project_key.entry(project_key).or_insert(0);
        *project_cost += cost;
    }

    fn subtract_cost(&mut self, project_key: ProjectKey, cost: usize) {
        match self.cost_per_project_key.entry(project_key) {
            Entry::Vacant(_) => {
                relay_log::error!(
                    "Trying to subtract cost for a project key that has not been tracked"
                );
            }
            Entry::Occupied(mut entry) => {
                // Handle per-project cost:
                let project_cost = entry.get_mut();
                if cost > *project_cost {
                    relay_log::error!("Subtracting a project cost higher than what we tracked");
                    self.total_cost = self.total_cost.saturating_sub(*project_cost);
                    *project_cost = 0;
                } else {
                    *project_cost -= cost;
                    self.total_cost = self.total_cost.saturating_sub(cost);
                }
                if *project_cost == 0 {
                    // Remove this project_key from the map
                    entry.remove();
                }
            }
        };
    }
}

impl fmt::Debug for CostTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CostTracker")
            .field("total_cost", &self.total_cost)
            .field(
                "cost_per_project_key",
                &BTreeMap::from_iter(self.cost_per_project_key.iter()),
            )
            .finish()
    }
}

/// A collector of [`Metric`] submissions.
///
/// # Aggregation
///
/// Each metric is dispatched into the a [`Bucket`] depending on its project key (DSN), name, type,
/// unit, tags and timestamp. The bucket timestamp is rounded to the precision declared by the
/// `bucket_interval` field on the [AggregatorConfig] configuration.
///
/// Each bucket stores the accumulated value of submitted metrics:
///
/// - `Counter`: Sum of values.
/// - `Distribution`: A list of values.
/// - `Set`: A unique set of hashed values.
/// - `Gauge`: A summary of the reported values, see [`GaugeValue`].
///
/// # Conflicts
///
/// Metrics are uniquely identified by the combination of their name, type and unit. It is allowed
/// to send metrics of different types and units under the same name. For example, sending a metric
/// once as set and once as distribution will result in two actual metrics being recorded.
///
/// # Flushing
///
/// Buckets are flushed to a receiver after their time window and a grace period have passed.
/// Metrics with a recent timestamp are given a longer grace period than backdated metrics, which
/// are flushed after a shorter debounce delay. See [`AggregatorConfig`] for configuration options.
///
/// Internally, the aggregator maintains a continuous flush cycle every 100ms. It guarantees that
/// all elapsed buckets belonging to the same [`ProjectKey`] are flushed together.
///
/// Receivers must implement a handler for the [`FlushBuckets`] message:
///
/// ```
/// use actix::prelude::*;
/// use relay_metrics::{Bucket, FlushBuckets};
///
/// struct BucketReceiver;
///
/// impl Actor for BucketReceiver {
///     type Context = Context<Self>;
/// }
///
/// impl Handler<FlushBuckets> for BucketReceiver {
///     type Result = Result<(), Vec<Bucket>>;
///
///     fn handle(&mut self, msg: FlushBuckets, _ctx: &mut Self::Context) -> Self::Result {
///         // Return `Ok` to consume the buckets or `Err` to send them back
///         Err(msg.into_buckets())
///     }
/// }
/// ```
pub struct Aggregator {
    config: AggregatorConfig,
    buckets: HashMap<BucketKey, QueuedBucket>,
    receiver: Recipient<FlushBuckets>,
    state: AggregatorState,
    cost_tracker: CostTracker,
}

impl Aggregator {
    /// Create a new aggregator and connect it to `receiver`.
    ///
    /// The aggregator will flush a list of buckets to the receiver in regular intervals based on
    /// the given `config`.
    pub fn new(config: AggregatorConfig, receiver: Recipient<FlushBuckets>) -> Self {
        Self {
            config,
            buckets: HashMap::new(),
            receiver,
            state: AggregatorState::Running,
            cost_tracker: CostTracker::default(),
        }
    }

    /// Validates the metric name and its tags are correct.
    ///
    /// Returns `Err` if the metric should be dropped.
    fn validate_bucket_key(
        mut key: BucketKey,
        aggregator_config: &AggregatorConfig,
    ) -> Result<BucketKey, AggregateMetricsError> {
        key = Self::validate_metric_name(key, aggregator_config)?;
        key = Self::validate_metric_tags(key, aggregator_config);
        Ok(key)
    }

    /// Removes invalid characters from metric names.
    ///
    /// Returns `Err` if the metric must be dropped.
    fn validate_metric_name(
        key: BucketKey,
        aggregator_config: &AggregatorConfig,
    ) -> Result<BucketKey, AggregateMetricsError> {
        let metric_name_length = key.metric_name.len();
        if metric_name_length > aggregator_config.max_name_length {
            relay_log::configure_scope(|scope| {
                scope.set_extra(
                    "bucket.project_key",
                    key.project_key.as_str().to_owned().into(),
                );
                scope.set_extra("bucket.metric_name", key.metric_name.into());
                scope.set_extra(
                    "bucket.metric_name.length",
                    metric_name_length.to_string().into(),
                );
                scope.set_extra(
                    "aggregator_config.max_name_length",
                    aggregator_config.max_name_length.to_string().into(),
                );
            });
            return Err(AggregateMetricsErrorKind::InvalidStringLength.into());
        }

        if !protocol::is_valid_mri(&key.metric_name) {
            relay_log::debug!("invalid metric name {:?}", key.metric_name);
            relay_log::configure_scope(|scope| {
                scope.set_extra(
                    "bucket.project_key",
                    key.project_key.as_str().to_owned().into(),
                );
                scope.set_extra("bucket.metric_name", key.metric_name.into());
            });
            return Err(AggregateMetricsErrorKind::InvalidCharacters.into());
        }
        Ok(key)
    }

    /// Removes tags with invalid characters in the key, and validates tag values.
    ///
    /// Tag values are validated with `protocol::validate_tag_value`.
    fn validate_metric_tags(mut key: BucketKey, aggregator_config: &AggregatorConfig) -> BucketKey {
        let proj_key = key.project_key.as_str();
        key.tags.retain(|tag_key, tag_value| {
            if tag_key.len() > aggregator_config.max_tag_key_length {
                relay_log::configure_scope(|scope| {
                    scope.set_extra("bucket.project_key", proj_key.to_owned().into());
                    scope.set_extra("bucket.metric.tag_key", tag_key.to_owned().into());
                    scope.set_extra(
                        "aggregator_config.max_tag_key_length",
                        aggregator_config.max_tag_key_length.to_string().into(),
                    );
                });
                relay_log::debug!("Invalid metric tag key");
                return false;
            }
            if tag_value.len() > aggregator_config.max_tag_value_length {
                relay_log::configure_scope(|scope| {
                    scope.set_extra("bucket.project_key", proj_key.to_owned().into());
                    scope.set_extra("bucket.metric.tag_value", tag_value.to_owned().into());
                    scope.set_extra(
                        "aggregator_config.max_tag_value_length",
                        aggregator_config.max_tag_value_length.to_string().into(),
                    );
                });
                relay_log::debug!("Invalid metric tag value");
                return false;
            }

            if protocol::is_valid_tag_key(tag_key) {
                true
            } else {
                relay_log::debug!("invalid metric tag key {:?}", tag_key);
                false
            }
        });
        for (_, tag_value) in key.tags.iter_mut() {
            protocol::validate_tag_value(tag_value);
        }
        key
    }

    /// Merges any mergeable value into the bucket at the given `key`.
    ///
    /// If no bucket exists for the given bucket key, a new bucket will be created.
    fn merge_in<T: MergeValue>(
        &mut self,
        key: BucketKey,
        value: T,
    ) -> Result<(), AggregateMetricsError> {
        let timestamp = key.timestamp;
        let project_key = key.project_key;

        let key = Self::validate_bucket_key(key, &self.config)?;

        // XXX: This is not a great implementation of cost enforcement.
        //
        // * it takes two lookups of the project key in the cost tracker to merge a bucket: once in
        //   `check_limits_exceeded` and once in `add_cost`.
        //
        // * the limits are not actually enforced consistently
        //
        //   A bucket can be merged that exceeds the cost limit, and only the next bucket will be
        //   limited because the limit is now reached. This implementation was chosen because it's
        //   currently not possible to determine cost accurately upfront: The bucket values have to
        //   be merged together to figure out how costly the merge was. Changing that would force
        //   us to unravel a lot of abstractions that we have already built.
        //
        //   As a result of that, it is possible to exceed the bucket cost limit significantly
        //   until we have guaranteed upper bounds on the cost of a single bucket (which we
        //   currently don't, because a metric can have arbitrary amount of tag values).
        //
        //   Another consequence is that a MergeValue that adds zero cost (such as an existing
        //   counter bucket being incremented) is currently rejected even though it doesn't have to
        //   be.
        //
        // The flipside of this approach is however that there's more optimization potential: If
        // the limit is already exceeded, we could implement an optimization that drops envelope
        // items before they are parsed, as we can be sure that the new metric bucket will be
        // rejected in the aggregator regardless of whether it is merged into existing buckets,
        // whether it is just a counter, etc.
        self.cost_tracker.check_limits_exceeded(
            project_key,
            self.config.max_total_bucket_bytes,
            self.config.max_project_key_bucket_bytes,
        )?;

        let added_cost;
        match self.buckets.entry(key) {
            Entry::Occupied(mut entry) => {
                relay_statsd::metric!(
                    counter(MetricCounters::MergeHit) += 1,
                    metric_type = entry.key().metric_type.as_str(),
                    metric_name = &entry.key().metric_name
                );
                let bucket_value = &mut entry.get_mut().value;
                let cost_before = bucket_value.cost();
                value.merge_into(bucket_value)?;
                let cost_after = bucket_value.cost();
                added_cost = cost_after.saturating_sub(cost_before);
            }
            Entry::Vacant(entry) => {
                relay_statsd::metric!(
                    counter(MetricCounters::MergeMiss) += 1,
                    metric_type = entry.key().metric_type.as_str(),
                    metric_name = &entry.key().metric_name
                );
                relay_statsd::metric!(
                    set(MetricSets::UniqueBucketsCreated) = entry.key().as_integer_lossy(),
                    metric_type = entry.key().metric_type.as_str(),
                    metric_name = &entry.key().metric_name
                );

                let flush_at = self.config.get_flush_time(timestamp, project_key);
                let bucket = value.into();
                added_cost = entry.key().cost() + bucket.cost();
                entry.insert(QueuedBucket::new(flush_at, bucket));
            }
        }

        self.cost_tracker.add_cost(project_key, added_cost);

        Ok(())
    }

    /// Inserts a metric into the corresponding bucket in this aggregator.
    ///
    /// If no bucket exists for the given bucket key, a new bucket will be created.
    pub fn insert(
        &mut self,
        project_key: ProjectKey,
        metric: Metric,
    ) -> Result<(), AggregateMetricsError> {
        relay_statsd::metric!(
            counter(MetricCounters::InsertMetric) += 1,
            metric_type = metric.value.ty().as_str(),
        );
        let key = BucketKey {
            project_key,
            timestamp: self.config.get_bucket_timestamp(metric.timestamp, 0)?,
            metric_name: metric.name,
            metric_type: metric.value.ty(),
            metric_unit: metric.unit,
            tags: metric.tags,
        };
        self.merge_in(key, metric.value)
    }

    /// Merge a preaggregated bucket into this aggregator.
    ///
    /// If no bucket exists for the given bucket key, a new bucket will be created.
    pub fn merge(
        &mut self,
        project_key: ProjectKey,
        bucket: Bucket,
    ) -> Result<(), AggregateMetricsError> {
        let key = BucketKey {
            project_key,
            timestamp: self
                .config
                .get_bucket_timestamp(bucket.timestamp, bucket.width)?,
            metric_name: bucket.name,
            metric_type: bucket.value.ty(),
            metric_unit: bucket.unit,
            tags: bucket.tags,
        };
        self.merge_in(key, bucket.value)
    }

    /// Merges all given `buckets` into this aggregator.
    ///
    /// Buckets that do not exist yet will be created.
    pub fn merge_all<I>(
        &mut self,
        project_key: ProjectKey,
        buckets: I,
    ) -> Result<(), AggregateMetricsError>
    where
        I: IntoIterator<Item = Bucket>,
    {
        for bucket in buckets.into_iter() {
            if let Err(error) = self.merge(project_key, bucket) {
                relay_log::error!("{}", error);
            }
        }

        Ok(())
    }

    /// Pop and return the buckets that are eligible for flushing out according to bucket interval.
    ///
    /// Note that this function is primarily intended for tests.
    pub fn pop_flush_buckets(&mut self) -> HashMap<ProjectKey, Vec<Bucket>> {
        relay_statsd::metric!(gauge(MetricGauges::Buckets) = self.buckets.len() as u64);

        // We only emit statsd metrics for the cost on flush (and not when merging the buckets),
        // assuming that this gives us more than enough data points.
        relay_statsd::metric!(
            gauge(MetricGauges::BucketsCost) = self.cost_tracker.total_cost as u64
        );

        let mut buckets = HashMap::<ProjectKey, Vec<Bucket>>::new();

        let force = matches!(&self.state, AggregatorState::ShuttingDown);

        relay_statsd::metric!(timer(MetricTimers::BucketsScanDuration), {
            let bucket_interval = self.config.bucket_interval;
            let cost_tracker = &mut self.cost_tracker;
            self.buckets.retain(|key, entry| {
                if force || entry.elapsed() {
                    // Take the value and leave a placeholder behind. It'll be removed right after.
                    let value = mem::replace(&mut entry.value, BucketValue::Counter(0.0));
                    cost_tracker.subtract_cost(key.project_key, key.cost());
                    cost_tracker.subtract_cost(key.project_key, value.cost());
                    let bucket = Bucket::from_parts(key.clone(), bucket_interval, value);
                    buckets.entry(key.project_key).or_default().push(bucket);

                    false
                } else {
                    true
                }
            });
        });

        buckets
    }

    /// Sends the [`FlushBuckets`] message to the receiver.
    ///
    /// If the receiver returns buckets, they are merged back into the cache.
    /// If `force` is true, flush all buckets unconditionally and do not attempt to merge back.
    fn try_flush(&mut self, context: &mut <Self as Actor>::Context) {
        let flush_buckets = self.pop_flush_buckets();

        if flush_buckets.is_empty() {
            return;
        }

        relay_log::trace!("flushing {} projects to receiver", flush_buckets.len());

        let mut total_bucket_count = 0u64;
        for (project_key, project_buckets) in flush_buckets.into_iter() {
            let bucket_count = project_buckets.len() as u64;
            relay_statsd::metric!(
                histogram(MetricHistograms::BucketsFlushedPerProject) = bucket_count
            );
            total_bucket_count += bucket_count;

            self.receiver
                .send(FlushBuckets::new(project_key, project_buckets))
                .into_actor(self)
                .and_then(move |result, slf, _ctx| {
                    if let Err(buckets) = result {
                        relay_log::trace!(
                            "returned {} buckets from receiver, merging back",
                            buckets.len()
                        );
                        slf.merge_all(project_key, buckets).ok();
                    }
                    fut::ok(())
                })
                .drop_err()
                .spawn(context);
        }

        relay_statsd::metric!(histogram(MetricHistograms::BucketsFlushed) = total_bucket_count);
    }
}

impl fmt::Debug for Aggregator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(std::any::type_name::<Self>())
            .field("config", &self.config)
            .field("buckets", &self.buckets)
            .field("receiver", &format_args!("Recipient<FlushBuckets>"))
            .finish()
    }
}

impl Actor for Aggregator {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        relay_log::info!("aggregator started");

        // Subscribe to shutdown
        Controller::subscribe(ctx.address());

        // TODO: Consider a better approach than busy polling
        ctx.run_interval(FLUSH_INTERVAL, |slf, context| {
            slf.try_flush(context);
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        relay_log::info!("aggregator stopped");
    }
}

impl Default for Aggregator {
    fn default() -> Self {
        unimplemented!("register with the SystemRegistry instead")
    }
}

impl Supervised for Aggregator {}

impl SystemService for Aggregator {}

impl Handler<Shutdown> for Aggregator {
    type Result = Result<(), ()>;

    fn handle(&mut self, message: Shutdown, _context: &mut Self::Context) -> Self::Result {
        if message.timeout.is_some() {
            relay_log::trace!("Shutting down...");
            self.state = AggregatorState::ShuttingDown;
        }
        Ok(())
    }
}

impl Drop for Aggregator {
    fn drop(&mut self) {
        let remaining_buckets = self.buckets.len();
        if remaining_buckets > 0 {
            relay_log::error!("Metrics aggregator dropping {} buckets", remaining_buckets);
            relay_statsd::metric!(
                counter(MetricCounters::BucketsDropped) += remaining_buckets as i64
            );
        }
    }
}

/// A message containing a list of [`Metric`]s to be inserted into the aggregator.
#[derive(Debug)]
pub struct InsertMetrics {
    project_key: ProjectKey,
    metrics: Vec<Metric>,
}

impl InsertMetrics {
    /// Creates a new message containing a list of [`Metric`]s.
    pub fn new<I>(project_key: ProjectKey, metrics: I) -> Self
    where
        I: IntoIterator<Item = Metric>,
    {
        Self {
            project_key,
            metrics: metrics.into_iter().collect(),
        }
    }
}

impl Message for InsertMetrics {
    type Result = Result<(), AggregateMetricsError>;
}

impl Handler<InsertMetrics> for Aggregator {
    type Result = Result<(), AggregateMetricsError>;

    fn handle(&mut self, msg: InsertMetrics, _ctx: &mut Self::Context) -> Self::Result {
        for metric in msg.metrics {
            self.insert(msg.project_key, metric)?;
        }

        Ok(())
    }
}

/// A message containing a list of [`Bucket`]s to be inserted into the aggregator.
#[derive(Debug)]
pub struct MergeBuckets {
    project_key: ProjectKey,
    buckets: Vec<Bucket>,
}

impl MergeBuckets {
    /// Creates a new message containing a list of [`Bucket`]s.
    pub fn new(project_key: ProjectKey, buckets: Vec<Bucket>) -> Self {
        Self {
            project_key,
            buckets,
        }
    }
}

impl Message for MergeBuckets {
    type Result = Result<(), AggregateMetricsError>;
}

impl Handler<MergeBuckets> for Aggregator {
    type Result = Result<(), AggregateMetricsError>;

    fn handle(&mut self, msg: MergeBuckets, _ctx: &mut Self::Context) -> Self::Result {
        self.merge_all(msg.project_key, msg.buckets)
    }
}

#[cfg(test)]
mod tests {
    use futures::future::Future;
    use std::sync::{Arc, RwLock};

    use super::*;

    use relay_common::{DurationUnit, MetricUnit};

    struct BucketCountInquiry;

    impl Message for BucketCountInquiry {
        type Result = usize;
    }

    impl Handler<BucketCountInquiry> for Aggregator {
        type Result = usize;

        fn handle(&mut self, _: BucketCountInquiry, _: &mut Self::Context) -> Self::Result {
            self.buckets.len()
        }
    }

    #[derive(Default)]
    struct ReceivedData {
        buckets: Vec<Bucket>,
    }

    #[derive(Clone, Default)]
    struct TestReceiver {
        // TODO: Better way to communicate with Actor after it's started?
        // Messages, maybe?
        data: Arc<RwLock<ReceivedData>>,
        reject_all: bool,
    }

    impl TestReceiver {
        fn add_buckets(&self, buckets: Vec<Bucket>) {
            self.data.write().unwrap().buckets.extend(buckets);
        }

        fn bucket_count(&self) -> usize {
            self.data.read().unwrap().buckets.len()
        }
    }

    impl Actor for TestReceiver {
        type Context = Context<Self>;
    }

    impl Handler<FlushBuckets> for TestReceiver {
        type Result = Result<(), Vec<Bucket>>;

        fn handle(&mut self, msg: FlushBuckets, _ctx: &mut Self::Context) -> Self::Result {
            let buckets = msg.into_buckets();
            relay_log::debug!("received buckets: {:#?}", buckets);
            if self.reject_all {
                return Err(buckets);
            }
            self.add_buckets(buckets);
            Ok(())
        }
    }

    fn test_config() -> AggregatorConfig {
        AggregatorConfig {
            bucket_interval: 1,
            initial_delay: 0,
            debounce_delay: 0,
            max_secs_in_past: 50 * 365 * 24 * 60 * 60,
            max_secs_in_future: 50 * 365 * 24 * 60 * 60,
            max_name_length: 200,
            max_tag_key_length: 200,
            max_tag_value_length: 200,
            max_project_key_bucket_bytes: None,
            max_total_bucket_bytes: None,
        }
    }

    fn some_metric() -> Metric {
        Metric {
            name: "c:foo".to_owned(),
            unit: MetricUnit::None,
            value: MetricValue::Counter(42.),
            timestamp: UnixTimestamp::from_secs(999994711),
            tags: BTreeMap::new(),
        }
    }

    #[test]
    fn test_distribution_insert() {
        let mut distribution = DistributionValue::new();
        assert_eq!(distribution.insert(2f64), 1);
        assert_eq!(distribution.insert(1f64), 1);
        assert_eq!(distribution.insert(2f64), 2);

        assert_eq!(distribution.len(), 3);

        assert!(!distribution.contains(0f64));
        assert!(distribution.contains(1f64));
        assert!(distribution.contains(2f64));

        assert_eq!(distribution.get(0f64), 0);
        assert_eq!(distribution.get(1f64), 1);
        assert_eq!(distribution.get(2f64), 2);
    }

    #[test]
    fn test_distribution_insert_multi() {
        let mut distribution = DistributionValue::new();
        assert_eq!(distribution.insert_multi(0f64, 0), 0);
        assert_eq!(distribution.insert_multi(2f64, 2), 2);
        assert_eq!(distribution.insert_multi(1f64, 1), 1);
        assert_eq!(distribution.insert_multi(3f64, 1), 1);
        assert_eq!(distribution.insert_multi(3f64, 2), 3);

        assert_eq!(distribution.len(), 6);

        assert!(!distribution.contains(0f64));
        assert!(distribution.contains(1f64));
        assert!(distribution.contains(2f64));
        assert!(distribution.contains(3f64));

        assert_eq!(distribution.get(0f64), 0);
        assert_eq!(distribution.get(1f64), 1);
        assert_eq!(distribution.get(2f64), 2);
        assert_eq!(distribution.get(3f64), 3);
    }

    #[test]
    fn test_distribution_iter_values() {
        let distribution = dist![2f64, 1f64, 2f64];

        let mut iter = distribution.iter_values();
        assert_eq!(iter.len(), 3);
        assert_eq!(iter.next(), Some(1f64));
        assert_eq!(iter.len(), 2);
        assert_eq!(iter.next(), Some(2f64));
        assert_eq!(iter.len(), 1);
        assert_eq!(iter.next(), Some(2f64));
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_distribution_iter_values_empty() {
        let distribution = DistributionValue::new();
        let mut iter = distribution.iter_values();
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_distribution_iter() {
        let distribution = dist![2f64, 1f64, 2f64];

        let mut iter = distribution.iter();
        assert_eq!(iter.next(), Some((1f64, 1)));
        assert_eq!(iter.next(), Some((2f64, 2)));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_parse_buckets() {
        let json = r#"[
          {
            "name": "endpoint.response_time",
            "unit": "millisecond",
            "value": [36, 49, 57, 68],
            "type": "d",
            "timestamp": 1615889440,
            "width": 10,
            "tags": {
                "route": "user_index"
            }
          }
        ]"#;

        let buckets = Bucket::parse_all(json.as_bytes()).unwrap();
        insta::assert_debug_snapshot!(buckets, @r###"
        [
            Bucket {
                timestamp: UnixTimestamp(1615889440),
                width: 10,
                name: "endpoint.response_time",
                unit: Duration(
                    MilliSecond,
                ),
                value: Distribution(
                    {
                        36.0: 1,
                        49.0: 1,
                        57.0: 1,
                        68.0: 1,
                    },
                ),
                tags: {
                    "route": "user_index",
                },
            },
        ]
        "###);
    }

    #[test]
    fn test_parse_bucket_defaults() {
        let json = r#"[
          {
            "name": "endpoint.hits",
            "value": 4,
            "type": "c",
            "timestamp": 1615889440,
            "width": 10
          }
        ]"#;

        let buckets = Bucket::parse_all(json.as_bytes()).unwrap();
        insta::assert_debug_snapshot!(buckets, @r###"
        [
            Bucket {
                timestamp: UnixTimestamp(1615889440),
                width: 10,
                name: "endpoint.hits",
                unit: None,
                value: Counter(
                    4.0,
                ),
                tags: {},
            },
        ]
        "###);
    }

    #[test]
    fn test_buckets_roundtrip() {
        let json = r#"[
  {
    "timestamp": 1615889440,
    "width": 10,
    "name": "endpoint.response_time",
    "type": "d",
    "value": [
      36.0,
      49.0,
      57.0,
      68.0
    ],
    "tags": {
      "route": "user_index"
    }
  },
  {
    "timestamp": 1615889440,
    "width": 10,
    "name": "endpoint.hits",
    "type": "c",
    "value": 4.0,
    "tags": {
      "route": "user_index"
    }
  },
  {
    "timestamp": 1615889440,
    "width": 10,
    "name": "endpoint.parallel_requests",
    "type": "g",
    "value": {
      "max": 42.0,
      "min": 17.0,
      "sum": 2210.0,
      "last": 25.0,
      "count": 85
    }
  },
  {
    "timestamp": 1615889440,
    "width": 10,
    "name": "endpoint.users",
    "type": "s",
    "value": [
      3182887624,
      4267882815
    ],
    "tags": {
      "route": "user_index"
    }
  }
]"#;

        let buckets = Bucket::parse_all(json.as_bytes()).unwrap();
        let serialized = serde_json::to_string_pretty(&buckets).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn test_bucket_value_merge_counter() {
        let mut value = BucketValue::Counter(42.);
        BucketValue::Counter(43.).merge_into(&mut value).unwrap();
        assert_eq!(value, BucketValue::Counter(85.));
    }

    #[test]
    fn test_bucket_value_merge_distribution() {
        let mut value = BucketValue::Distribution(dist![1., 2., 3.]);
        BucketValue::Distribution(dist![2., 4.])
            .merge_into(&mut value)
            .unwrap();
        assert_eq!(value, BucketValue::Distribution(dist![1., 2., 2., 3., 4.]));
    }

    #[test]
    fn test_bucket_value_merge_set() {
        let mut value = BucketValue::Set(vec![1, 2].into_iter().collect());
        BucketValue::Set(vec![2, 3].into_iter().collect())
            .merge_into(&mut value)
            .unwrap();
        assert_eq!(value, BucketValue::Set(vec![1, 2, 3].into_iter().collect()));
    }

    #[test]
    fn test_bucket_value_merge_gauge() {
        let mut value = BucketValue::Gauge(GaugeValue::single(42.));
        BucketValue::Gauge(GaugeValue::single(43.))
            .merge_into(&mut value)
            .unwrap();

        assert_eq!(
            value,
            BucketValue::Gauge(GaugeValue {
                max: 43.,
                min: 42.,
                sum: 85.,
                last: 43.,
                count: 2,
            })
        );
    }

    #[test]
    fn test_bucket_value_insert_counter() {
        let mut value = BucketValue::Counter(42.);
        MetricValue::Counter(43.).merge_into(&mut value).unwrap();
        assert_eq!(value, BucketValue::Counter(85.));
    }

    #[test]
    fn test_bucket_value_insert_distribution() {
        let mut value = BucketValue::Distribution(dist![1., 2., 3.]);
        MetricValue::Distribution(2.0)
            .merge_into(&mut value)
            .unwrap();
        // TODO: This should be ordered
        assert_eq!(value, BucketValue::Distribution(dist![1., 2., 3., 2.]));
    }

    #[test]
    fn test_bucket_value_insert_set() {
        let mut value = BucketValue::Set(vec![1, 2].into_iter().collect());
        MetricValue::Set(3).merge_into(&mut value).unwrap();
        assert_eq!(value, BucketValue::Set(vec![1, 2, 3].into_iter().collect()));
        MetricValue::Set(2).merge_into(&mut value).unwrap();
        assert_eq!(value, BucketValue::Set(vec![1, 2, 3].into_iter().collect()));
    }

    #[test]
    fn test_bucket_value_insert_gauge() {
        let mut value = BucketValue::Gauge(GaugeValue::single(42.));
        MetricValue::Gauge(43.).merge_into(&mut value).unwrap();
        assert_eq!(
            value,
            BucketValue::Gauge(GaugeValue {
                max: 43.,
                min: 42.,
                sum: 85.,
                last: 43.,
                count: 2,
            })
        );
    }

    #[test]
    fn test_bucket_value_cost() {
        // When this test fails, it means that the cost model has changed.
        // Check dimensionality limits.
        let expected_bucket_value_size = 48;
        let expected_set_entry_size = 4;

        let counter = BucketValue::Counter(123.0);
        assert_eq!(counter.cost(), expected_bucket_value_size);
        let set = BucketValue::Set(BTreeSet::<u32>::from([1, 2, 3, 4, 5]));
        assert_eq!(
            set.cost(),
            expected_bucket_value_size + 5 * expected_set_entry_size
        );
        let distribution = BucketValue::Distribution(dist![1., 2., 3.]);
        assert_eq!(
            distribution.cost(),
            expected_bucket_value_size + 3 * (8 + 4)
        );
        let gauge = BucketValue::Gauge(GaugeValue {
            max: 43.,
            min: 42.,
            sum: 85.,
            last: 43.,
            count: 2,
        });
        assert_eq!(gauge.cost(), expected_bucket_value_size);
    }

    #[test]
    fn test_bucket_key_cost() {
        // When this test fails, it means that the cost model has changed.
        // Check dimensionality limits.
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
        let name = "12345".to_owned();
        let bucket_key = BucketKey {
            project_key,
            timestamp: UnixTimestamp::now(),
            metric_name: name,
            metric_type: MetricType::Counter,
            metric_unit: MetricUnit::None,
            tags: BTreeMap::from([
                ("hello".to_owned(), "world".to_owned()),
                ("answer".to_owned(), "42".to_owned()),
            ]),
        };
        assert_eq!(
            bucket_key.cost(),
            112 + // BucketKey
            5 + // name
            (5 + 5 + 6 + 2) // tags
        );
    }

    #[test]
    fn test_aggregator_merge_counters() {
        relay_test::setup();
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();

        let receiver = TestReceiver::start_default().recipient();
        let mut aggregator = Aggregator::new(test_config(), receiver);

        let metric1 = some_metric();

        let mut metric2 = metric1.clone();
        metric2.value = MetricValue::Counter(43.);
        aggregator.insert(project_key, metric1).unwrap();
        aggregator.insert(project_key, metric2).unwrap();

        let buckets: Vec<_> = aggregator
            .buckets
            .iter()
            .map(|(k, e)| (k, &e.value)) // skip flush times, they are different every time
            .collect();

        insta::assert_debug_snapshot!(buckets, @r###"
        [
            (
                BucketKey {
                    project_key: ProjectKey("a94ae32be2584e0bbd7a4cbb95971fee"),
                    timestamp: UnixTimestamp(999994711),
                    metric_name: "c:foo",
                    metric_type: Counter,
                    metric_unit: None,
                    tags: {},
                },
                Counter(
                    85.0,
                ),
            ),
        ]
        "###);
    }

    #[test]
    fn test_aggregator_merge_timestamps() {
        relay_test::setup();
        let config = AggregatorConfig {
            bucket_interval: 10,
            ..test_config()
        };
        let receiver = TestReceiver::start_default().recipient();
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();

        let mut aggregator = Aggregator::new(config, receiver);

        let metric1 = some_metric();

        let mut metric2 = metric1.clone();
        metric2.timestamp = UnixTimestamp::from_secs(999994712);

        let mut metric3 = metric1.clone();
        metric3.timestamp = UnixTimestamp::from_secs(999994721);
        aggregator.insert(project_key, metric1).unwrap();
        aggregator.insert(project_key, metric2).unwrap();
        aggregator.insert(project_key, metric3).unwrap();

        let mut buckets: Vec<_> = aggregator
            .buckets
            .iter()
            .map(|(k, e)| (k, &e.value)) // skip flush times, they are different every time
            .collect();

        buckets.sort_by(|a, b| a.0.timestamp.cmp(&b.0.timestamp));
        insta::assert_debug_snapshot!(buckets, @r###"
        [
            (
                BucketKey {
                    project_key: ProjectKey("a94ae32be2584e0bbd7a4cbb95971fee"),
                    timestamp: UnixTimestamp(999994710),
                    metric_name: "c:foo",
                    metric_type: Counter,
                    metric_unit: None,
                    tags: {},
                },
                Counter(
                    84.0,
                ),
            ),
            (
                BucketKey {
                    project_key: ProjectKey("a94ae32be2584e0bbd7a4cbb95971fee"),
                    timestamp: UnixTimestamp(999994720),
                    metric_name: "c:foo",
                    metric_type: Counter,
                    metric_unit: None,
                    tags: {},
                },
                Counter(
                    42.0,
                ),
            ),
        ]
        "###);
    }

    #[test]
    fn test_aggregator_mixed_types() {
        relay_test::setup();

        let config = AggregatorConfig {
            bucket_interval: 10,
            ..test_config()
        };

        let receiver = TestReceiver::start_default().recipient();
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();

        let mut aggregator = Aggregator::new(config, receiver);

        let metric1 = some_metric();

        let mut metric2 = metric1.clone();
        metric2.value = MetricValue::Set(123);

        // It's OK to have same name for different types:
        aggregator.insert(project_key, metric1).unwrap();
        aggregator.insert(project_key, metric2).unwrap();
        assert_eq!(aggregator.buckets.len(), 2);
    }

    #[test]
    fn test_aggregator_mixed_units() {
        relay_test::setup();

        let config = AggregatorConfig {
            bucket_interval: 10,
            ..test_config()
        };

        let receiver = TestReceiver::start_default().recipient();
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();

        let mut aggregator = Aggregator::new(config, receiver);

        let metric1 = some_metric();

        let mut metric2 = metric1.clone();
        metric2.unit = MetricUnit::Duration(DurationUnit::Second);

        // It's OK to have same metric with different units:
        aggregator.insert(project_key, metric1).unwrap();
        aggregator.insert(project_key, metric2).unwrap();

        // TODO: This should convert if units are convertible
        assert_eq!(aggregator.buckets.len(), 2);
    }

    #[test]
    fn test_aggregator_mixed_projects() {
        relay_test::setup();

        let config = AggregatorConfig {
            bucket_interval: 10,
            ..test_config()
        };

        let receiver = TestReceiver::start_default().recipient();
        let project_key1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let project_key2 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();

        let mut aggregator = Aggregator::new(config, receiver);

        // It's OK to have same metric with different projects:
        aggregator.insert(project_key1, some_metric()).unwrap();
        aggregator.insert(project_key2, some_metric()).unwrap();

        assert_eq!(aggregator.buckets.len(), 2);
    }

    #[test]
    fn test_cost_tracker() {
        let project_key1 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();
        let project_key2 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
        let project_key3 = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fef").unwrap();
        let mut cost_tracker = CostTracker::default();
        insta::assert_debug_snapshot!(cost_tracker, @r###"
        CostTracker {
            total_cost: 0,
            cost_per_project_key: {},
        }
        "###);
        cost_tracker.add_cost(project_key1, 100);
        insta::assert_debug_snapshot!(cost_tracker, @r###"
        CostTracker {
            total_cost: 100,
            cost_per_project_key: {
                ProjectKey("a94ae32be2584e0bbd7a4cbb95971fed"): 100,
            },
        }
        "###);
        cost_tracker.add_cost(project_key2, 200);
        insta::assert_debug_snapshot!(cost_tracker, @r###"
        CostTracker {
            total_cost: 300,
            cost_per_project_key: {
                ProjectKey("a94ae32be2584e0bbd7a4cbb95971fed"): 100,
                ProjectKey("a94ae32be2584e0bbd7a4cbb95971fee"): 200,
            },
        }
        "###);
        // Unknown project: Will log error, but not crash
        cost_tracker.subtract_cost(project_key3, 666);
        insta::assert_debug_snapshot!(cost_tracker, @r###"
        CostTracker {
            total_cost: 300,
            cost_per_project_key: {
                ProjectKey("a94ae32be2584e0bbd7a4cbb95971fed"): 100,
                ProjectKey("a94ae32be2584e0bbd7a4cbb95971fee"): 200,
            },
        }
        "###);
        // Subtract too much: Will log error, but not crash
        cost_tracker.subtract_cost(project_key1, 666);
        insta::assert_debug_snapshot!(cost_tracker, @r###"
        CostTracker {
            total_cost: 200,
            cost_per_project_key: {
                ProjectKey("a94ae32be2584e0bbd7a4cbb95971fee"): 200,
            },
        }
        "###);
        cost_tracker.subtract_cost(project_key2, 20);
        insta::assert_debug_snapshot!(cost_tracker, @r###"
        CostTracker {
            total_cost: 180,
            cost_per_project_key: {
                ProjectKey("a94ae32be2584e0bbd7a4cbb95971fee"): 180,
            },
        }
        "###);
        cost_tracker.subtract_cost(project_key2, 180);
        insta::assert_debug_snapshot!(cost_tracker, @r###"
        CostTracker {
            total_cost: 0,
            cost_per_project_key: {},
        }
        "###);
    }

    #[test]
    fn test_aggregator_cost_tracking() {
        // Make sure that the right cost is added / subtracted
        let receiver = TestReceiver::start_default().recipient();
        let mut aggregator = Aggregator::new(test_config(), receiver);
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();

        let mut metric = Metric {
            name: "c:foo".to_owned(),
            unit: MetricUnit::None,
            value: MetricValue::Counter(42.),
            timestamp: UnixTimestamp::from_secs(999994711),
            tags: BTreeMap::new(),
        };
        let bucket_key = BucketKey {
            project_key,
            timestamp: UnixTimestamp::now(),
            metric_name: "c:foo".to_owned(),
            metric_type: MetricType::Counter,
            metric_unit: MetricUnit::None,
            tags: BTreeMap::new(),
        };
        let fixed_cost = bucket_key.cost() + mem::size_of::<BucketValue>();
        for (metric_value, expected_added_cost) in [
            (MetricValue::Counter(42.), fixed_cost),
            (MetricValue::Counter(42.), 0), // counters have constant size
            (MetricValue::Set(123), fixed_cost + 4), // Added a new bucket + 1 element
            (MetricValue::Set(123), 0),     // Same element in set, no change
            (MetricValue::Set(456), 4),     // Different element in set -> +4
            (MetricValue::Distribution(1.0), fixed_cost + 12), // New bucket + 1 element
            (MetricValue::Distribution(1.0), 0), // no new element
            (MetricValue::Distribution(2.0), 12), // 1 new element
            (MetricValue::Gauge(0.3), fixed_cost), // New bucket
            (MetricValue::Gauge(0.2), 0),   // gauge has constant size
        ] {
            metric.value = metric_value;
            let current_cost = aggregator.cost_tracker.total_cost;
            aggregator.insert(project_key, metric.clone()).unwrap();
            assert_eq!(
                aggregator.cost_tracker.total_cost,
                current_cost + expected_added_cost
            );
        }

        aggregator.pop_flush_buckets();
        assert_eq!(aggregator.cost_tracker.total_cost, 0);
    }

    #[test]
    fn test_flush_bucket() {
        relay_test::setup();
        let receiver = TestReceiver::default();
        relay_test::block_fn(|| {
            let config = AggregatorConfig {
                bucket_interval: 1,
                initial_delay: 0,
                debounce_delay: 0,
                ..Default::default()
            };
            let recipient = receiver.clone().start().recipient();
            let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
            let aggregator = Aggregator::new(config, recipient).start();

            let mut metric = some_metric();
            metric.timestamp = UnixTimestamp::now();
            aggregator
                .send(InsertMetrics {
                    project_key,
                    metrics: vec![metric],
                })
                .and_then(move |_| aggregator.send(BucketCountInquiry))
                .map_err(|_| ())
                .and_then(|bucket_count| {
                    // Immediately after sending the metric, nothing has been flushed:
                    assert_eq!(bucket_count, 1);
                    assert_eq!(receiver.bucket_count(), 0);
                    Ok(())
                })
                .and_then(|_| {
                    // Wait until flush delay has passed. It is up to 2s: 1s for the current bucket
                    // and 1s for the flush shift. Adding 100ms buffer.
                    relay_test::delay(Duration::from_millis(2100)).map_err(|_| ())
                })
                .and_then(|_| {
                    // After the flush delay has passed, the receiver should have the bucket:
                    assert_eq!(receiver.bucket_count(), 1);
                    Ok(())
                })
        })
        .ok();
    }

    #[test]
    fn test_merge_back() {
        relay_test::setup();

        // Create a receiver which accepts nothing:
        let receiver = TestReceiver {
            reject_all: true,
            ..TestReceiver::default()
        };

        relay_test::block_fn(|| {
            let config = AggregatorConfig {
                bucket_interval: 1,
                initial_delay: 0,
                debounce_delay: 0,
                ..Default::default()
            };
            let recipient = receiver.clone().start().recipient();
            let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();

            let aggregator = Aggregator::new(config, recipient).start();

            let mut metric = some_metric();
            metric.timestamp = UnixTimestamp::now();
            aggregator
                .send(InsertMetrics {
                    project_key,
                    metrics: vec![metric],
                })
                .map_err(|_| ())
                .and_then(|_| {
                    // Immediately after sending the metric, nothing has been flushed:
                    assert_eq!(receiver.bucket_count(), 0);
                    Ok(())
                })
                .map_err(|_| ())
                .and_then(|_| {
                    // Wait until flush delay has passed
                    relay_test::delay(Duration::from_millis(1100)).map_err(|_| ())
                })
                .and_then(move |_| aggregator.send(BucketCountInquiry).map_err(|_| ()))
                .and_then(|bucket_count| {
                    // After the flush delay has passed, the receiver should still not have the
                    // bucket
                    assert_eq!(bucket_count, 1);
                    assert_eq!(receiver.bucket_count(), 0);
                    Ok(())
                })
        })
        .ok();
    }

    #[test]
    fn test_get_bucket_timestamp_overflow() {
        let config = AggregatorConfig {
            bucket_interval: 10,
            initial_delay: 0,
            debounce_delay: 0,
            ..Default::default()
        };

        assert!(matches!(
            config
                .get_bucket_timestamp(UnixTimestamp::from_secs(u64::MAX), 2)
                .unwrap_err()
                .kind,
            AggregateMetricsErrorKind::InvalidTimestamp
        ));
    }

    #[test]
    fn test_get_bucket_timestamp_zero() {
        let config = AggregatorConfig {
            bucket_interval: 10,
            initial_delay: 0,
            debounce_delay: 0,
            ..Default::default()
        };

        let now = UnixTimestamp::now().as_secs();
        let rounded_now = UnixTimestamp::from_secs(now / 10 * 10);
        assert_eq!(
            config
                .get_bucket_timestamp(UnixTimestamp::from_secs(now), 0)
                .unwrap(),
            rounded_now
        );
    }

    #[test]
    fn test_get_bucket_timestamp_multiple() {
        let config = AggregatorConfig {
            bucket_interval: 10,
            initial_delay: 0,
            debounce_delay: 0,
            ..Default::default()
        };

        let rounded_now = UnixTimestamp::now().as_secs() / 10 * 10;
        let now = rounded_now + 3;
        assert_eq!(
            config
                .get_bucket_timestamp(UnixTimestamp::from_secs(now), 20)
                .unwrap()
                .as_secs(),
            rounded_now + 10
        );
    }

    #[test]
    fn test_get_bucket_timestamp_non_multiple() {
        let config = AggregatorConfig {
            bucket_interval: 10,
            initial_delay: 0,
            debounce_delay: 0,
            ..Default::default()
        };

        let rounded_now = UnixTimestamp::now().as_secs() / 10 * 10;
        let now = rounded_now + 3;
        assert_eq!(
            config
                .get_bucket_timestamp(UnixTimestamp::from_secs(now), 23)
                .unwrap()
                .as_secs(),
            rounded_now + 10
        );
    }

    #[test]
    fn test_validate_bucket_key_chars() {
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();

        let bucket_key = BucketKey {
            project_key,
            timestamp: UnixTimestamp::now(),
            metric_name: "c:hergus.bergus".to_owned(),
            metric_type: MetricType::Counter,
            metric_unit: MetricUnit::None,
            tags: {
                let mut tags = BTreeMap::new();
                // There are some SDKs which mess up content encodings, and interpret the raw bytes
                // of an UTF-16 string as UTF-8. Leading to ASCII
                // strings getting null-bytes interleaved.
                //
                // Somehow those values end up as release tag in sessions, while in error events we
                // haven't observed this malformed encoding. We believe it's slightly better to
                // strip out NUL-bytes instead of dropping the tag such that those values line up
                // again across sessions and events. Should that cause too high cardinality we'll
                // have to drop tags.
                //
                // Note that releases are validated separately against much stricter character set,
                // but the above idea should still apply to other tags.
                tags.insert(
                    "is_it_garbage".to_owned(),
                    "a\0b\0s\0o\0l\0u\0t\0e\0l\0y".to_owned(),
                );
                tags.insert("another\0garbage".to_owned(), "bye".to_owned());
                tags
            },
        };
        let aggregator_config = test_config();

        let mut bucket_key =
            Aggregator::validate_bucket_key(bucket_key, &aggregator_config).unwrap();

        assert_eq!(bucket_key.tags.len(), 1);
        assert_eq!(
            bucket_key.tags.get("is_it_garbage"),
            Some(&"absolutely".to_owned())
        );
        assert_eq!(bucket_key.tags.get("another\0garbage"), None);

        bucket_key.metric_name = "hergus\0bergus".to_owned();
        Aggregator::validate_bucket_key(bucket_key, &aggregator_config).unwrap_err();
    }

    #[test]
    fn test_validate_bucket_key_str_lens() {
        relay_test::setup();
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
        let aggregator_config = test_config();

        let short_metric = BucketKey {
            project_key,
            timestamp: UnixTimestamp::now(),
            metric_name: "c:a_short_metric".to_owned(),
            metric_type: MetricType::Counter,
            metric_unit: MetricUnit::None,
            tags: BTreeMap::new(),
        };
        assert!(Aggregator::validate_bucket_key(short_metric, &aggregator_config).is_ok());

        let long_metric = BucketKey {
            project_key,
            timestamp: UnixTimestamp::now(),
            metric_name: "c:long_name_a_very_long_name_its_super_long_really_but_like_super_long_probably_the_longest_name_youve_seen_and_even_the_longest_name_ever_its_extremly_long_i_cant_tell_how_long_it_is_because_i_dont_have_that_many_fingers_thus_i_cant_count_the_many_characters_this_long_name_is".to_owned(),
            metric_type: MetricType::Counter,
            metric_unit: MetricUnit::None,
            tags: BTreeMap::new(),
        };
        let validation = Aggregator::validate_bucket_key(long_metric, &aggregator_config);

        assert_eq!(
            validation.unwrap_err(),
            AggregateMetricsError::from(AggregateMetricsErrorKind::InvalidStringLength)
        );

        let short_metric_long_tag_key = BucketKey {
            project_key,
            timestamp: UnixTimestamp::now(),
            metric_name: "c:a_short_metric_with_long_tag_key".to_owned(),
            metric_type: MetricType::Counter,
            metric_unit: MetricUnit::None,
            tags: BTreeMap::from([("i_run_out_of_creativity_so_here_we_go_Lorem_Ipsum_is_simply_dummy_text_of_the_printing_and_typesetting_industry_Lorem_Ipsum_has_been_the_industrys_standard_dummy_text_ever_since_the_1500s_when_an_unknown_printer_took_a_galley_of_type_and_scrambled_it_to_make_a_type_specimen_book".into(), "tag_value".into())]),
        };
        let validation =
            Aggregator::validate_bucket_key(short_metric_long_tag_key, &aggregator_config).unwrap();
        assert_eq!(validation.tags.len(), 0);

        let short_metric_long_tag_value = BucketKey {
            project_key,
            timestamp: UnixTimestamp::now(),
            metric_name: "c:a_short_metric_with_long_tag_value".to_owned(),
            metric_type: MetricType::Counter,
            metric_unit: MetricUnit::None,
                tags: BTreeMap::from([("tag_key".into(), "i_run_out_of_creativity_so_here_we_go_Lorem_Ipsum_is_simply_dummy_text_of_the_printing_and_typesetting_industry_Lorem_Ipsum_has_been_the_industrys_standard_dummy_text_ever_since_the_1500s_when_an_unknown_printer_took_a_galley_of_type_and_scrambled_it_to_make_a_type_specimen_book".into())]),
        };
        let validation =
            Aggregator::validate_bucket_key(short_metric_long_tag_value, &aggregator_config)
                .unwrap();
        assert_eq!(validation.tags.len(), 0);
    }

    #[test]
    fn test_aggregator_cost_enforcement_total() {
        let config = AggregatorConfig {
            max_total_bucket_bytes: Some(1),
            ..test_config()
        };

        let metric = Metric {
            name: "c:foo".to_owned(),
            unit: MetricUnit::None,
            value: MetricValue::Counter(42.),
            timestamp: UnixTimestamp::from_secs(999994711),
            tags: BTreeMap::new(),
        };

        let receiver = TestReceiver::start_default().recipient();
        let mut aggregator = Aggregator::new(config, receiver);
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();

        aggregator.insert(project_key, metric.clone()).unwrap();
        assert_eq!(
            aggregator.insert(project_key, metric).unwrap_err().kind,
            AggregateMetricsErrorKind::TotalLimitExceeded
        );
    }

    #[test]
    fn test_aggregator_cost_enforcement_project() {
        let config = AggregatorConfig {
            max_project_key_bucket_bytes: Some(1),
            ..test_config()
        };

        let metric = Metric {
            name: "c:foo".to_owned(),
            unit: MetricUnit::None,
            value: MetricValue::Counter(42.),
            timestamp: UnixTimestamp::from_secs(999994711),
            tags: BTreeMap::new(),
        };

        let receiver = TestReceiver::start_default().recipient();
        let mut aggregator = Aggregator::new(config, receiver);
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fed").unwrap();

        aggregator.insert(project_key, metric.clone()).unwrap();
        assert_eq!(
            aggregator.insert(project_key, metric).unwrap_err().kind,
            AggregateMetricsErrorKind::ProjectLimitExceeded
        );
    }
}
