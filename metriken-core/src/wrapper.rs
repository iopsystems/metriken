use std::marker::PhantomData;

use crate::{Format, Metric, MetricEntry, Request};

/// A helper trait for use with [`MetricWrapper`] that allows for injecting
/// additional provided values.
///
/// This is used by the [`declare_metric_v1`][0] macro to provide certain
/// built-in types for the metric.
///
/// [0]: crate::declare_metric_v1
pub trait InjectedProvider: Send + Sync + 'static {
    fn provide(request: &mut Request<'_>);
}

/// A wrapper around a metric that injects a few more values into the request
/// via the [`InjectedProvider`] `P`.
///
/// This allows us to use the provider for some types that are internal to this
/// crate.
#[repr(transparent)]
pub struct MetricWrapper<M, P> {
    metric: M,
    provider: PhantomData<P>,
}

impl<M, P> MetricWrapper<M, P> {
    pub const fn new(metric: M) -> Self {
        Self {
            metric,
            provider: PhantomData,
        }
    }

    pub const fn from_ref(metric: &M) -> &Self {
        // SAFETY: We are #[repr(transparent)] so this is safe.
        unsafe { &*(metric as *const M as *const Self) }
    }
}

impl<M, P> Metric for MetricWrapper<M, P>
where
    M: Metric,
    P: InjectedProvider,
{
    fn is_enabled(&self) -> bool {
        self.metric.is_enabled()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.metric.as_any()
    }

    fn value(&self) -> Option<crate::Value> {
        self.metric.value()
    }

    fn provide<'a>(&'a self, request: &mut Request<'a>) {
        <P as InjectedProvider>::provide(request);
        self.metric.provide(request)
    }
}

/// Metric provider type that wraps around a formatting function.
pub struct FormattingFn(pub fn(&MetricEntry, Format) -> String);
