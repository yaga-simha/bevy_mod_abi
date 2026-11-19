//! Typed tuple queries without magic column indices.
//!
//! Sealed over tuples of arity 1 through 16 (as well as single-term queries) covering `&T`,
//! `&mut T`, `Option<&T>`, `Option<&mut T>`, [`With<T>`], [`Without<T>`], [`Changed<T>`],
//! [`Added<T>`], [`Allow<T>`], [`Has<T>`], [`Or<(T0, T1, ...)>`] (2..=8 filter terms, with
//! [`Or2`] and [`Or3`] type aliases), and [`FfiEntity`] where `T: GuestComponent`.

use core::marker::PhantomData;

use bevy_abi_mod_guest_sys::{ChunkColumnDispenser, Host};
use bevy_abi_mod_sys::{
    AccessMode, BamComponent, BamTermDesc, FfiEntity, FfiResult, MAX_TERMS_PER_QUERY,
};

use crate::{
    app::IntoSystemResult,
    component::{GuestComponent, guest_uuid_filter, resolve_guest_component},
    iter::ChunkIter,
};

mod private {
    pub trait TermSealed {}
    pub trait TupleSealed {}
    pub trait OrFilterSealed {}
    pub trait OrFilterTupleSealed {}
}

/// Archetype inclusion filter marker (`AccessMode::FILTER_ONLY`) for [`QueryTuple`].
pub struct With<T>(PhantomData<fn() -> T>);

/// Archetype exclusion filter marker (`AccessMode::WITHOUT`) for [`QueryTuple`].
pub struct Without<T>(PhantomData<fn() -> T>);

/// Row-level change-tick filter with read-only slice access (`AccessMode::CHANGED`) for [`QueryTuple`].
pub struct Changed<T>(PhantomData<fn() -> T>);

/// Row-level added-tick filter with read-only slice access (`AccessMode::ADDED`) for [`QueryTuple`].
pub struct Added<T>(PhantomData<fn() -> T>);

/// Archetype filter bypass marker (`AccessMode::ALLOW`) suppressing `DefaultQueryFilters`
/// (such as `Disabled`) for `T` without requiring `T` to be present on matched entities.
pub struct Allow<T>(PhantomData<fn() -> T>);

/// Archetypal presence indicator (`AccessMode::HAS`) yielding `bool` for each matched chunk
/// without borrowing `T`'s component storage (zero scheduler conflict with `&mut T`).
pub struct Has<T>(PhantomData<fn() -> T>);

/// Disjunctive `Or<(T0, T1, ...)>` query filter (`or_group != 0`) over 2..=8 [`OrFilterTerm`]
/// items for [`QueryTuple`].
///
/// Non-filter data terms (such as `&T` or `&mut T`) and 1-element tuples are rejected at compile
/// time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::iter::{Or, QueryTuple};
/// use bevy_abi_mod_guest::FfiChildOf;
///
/// fn assert_tuple<Q: QueryTuple>() {}
/// assert_tuple::<Or<(&FfiChildOf, &FfiChildOf)>>();
/// ```
///
/// ```compile_fail
/// use bevy_abi_mod_guest::iter::{Or, QueryTuple, With};
/// use bevy_abi_mod_guest::FfiChildOf;
///
/// fn assert_tuple<Q: QueryTuple>() {}
/// assert_tuple::<Or<(With<FfiChildOf>,)>>();
/// ```
pub struct Or<T>(PhantomData<fn() -> T>);

/// Disjunctive 2-branch `Or<(A, B)>` query filter (`or_group != 0`) for [`QueryTuple`].
///
/// Non-filter data terms (such as `&T` or `&mut T`) are rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::iter::{Or2, QueryTuple};
/// use bevy_abi_mod_guest::FfiChildOf;
///
/// fn assert_tuple<Q: QueryTuple>() {}
/// assert_tuple::<Or2<&FfiChildOf, &FfiChildOf>>();
/// ```
pub type Or2<A, B> = Or<(A, B)>;

/// Disjunctive 3-branch `Or<(A, B, C)>` query filter (`or_group != 0`) for [`QueryTuple`].
pub type Or3<A, B, C> = Or<(A, B, C)>;

/// Sealed trait implemented by filter terms permitted inside [`Or`] ([`Or2`] / [`Or3`]):
/// [`With<T>`], [`Without<T>`], [`Changed<T>`], and [`Added<T>`].
pub trait OrFilterTerm: private::OrFilterSealed {
    /// Resolves and appends the filter term with `or_group` assigned.
    fn push_filter_desc(
        host: &Host,
        manifest_name: &str,
        or_group: u8,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult>;

    /// Validates this filter term's descriptor in the active chunk without borrowing data slices.
    fn fetch_filter<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<(), FfiResult>;
}

/// Sealed trait implemented by 2..=8 tuples of [`OrFilterTerm`] items accepted by [`Or`].
pub trait OrFilterTuple: private::OrFilterTupleSealed {
    /// Number of disjunctive branches in this `Or<(...)>` group (`2..=8`).
    const BRANCH_COUNT: usize;

    /// Resolves and appends every branch in this tuple with the shared `or_group` ID.
    fn push_group_descs(
        host: &Host,
        manifest_name: &str,
        or_group: u8,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult>;

    /// Validates every branch descriptor in the active chunk without borrowing data slices.
    fn fetch_group_filters<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<(), FfiResult>;
}

#[inline]
fn push_resolved_filter_term<T: GuestComponent>(
    host: &Host,
    manifest_name: &str,
    mode: AccessMode,
    or_group: u8,
    out: &mut Vec<BamTermDesc>,
) -> Result<(), FfiResult> {
    if out.len() >= MAX_TERMS_PER_QUERY {
        return Err(FfiResult::CAPACITY_EXCEEDED);
    }
    let comp = resolve_guest_component::<T>(host, manifest_name)?;
    out.push(BamTermDesc::new(comp, mode).with_or_group(or_group));
    Ok(())
}

#[inline]
fn next_or_group_id(out: &[BamTermDesc]) -> Result<u8, FfiResult> {
    out.iter()
        .map(|t| t.or_group)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(FfiResult::CAPACITY_EXCEEDED)
}

#[inline]
fn push_resolved_term<T: GuestComponent>(
    host: &Host,
    manifest_name: &str,
    mode: AccessMode,
    out: &mut Vec<BamTermDesc>,
) -> Result<(), FfiResult> {
    if out.len() >= MAX_TERMS_PER_QUERY {
        return Err(FfiResult::CAPACITY_EXCEEDED);
    }
    let comp = resolve_guest_component::<T>(host, manifest_name)?;
    out.push(BamTermDesc::new(comp, mode));
    Ok(())
}

/// Sealed trait implemented by individual typed query terms (`&T`, `&mut T`, `Option<&T>`,
/// `Option<&mut T>`, [`With<T>`], [`Without<T>`], [`Changed<T>`], [`Added<T>`], [`Allow<T>`],
/// [`Has<T>`], [`Or<(T0, T1, ...)>`], and [`FfiEntity`]).
pub trait QueryTerm: private::TermSealed {
    /// Zero-copy slice or unit marker yielded for each chunk of this term.
    type Slice<'chunk>;

    /// `true` if this term never requests mutable column access (`WRITE` / `OPTIONAL_WRITE`).
    const IS_READ_ONLY: bool = true;

    /// Resolves and appends the corresponding [`BamTermDesc`] (if any) during system registration.
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult>;

    /// Validates and dispenses this term's slice from the active chunk.
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult>;
}

impl private::TermSealed for FfiEntity {}
impl QueryTerm for FfiEntity {
    type Slice<'chunk> = &'chunk [FfiEntity];

    #[inline]
    fn push_desc(
        _host: &Host,
        _manifest_name: &str,
        _out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        Ok(())
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        dispenser.entities()
    }
}

impl<T: GuestComponent> private::TermSealed for &T {}
impl<T: GuestComponent> QueryTerm for &T {
    type Slice<'chunk> = &'chunk [T];

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::READ, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_read::<T>(AccessMode::READ, Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for &mut T {}
impl<T: GuestComponent> QueryTerm for &mut T {
    type Slice<'chunk> = &'chunk mut [T];
    const IS_READ_ONLY: bool = false;

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::WRITE, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_write::<T>(Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for Option<&T> {}
impl<T: GuestComponent> QueryTerm for Option<&T> {
    type Slice<'chunk> = Option<&'chunk [T]>;

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::OPTIONAL_READ, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_optional_read::<T>(Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for Option<&mut T> {}
impl<T: GuestComponent> QueryTerm for Option<&mut T> {
    type Slice<'chunk> = Option<&'chunk mut [T]>;
    const IS_READ_ONLY: bool = false;

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::OPTIONAL_WRITE, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_optional_write::<T>(Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for With<T> {}
impl<T: GuestComponent> QueryTerm for With<T> {
    type Slice<'chunk> = ();

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::FILTER_ONLY, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_filter::<T>(AccessMode::FILTER_ONLY, Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for Without<T> {}
impl<T: GuestComponent> QueryTerm for Without<T> {
    type Slice<'chunk> = ();

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::WITHOUT, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_filter::<T>(AccessMode::WITHOUT, Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for Changed<T> {}
impl<T: GuestComponent> QueryTerm for Changed<T> {
    type Slice<'chunk> = &'chunk [T];

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::CHANGED, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_read::<T>(AccessMode::CHANGED, Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for Added<T> {}
impl<T: GuestComponent> QueryTerm for Added<T> {
    type Slice<'chunk> = &'chunk [T];

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::ADDED, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_read::<T>(AccessMode::ADDED, Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for Allow<T> {}
impl<T: GuestComponent> QueryTerm for Allow<T> {
    type Slice<'chunk> = ();

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::ALLOW, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_filter::<T>(AccessMode::ALLOW, Some(filter))
    }
}

impl<T: GuestComponent> private::TermSealed for Has<T> {}
impl<T: GuestComponent> QueryTerm for Has<T> {
    type Slice<'chunk> = bool;

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        push_resolved_term::<T>(host, manifest_name, AccessMode::HAS, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
        dispenser.next_has::<T>(Some(filter))
    }
}

macro_rules! impl_or_filter_term {
    ($target:ty, $mode:expr) => {
        impl<T: GuestComponent> private::OrFilterSealed for $target {}
        impl<T: GuestComponent> OrFilterTerm for $target {
            #[inline]
            fn push_filter_desc(
                host: &Host,
                manifest_name: &str,
                or_group: u8,
                out: &mut Vec<BamTermDesc>,
            ) -> Result<(), FfiResult> {
                push_resolved_filter_term::<T>(host, manifest_name, $mode, or_group, out)
            }

            #[inline]
            fn fetch_filter<'chunk>(
                dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
            ) -> Result<(), FfiResult> {
                let filter = guest_uuid_filter::<T>(dispenser.host().manifest_name());
                dispenser.next_filter::<T>($mode, Some(filter))
            }
        }
    };
}

impl_or_filter_term!(With<T>, AccessMode::FILTER_ONLY);
impl_or_filter_term!(Without<T>, AccessMode::WITHOUT);
impl_or_filter_term!(Changed<T>, AccessMode::CHANGED);
impl_or_filter_term!(Added<T>, AccessMode::ADDED);

macro_rules! impl_or_filter_tuple {
    ($count:expr => $($term:ident),+ $(,)?) => {
        impl<$($term: OrFilterTerm),+> private::OrFilterTupleSealed for ($($term,)+) {}

        impl<$($term: OrFilterTerm),+> OrFilterTuple for ($($term,)+) {
            const BRANCH_COUNT: usize = $count;

            #[inline]
            fn push_group_descs(
                host: &Host,
                manifest_name: &str,
                or_group: u8,
                out: &mut Vec<BamTermDesc>,
            ) -> Result<(), FfiResult> {
                $(
                    $term::push_filter_desc(host, manifest_name, or_group, out)?;
                )+
                Ok(())
            }

            #[inline]
            fn fetch_group_filters<'chunk>(
                dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
            ) -> Result<(), FfiResult> {
                $(
                    $term::fetch_filter(dispenser)?;
                )+
                Ok(())
            }
        }
    };
}

impl_or_filter_tuple!(2 => T0, T1);
impl_or_filter_tuple!(3 => T0, T1, T2);
impl_or_filter_tuple!(4 => T0, T1, T2, T3);
impl_or_filter_tuple!(5 => T0, T1, T2, T3, T4);
impl_or_filter_tuple!(6 => T0, T1, T2, T3, T4, T5);
impl_or_filter_tuple!(7 => T0, T1, T2, T3, T4, T5, T6);
impl_or_filter_tuple!(8 => T0, T1, T2, T3, T4, T5, T6, T7);

impl<T: OrFilterTuple> private::TermSealed for Or<T> {}
impl<T: OrFilterTuple> QueryTerm for Or<T> {
    type Slice<'chunk> = ();

    #[inline]
    fn push_desc(
        host: &Host,
        manifest_name: &str,
        out: &mut Vec<BamTermDesc>,
    ) -> Result<(), FfiResult> {
        let group_id = next_or_group_id(out)?;
        T::push_group_descs(host, manifest_name, group_id, out)
    }

    #[inline]
    fn fetch_slice<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slice<'chunk>, FfiResult> {
        T::fetch_group_filters(dispenser)
    }
}

/// Sealed tuple trait over 1..=16 [`QueryTerm`] items (plus single-item shorthand).
pub trait QueryTuple: private::TupleSealed {
    /// Tuple (or single item) of chunk slices yielded per chunk.
    type Slices<'chunk>;

    /// `true` if no term in this tuple requests mutable column access (`WRITE` / `OPTIONAL_WRITE`).
    const IS_READ_ONLY: bool;

    /// Collects the ordered [`BamTermDesc`] list for this query tuple.
    fn collect_descs(host: &Host, manifest_name: &str) -> Result<Vec<BamTermDesc>, FfiResult>;

    /// Dispenses all slices in declaration order from `dispenser`, verifying disjointness and UUIDs.
    fn fetch_slices<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slices<'chunk>, FfiResult>;
}

macro_rules! impl_single_query_tuple {
    ($($lifetime:lifetime,)? $ty_param:ident : $bound:ident => $target:ty) => {
        impl<$($lifetime,)? $ty_param: $bound> private::TupleSealed for $target {}
        impl<$($lifetime,)? $ty_param: $bound> QueryTuple for $target {
            type Slices<'chunk> = <Self as QueryTerm>::Slice<'chunk>;
            const IS_READ_ONLY: bool = <Self as QueryTerm>::IS_READ_ONLY;

            #[inline]
            fn collect_descs(
                host: &Host,
                manifest_name: &str,
            ) -> Result<Vec<BamTermDesc>, FfiResult> {
                let mut out = Vec::with_capacity(1);
                <Self as QueryTerm>::push_desc(host, manifest_name, &mut out)?;
                Ok(out)
            }

            #[inline]
            fn fetch_slices<'chunk>(
                dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
            ) -> Result<Self::Slices<'chunk>, FfiResult> {
                <Self as QueryTerm>::fetch_slice(dispenser)
            }
        }
    };
}

impl private::TupleSealed for FfiEntity {}
impl QueryTuple for FfiEntity {
    type Slices<'chunk> = &'chunk [FfiEntity];
    const IS_READ_ONLY: bool = true;

    #[inline]
    fn collect_descs(_host: &Host, _manifest_name: &str) -> Result<Vec<BamTermDesc>, FfiResult> {
        Ok(Vec::new())
    }

    #[inline]
    fn fetch_slices<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slices<'chunk>, FfiResult> {
        dispenser.entities()
    }
}

impl_single_query_tuple!('q, T: GuestComponent => &'q T);
impl_single_query_tuple!('q, T: GuestComponent => &'q mut T);
impl_single_query_tuple!('q, T: GuestComponent => Option<&'q T>);
impl_single_query_tuple!('q, T: GuestComponent => Option<&'q mut T>);
impl_single_query_tuple!(T: GuestComponent => With<T>);
impl_single_query_tuple!(T: GuestComponent => Without<T>);
impl_single_query_tuple!(T: GuestComponent => Changed<T>);
impl_single_query_tuple!(T: GuestComponent => Added<T>);
impl_single_query_tuple!(T: GuestComponent => Allow<T>);
impl_single_query_tuple!(T: GuestComponent => Has<T>);

impl<T: OrFilterTuple> private::TupleSealed for Or<T> {}
impl<T: OrFilterTuple> QueryTuple for Or<T> {
    type Slices<'chunk> = ();
    const IS_READ_ONLY: bool = true;

    #[inline]
    fn collect_descs(host: &Host, manifest_name: &str) -> Result<Vec<BamTermDesc>, FfiResult> {
        let mut out = Vec::with_capacity(T::BRANCH_COUNT);
        <Self as QueryTerm>::push_desc(host, manifest_name, &mut out)?;
        Ok(out)
    }

    #[inline]
    fn fetch_slices<'chunk>(
        dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
    ) -> Result<Self::Slices<'chunk>, FfiResult> {
        <Self as QueryTerm>::fetch_slice(dispenser)
    }
}

macro_rules! impl_query_tuple {
    ($cap:expr => $($term:ident),+ $(,)?) => {
        impl<$($term: QueryTerm),+> private::TupleSealed for ($($term,)+) {}

        impl<$($term: QueryTerm),+> QueryTuple for ($($term,)+) {
            type Slices<'chunk> = ($($term::Slice<'chunk>,)+);
            const IS_READ_ONLY: bool = true $(&& $term::IS_READ_ONLY)+;

            #[inline]
            fn collect_descs(
                host: &Host,
                manifest_name: &str,
            ) -> Result<Vec<BamTermDesc>, FfiResult> {
                let mut out = Vec::with_capacity($cap);
                $(
                    $term::push_desc(host, manifest_name, &mut out)?;
                )+
                Ok(out)
            }

            #[inline]
            fn fetch_slices<'chunk>(
                dispenser: &mut ChunkColumnDispenser<'chunk, '_>,
            ) -> Result<Self::Slices<'chunk>, FfiResult> {
                Ok(($(
                    $term::fetch_slice(dispenser)?,
                )+))
            }
        }
    };
}

impl_query_tuple!(1 => T0);
impl_query_tuple!(2 => T0, T1);
impl_query_tuple!(3 => T0, T1, T2);
impl_query_tuple!(4 => T0, T1, T2, T3);
impl_query_tuple!(5 => T0, T1, T2, T3, T4);
impl_query_tuple!(6 => T0, T1, T2, T3, T4, T5);
impl_query_tuple!(7 => T0, T1, T2, T3, T4, T5, T6);
impl_query_tuple!(8 => T0, T1, T2, T3, T4, T5, T6, T7);
impl_query_tuple!(9 => T0, T1, T2, T3, T4, T5, T6, T7, T8);
impl_query_tuple!(10 => T0, T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_query_tuple!(11 => T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_query_tuple!(12 => T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_query_tuple!(13 => T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
impl_query_tuple!(14 => T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
impl_query_tuple!(15 => T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14);
impl_query_tuple!(16 => T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15);

/// Typed chunk iterator over a declared [`QueryTuple`] `Q`, dispensing borrow-checked,
/// UUID-verified column slices without magic column index literals.
pub struct TypedChunkIter<'iter, 'ctx, Q: QueryTuple> {
    pub(crate) iter: &'iter mut ChunkIter<'ctx>,
    pub(crate) _marker: PhantomData<fn() -> Q>,
}

impl<'iter, 'ctx, Q: QueryTuple> TypedChunkIter<'iter, 'ctx, Q> {
    /// Advances to the next matching chunk and returns `Some(Ok(slices))`, `Some(Err(status))` on
    /// layout/UUID/access mismatch, or `None` when exhausted.
    #[inline]
    pub fn try_next_chunk(&mut self) -> Option<Result<Q::Slices<'_>, FfiResult>> {
        self.iter.inner.next_chunk()?;
        let mut dispenser = ChunkColumnDispenser::new(&mut self.iter.inner);
        Some(Q::fetch_slices(&mut dispenser))
    }

    /// Advances to the next matching chunk and yields typed `Q::Slices<'_>`, returning `None`
    /// when exhausted or if `Q` does not match the registered query signature.
    #[inline]
    pub fn next_chunk(&mut self) -> Option<Q::Slices<'_>> {
        self.try_next_chunk()?.ok()
    }

    /// Resets the underlying iterator back to its initial chunk for nested loops.
    #[inline]
    pub fn reset(&mut self) -> FfiResult {
        self.iter.reset()
    }

    /// Non-destructively counts all matching entities in this query via `BamHostApi::iter_count`
    /// (slot 57) without advancing or resetting the active chunk cursor.
    #[inline]
    pub fn count(&mut self) -> Result<usize, FfiResult> {
        self.iter.count()
    }

    /// Walks an ancestor relationship chain upward along `Rel: GuestComponent` starting from
    /// `entity` via `BamHostApi::iter_up_cascade` (slot 58) until locating an ancestor carrying
    /// read-only component `T: GuestComponent` at `term_index`, validating `T`'s layout and UUID.
    ///
    /// Returns `Ok(Some((owner, value)))` if an ancestor carries `T`, `Ok(None)` if no ancestor
    /// carries `T`, or `Err(status)` on resolution, permission, layout, cycle, or unsupported
    /// errors.
    #[inline]
    pub fn up_cascade<Rel: GuestComponent, T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        term_index: usize,
    ) -> Result<Option<(FfiEntity, &T)>, FfiResult> {
        let host = self.iter.inner.host();
        let rel = resolve_guest_component::<Rel>(host, host.manifest_name())?;
        self.iter.up_cascade::<T>(rel, entity, term_index)
    }

    /// Walks an ancestor relationship chain upward along pre-resolved relationship handle
    /// `relation` starting from `entity` via `BamHostApi::iter_up_cascade` (slot 58) until
    /// locating an ancestor carrying read-only component `T: GuestComponent` at `term_index`.
    #[inline]
    pub fn up_cascade_handle<T: GuestComponent>(
        &mut self,
        relation: BamComponent,
        entity: FfiEntity,
        term_index: usize,
    ) -> Result<Option<(FfiEntity, &T)>, FfiResult> {
        self.iter.up_cascade::<T>(relation, entity, term_index)
    }

    /// Iterates sequentially over all chunks, propagating any `TYPE_MISMATCH` / `ACCESS_CONFLICT`
    /// or callback error.
    pub fn for_each<F, R>(&mut self, mut f: F) -> FfiResult
    where
        F: FnMut(Q::Slices<'_>) -> R,
        R: IntoSystemResult,
    {
        while let Some(chunk_res) = self.try_next_chunk() {
            let slices = match chunk_res {
                Ok(s) => s,
                Err(err) => return err,
            };
            let status = f(slices).into_ffi_result();
            if !status.is_ok() {
                return status;
            }
        }
        FfiResult::OK
    }

    /// Iterates in parallel over disjoint chunk partitions on the host `ComputeTaskPool`, yielding
    /// typed `Q::Slices<'_>` to `f`.
    pub fn par_for_each<F, R>(&mut self, f: F) -> FfiResult
    where
        F: Fn(Q::Slices<'_>) -> R + Sync,
        R: IntoSystemResult,
    {
        self.iter.inner.par_for_each_chunk(|task_iter| {
            let mut dispenser = ChunkColumnDispenser::new(task_iter);
            let slices = match Q::fetch_slices(&mut dispenser) {
                Ok(s) => s,
                Err(err) => return err,
            };
            f(slices).into_ffi_result()
        })
    }

    /// Iterates in parallel over disjoint chunk partitions on the host `ComputeTaskPool` in
    /// strictly read-only mode via `BamHostApi::par_for_each_chunk_readonly` (slot 59), yielding
    /// typed `Q::Slices<'_>` to `f`.
    ///
    /// Rejects any [`QueryTuple`] declaring writable terms (`&mut T` or `Option<&mut T>`) with
    /// [`FfiResult::PERMISSION_DENIED`] before crossing FFI, and the host also rejects queries
    /// registered with writable terms.
    pub fn par_for_each_chunk_readonly<F, R>(&mut self, f: F) -> FfiResult
    where
        F: Fn(Q::Slices<'_>) -> R + Sync,
        R: IntoSystemResult,
    {
        if !Q::IS_READ_ONLY {
            return FfiResult::PERMISSION_DENIED;
        }
        self.iter.inner.par_for_each_chunk_readonly(|task_iter| {
            let mut dispenser = ChunkColumnDispenser::new(task_iter);
            let slices = match Q::fetch_slices(&mut dispenser) {
                Ok(s) => s,
                Err(err) => return err,
            };
            f(slices).into_ffi_result()
        })
    }
}
