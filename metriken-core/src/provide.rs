//! This module is a copy of a subset of the APIs available in `core::error`.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::panic::{RefUnwindSafe, UnwindSafe};

use crate::Metric;

/// Request a value of type `T` from the given `impl Metric`.
pub fn request_value<'a, T>(metric: &'a (impl Metric + ?Sized)) -> Option<T>
where
    T: 'static,
{
    request_by_type_tag::<'a, tags::Value<T>>(metric)
}

/// Request a reference of type `T` from the given `impl Metric`.
pub fn request_ref<'a, T>(metric: &'a (impl Metric + ?Sized)) -> Option<&'a T>
where
    T: ?Sized + 'static,
{
    request_by_type_tag::<'a, tags::Ref<tags::MaybeSizedValue<T>>>(metric)
}

fn request_by_type_tag<'a, I>(metric: &'a (impl Metric + ?Sized)) -> Option<I::Reified>
where
    I: tags::Type<'a>,
{
    let mut tagged = TaggedOption::<'a, I>(None);
    metric.provide(tagged.as_request());
    tagged.0
}

/// `Request` supports generic, type-driven access to data.
///
/// This type is based on the unstable request type in [`std::error`] and it
/// works in pretty much the exact same way.
#[repr(transparent)]
pub struct Request<'a>(dyn Erased<'a>);

impl<'a> Request<'a> {
    fn new<'b>(erased: &'b mut (dyn Erased<'a> + 'a)) -> &'b mut Request<'a> {
        // SAFETY: transmuting `&mut (dyn Erased<'a> + 'a)` to `&mut
        //         Request<'a>` is safe because `Request` is
        //         `#[repr(transparent)]`.
        unsafe { &mut *(erased as *mut dyn Erased<'a> as *mut Request<'a>) }
    }

    /// Provide a value or other type with only static lifetimes.
    pub fn provide_value<T>(&mut self, value: T) -> &mut Self
    where
        T: 'static,
    {
        self.provide::<tags::Value<T>>(value)
    }

    /// Provide a value or other type with only static lifetimes computed using
    /// a closure.
    pub fn provide_value_with<T>(&mut self, fulfil: impl FnOnce() -> T) -> &mut Self
    where
        T: 'static,
    {
        self.provide_with::<tags::Value<T>>(fulfil)
    }

    /// Provide a reference. The referee type must be bounded by `'static`,
    /// but may be unsized.
    pub fn provide_ref<T: ?Sized + 'static>(&mut self, value: &'a T) -> &mut Self {
        self.provide::<tags::Ref<tags::MaybeSizedValue<T>>>(value)
    }

    /// Provide a reference computed using a closure. The referee type
    /// must be bounded by `'static`, but may be unsized.
    pub fn provide_ref_with<T: ?Sized + 'static>(
        &mut self,
        fulfil: impl FnOnce() -> &'a T,
    ) -> &mut Self {
        self.provide_with::<tags::Ref<tags::MaybeSizedValue<T>>>(fulfil)
    }

    /// Provide a value for the given `Type` tag.
    fn provide<I>(&mut self, value: I::Reified) -> &mut Self
    where
        I: tags::Type<'a>,
    {
        self.provide_with::<I>(move || value)
    }

    /// Provide a value with the given `Type` tag, using a closure to prevent
    /// unnecessary work.
    fn provide_with<I>(&mut self, fulfil: impl FnOnce() -> I::Reified) -> &mut Self
    where
        I: tags::Type<'a>,
    {
        if let Some(res @ TaggedOption(None)) = self.0.downcast_mut::<I>() {
            res.0 = Some(fulfil());
        }

        self
    }

    /// Check if the `Request` would be satisfied if provided with a
    /// value of the specified type. If the type does not match or has
    /// already been provided, returns false.
    pub fn would_be_satisfied_by_value_of<T>(&self) -> bool
    where
        T: 'static,
    {
        self.would_be_satisfied_by::<tags::Value<T>>()
    }

    /// Check if the `Request` would be satisfied if provided with a
    /// reference to a value of the specified type. If the type does
    /// not match or has already been provided, returns false.
    pub fn would_be_satisfied_by_ref_of<T>(&self) -> bool
    where
        T: ?Sized + 'static,
    {
        self.would_be_satisfied_by::<tags::Ref<tags::MaybeSizedValue<T>>>()
    }

    fn would_be_satisfied_by<I>(&self) -> bool
    where
        I: tags::Type<'a>,
    {
        matches!(self.0.downcast::<I>(), Some(TaggedOption(None)))
    }
}

mod tags {
    //! Type tags are used to identify a type using a separate value. This
    //! module includes type tags for some very common types.

    use std::marker::PhantomData;

    /// This trait is implemented by specific tag types in order to allow
    /// describing a type which can be requested for a given lifetime `'a`.
    pub(crate) trait Type<'a>: Sized + 'static {
        type Reified: 'a;
    }

    /// Similar to the [`Type`] trait, but for a type which may be unsized.
    pub(crate) trait MaybeSizedType<'a>: Sized + 'static {
        type Reified: ?Sized + 'a;
    }

    impl<'a, T: Type<'a>> MaybeSizedType<'a> for T {
        type Reified = T;
    }

    /// Type-based tag for types bounded by `'static`.
    #[derive(Debug)]
    pub(crate) struct Value<T: 'static>(PhantomData<T>);

    impl<'a, T: 'static> Type<'a> for Value<T> {
        type Reified = T;
    }

    /// Type-based tag similar to [`Value`] but which may be unsized (i.e., has
    /// a `?Sized` bound).
    #[derive(Debug)]
    pub(crate) struct MaybeSizedValue<T: ?Sized + 'static>(PhantomData<T>);

    impl<'a, T: ?Sized + 'static> MaybeSizedType<'a> for MaybeSizedValue<T> {
        type Reified = T;
    }

    pub(crate) struct Ref<I>(PhantomData<I>);

    impl<'a, I: MaybeSizedType<'a>> Type<'a> for Ref<I> {
        type Reified = &'a I::Reified;
    }
}

/// An `Option` with a type tag `I`.
///
/// Since this struct implements `Erased`, the type can be erased to make a
/// dynamically typed option. The type can be checked dynamically using
/// `Erased::tag_id` and since this is statically checked for the concrete type,
/// there is some degree of type safety.
#[repr(transparent)]
pub(crate) struct TaggedOption<'a, I: tags::Type<'a>>(pub Option<I::Reified>);

impl<'a, I: tags::Type<'a>> TaggedOption<'a, I> {
    pub(crate) fn as_request(&mut self) -> &mut Request<'a> {
        Request::new(self as &mut (dyn Erased<'a> + 'a))
    }
}

/// Represents a type-erased but identifiable object.
///
/// This trait is exclusively implemented by the `TaggedOption` type.
///
/// # Safety
/// The `TypeId` returned by `tag_id` _must_ be the type id of the tagged type.
unsafe trait Erased<'a>: 'a {
    /// The `TypeId` of the erased type.
    fn tag_id(&self) -> TypeId;
}

unsafe impl<'a, I: tags::Type<'a>> Erased<'a> for TaggedOption<'a, I> {
    fn tag_id(&self) -> TypeId {
        TypeId::of::<I>()
    }
}

impl<'a> dyn Erased<'a> + 'a {
    /// Returns a reference to the dynamic value if it is tagged with `I` or
    /// `None` otherwise.
    fn downcast<I>(&self) -> Option<&TaggedOption<'a, I>>
    where
        I: tags::Type<'a>,
    {
        if self.tag_id() == TypeId::of::<I>() {
            // SAFETY: We just checked whether we're pointing to an I
            Some(unsafe { &*(self as *const Self).cast::<TaggedOption<'a, I>>() })
        } else {
            None
        }
    }

    /// Returns a mutable reference to the dynamic value if it is tagged with
    /// `I` or `None` otherwise.
    fn downcast_mut<I>(&mut self) -> Option<&mut TaggedOption<'a, I>>
    where
        I: tags::Type<'a>,
    {
        if self.tag_id() == TypeId::of::<I>() {
            // SAFETY: Just checked whether we're pointing to an I.
            Some(unsafe { &mut *(self as *mut Self).cast::<TaggedOption<'a, I>>() })
        } else {
            None
        }
    }
}

/// A dynamic map for types which can be provided.
///
/// This is used by the `dynmetrics` module.
#[derive(Default)]
pub(crate) struct ProviderMap(HashMap<TypeId, Box<dyn Provide>>);

impl ProviderMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Provide>(&mut self, value: T) {
        self.0.insert(Self::typeid_for::<T>(), Box::new(value));
    }

    pub fn provide<'a>(&'a self, request: &mut Request<'a>) {
        if let Some(element) = self.0.get(&request.0.tag_id()) {
            element.provide(request);
        }
    }

    fn typeid_for<T: Provide>() -> TypeId {
        TypeId::of::<tags::Ref<T>>()
    }
}

// Needed so that DynBoxedMetric and DynPinnedMetric both implement these.
impl UnwindSafe for ProviderMap {}
impl RefUnwindSafe for ProviderMap {}

pub(crate) trait Provide: Any + Send + Sync + 'static {
    fn provide<'a>(&'a self, request: &mut Request<'a>);
}

impl<T: Any + Send + Sync> Provide for T {
    fn provide<'a>(&'a self, request: &mut Request<'a>) {
        request.provide_ref(self);
    }
}
