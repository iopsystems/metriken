// Copyright 2021 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

//! Methods and structs for working with dynamically created and destroyed
//! metrics.
//!
//! Generally users should not need to use anything in this module with the
//! exception of [`DynPinnedMetric`] and [`DynBoxedMetric`].

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::pin::Pin;

// We use parking_lot here since it avoids lock poisioning
use parking_lot::{const_rwlock, RwLock, RwLockReadGuard};

use crate::null::NullMetric;
use crate::provide::ProviderMap;
use crate::wrapper::FormattingFn;
use crate::{default_formatter, Format, Metadata, Metric, MetricEntry};

pub(crate) struct DynMetricsRegistry {
    metrics: BTreeMap<usize, MetricEntry>,
}

impl DynMetricsRegistry {
    const fn new() -> Self {
        Self {
            metrics: BTreeMap::new(),
        }
    }

    fn key_for(entry: &MetricEntry) -> usize {
        entry.metric() as *const dyn Metric as *const () as usize
    }

    fn register(&mut self, entry: MetricEntry) {
        self.metrics.insert(Self::key_for(&entry), entry);
    }

    fn unregister(&mut self, metric: *const dyn Metric) {
        let key = metric as *const () as usize;
        self.metrics.remove(&key);
    }

    pub(crate) fn metrics(&self) -> &BTreeMap<usize, MetricEntry> {
        &self.metrics
    }
}

static REGISTRY: RwLock<DynMetricsRegistry> = const_rwlock(DynMetricsRegistry::new());

pub(crate) fn get_registry() -> RwLockReadGuard<'static, DynMetricsRegistry> {
    REGISTRY.read()
}

/// Builder for creating a dynamic metric.
///
/// This can be used to directly create a [`DynBoxedMetric`] or you can convert
/// this builder into a [`MetricEntry`] for more advanced use cases.
pub struct MetricBuilder {
    name: Cow<'static, str>,
    desc: Option<Cow<'static, str>>,
    provider: ProviderMap,
    metadata: HashMap<String, String>,
    formatter: fn(&MetricEntry, Format) -> String,
}

impl MetricBuilder {
    /// Create a new builder, starting with the metric name.
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: name.into(),
            desc: None,
            provider: ProviderMap::new(),
            metadata: HashMap::new(),
            formatter: default_formatter,
        }
    }

    /// Add a description of this metric.
    pub fn description(mut self, desc: impl Into<Cow<'static, str>>) -> Self {
        self.desc = Some(desc.into());
        self
    }

    /// Add a new key-value metadata entry.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    pub fn formatter(self, formatter: fn(&MetricEntry, Format) -> String) -> Self {
        self.provide(FormattingFn(formatter))
    }

    /// Add provided type data to this metric.
    ///
    /// These can then be accessed via [`MetricEntry::request_ref`].
    pub fn provide<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.provider.insert(value);
        self
    }

    /// Convert this builder directly into a [`MetricEntry`].
    ///
    /// This method is generally not what you want. Use [`take_entry`] and
    /// [`build_pinned`] instead.
    ///
    /// [`take_entry`]: MetricBuilder::take_entry
    /// [`build_pinned`]: MetricBuilder::build_pinned
    pub fn into_entry(mut self) -> MetricEntry {
        self.take_entry()
    }

    /// Create a [`MetricEntry`] by taking config values out of this builder.
    pub fn take_entry(&mut self) -> MetricEntry {
        MetricEntry {
            metric: &NullMetric,
            name: std::mem::take(&mut self.name),
            description: std::mem::take(&mut self.desc),
        }
    }

    /// Build a [`DynBoxedMetric`] for use with this builder.
    pub fn build<T: Metric>(mut self, metric: T) -> DynBoxedMetric<T> {
        let entry = self.take_entry();
        let metric = self.build_pinned(metric);

        DynBoxedMetric::from_pinned(metric, entry)
    }

    /// Build a [`DynPinnedMetric`] for use with this builder.
    ///
    /// In order to register it you will likely want to call [`take_entry`] in
    /// order to extract the [`MetricEntry`] for this metric first.
    ///
    /// [`take_entry`]: MetricBuilder::take_entry
    pub fn build_pinned<T: Metric>(mut self, metric: T) -> DynPinnedMetric<T> {
        self.provider.insert(Metadata::new(self.metadata));
        self.provider.insert(FormattingFn(self.formatter));

        DynPinnedMetric::new_v2(metric, std::mem::take(&mut self.provider))
    }
}

/// Registers a new dynamic metric entry.
///
/// The [`MetricEntry`] instance will be kept until an [`unregister`] call is
/// made with a metric pointer that matches the one within the [`MetricEntry`].
/// When using this take care to note how it interacts with [`MetricEntry`]'s
/// safety guarantees.
///
/// # Safety
/// The pointer in `entry.metric` must remain valid to dereference until it is
/// removed from the registry via [`unregister`].
pub(crate) unsafe fn register(entry: MetricEntry) {
    REGISTRY.write().register(entry);
}

/// Unregisters all dynamic entries added via [`register`] that point to the
/// same address as `metric`.
///
/// This function may remove multiple entries if the same metric has been
/// registered multiple times.
pub(crate) fn unregister(metric: *const dyn Metric) {
    REGISTRY.write().unregister(metric);
}

/// A metric combined with a set of dynamic providers.
struct ProviderMetric<M> {
    metric: M,
    provider: ProviderMap,
}

impl<M: Metric> Metric for ProviderMetric<M> {
    fn is_enabled(&self) -> bool {
        self.metric.is_enabled()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.metric.as_any()
    }

    fn value(&self) -> Option<crate::Value> {
        self.metric.value()
    }

    fn provide<'a>(&'a self, request: &mut crate::Request<'a>) {
        self.provider.provide(request);
        self.metric.provide(request);
    }
}

/// A dynamic metric that stores the metric inline.
///
/// This is a dynamic metric that relies on pinning guarantees to ensure that
/// the stored metric can be safely accessed from other threads looking through
/// the global dynamic metrics registry. As it requires pinning, it is somewhat
/// unweildy to use. Most use cases can probably use [`DynBoxedMetric`] instead.
///
/// To use this, first create the `DynPinnedMetric` and then, once it is pinned,
/// call [`register`] any number of times with all of the names the metric
/// should be registered under. When the `DynPinnedMetric` instance is dropped
/// it will unregister all the metric entries added via [`register`].
///
/// # Example
/// ```
/// # use metriken::*;
/// # use std::pin::pin;
/// let my_dyn_metric = pin!(DynPinnedMetric::new(Counter::new()));
/// my_dyn_metric.as_ref().register(MetricBuilder::new("a.dynamic.counter").into_entry());
/// ```
///
/// [`register`]: crate::dynmetrics::DynPinnedMetric::register
pub struct DynPinnedMetric<M: Metric> {
    metric: ProviderMetric<M>,
}

impl<M: Metric> DynPinnedMetric<M> {
    /// Create a new `DynPinnedMetric` with the provided internal metric.
    ///
    /// This does not register the metric. To do that call [`register`].
    ///
    /// [`register`]: self::DynPinnedMetric::register
    #[deprecated = "DynPinnedMetric::new misses some metadata fields. Use MetricBuilder::build_pinned instead."]
    pub fn new(metric: M) -> Self {
        Self::new_v2(metric, ProviderMap::new())
    }

    fn new_v2(metric: M, provider: ProviderMap) -> Self {
        Self {
            metric: ProviderMetric { metric, provider },
        }
    }

    /// Register this metric in the global list of dynamic metrics with `name`.
    ///
    /// Calling this multiple times will result in the same metric being
    /// registered multiple times under potentially different names.
    pub fn register(self: Pin<&Self>, mut entry: MetricEntry) {
        entry.metric = &self.metric;

        // SAFETY:
        // To prove that this is safe we need to list out a few guarantees/requirements:
        //  - Pin ensures that the memory of this struct instance will not be reused
        //    until the drop call completes.
        //  - MetricEntry::new_unchecked requires that the metric reference outlive
        //    created the MetricEntry instance.
        //
        // Finally, register will keep the MetricEntry instance in a global list until
        // the corresponding unregister call is made.
        //
        // Taking all of these together, we can guarantee that self.metric will not be
        // dropped until this instance of DynPinnedMetric is dropped itself. At that
        // point, drop calls unregister which will drop the MetricEntry instance. This
        // ensures that the references to self.metric in REGISTRY will always be valid
        // and that this method is safe.
        unsafe { register(entry) };
    }
}

impl<M: Metric> Drop for DynPinnedMetric<M> {
    fn drop(&mut self) {
        // If this metric has not been registered then nothing will be removed.
        unregister(&self.metric);
    }
}

impl<M: Metric> Deref for DynPinnedMetric<M> {
    type Target = M;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.metric.metric
    }
}

/// A dynamic metric that stores the metric instance on the heap.
///
/// This avoids a lot of the hangup with [`DynPinnedMetric`] as it allows for
/// moving the `DynBoxedMetric` without having to worry about pinning or safety
/// issues. However, this comes at the expense of requiring a heap allocation
/// for the metric.
///
/// # Example
/// ```
/// # use metriken::*;
/// let my_gauge = MetricBuilder::new("my.dynamic.gauge").build(Gauge::new());
///
/// my_gauge.increment();
/// ```
pub struct DynBoxedMetric<M: Metric> {
    metric: Pin<Box<DynPinnedMetric<M>>>,
}

impl<M: Metric> DynBoxedMetric<M> {
    /// Create a new dynamic metric using the provided metric type with the
    /// provided `name`.
    #[deprecated = "DynBoxedMetric::new loses some metadata fields. Use MetricBuilder::build_pinned instead."]
    pub fn new(metric: M, entry: MetricEntry) -> Self {
        Self::from_pinned(DynPinnedMetric::new_v2(metric, ProviderMap::new()), entry)
    }

    fn from_pinned(metric: DynPinnedMetric<M>, entry: MetricEntry) -> Self {
        let metric = Box::pin(metric);
        let this = Self { metric };
        this.register(entry);
        this
    }

    /// Register this metric in the global list of dynamic metrics with `name`.
    ///
    /// Calling this multiple times will result in the same metric being
    /// registered multiple times under potentially different names.
    fn register(&self, entry: MetricEntry) {
        self.metric.as_ref().register(entry)
    }
}

impl<M: Metric> Deref for DynBoxedMetric<M> {
    type Target = M;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.metric
    }
}
