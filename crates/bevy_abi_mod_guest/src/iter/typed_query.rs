//! Typed tuple queries without magic column indices (`G-03`).
//!
//! Sealed over tuples of arity 1 through 8 (as well as single-term queries) covering `&T`,
//! `&mut T`, `Option<&T>`, `Option<&mut T>`, [`With<T>`], [`Without<T>`], [`Changed<T>`],
//! [`Added<T>`], and [`FfiEntity`] where `T: GuestComponent`.

use core::marker::PhantomData;

use bevy_abi_mod_guest_sys::{ChunkColumnDispenser, Host};
use bevy_abi_mod_sys::{AccessMode, BamTermDesc, FfiEntity, FfiResult, MAX_TERMS_PER_QUERY};

use crate::{
    app::IntoSystemResult,
    component::{GuestComponent, guest_uuid_filter, resolve_guest_component},
    iter::ChunkIter,
};

mod private {
    pub trait TermSealed {}
    pub trait TupleSealed {}
}

/// Archetype inclusion filter marker (`AccessMode::FILTER_ONLY`) for [`QueryTuple`] (`G-03`).
pub struct With<T>(PhantomData<fn() -> T>);

/// Archetype exclusion filter marker (`AccessMode::WITHOUT`) for [`QueryTuple`] (`G-03`).
pub struct Without<T>(PhantomData<fn() -> T>);

/// Row-level change-tick filter with read-only slice access (`AccessMode::CHANGED`) for [`QueryTuple`] (`G-03`).
pub struct Changed<T>(PhantomData<fn() -> T>);

/// Row-level added-tick filter with read-only slice access (`AccessMode::ADDED`) for [`QueryTuple`] (`G-03`).
pub struct Added<T>(PhantomData<fn() -> T>);

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
/// `Option<&mut T>`, [`With<T>`], [`Without<T>`], [`Changed<T>`], [`Added<T>`], and [`FfiEntity`]).
pub trait QueryTerm: private::TermSealed {
    /// Zero-copy slice or unit marker yielded for each chunk of this term.
    type Slice<'chunk>;

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

/// Sealed tuple trait over 1..=8 [`QueryTerm`] items (plus single-item shorthand) (`G-03`).
pub trait QueryTuple: private::TupleSealed {
    /// Tuple (or single item) of chunk slices yielded per chunk.
    type Slices<'chunk>;

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

macro_rules! impl_query_tuple {
    ($cap:expr => $($term:ident),+ $(,)?) => {
        impl<$($term: QueryTerm),+> private::TupleSealed for ($($term,)+) {}

        impl<$($term: QueryTerm),+> QueryTuple for ($($term,)+) {
            type Slices<'chunk> = ($($term::Slice<'chunk>,)+);

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

/// Typed chunk iterator over a declared [`QueryTuple`] `Q` (`G-03`), dispensing borrow-checked,
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

    /// Iterates sequentially over all chunks, propagating any `TYPE_MISMATCH` / `ACCESS_CONFLICT`
    /// or callback error (`G-03`).
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
    /// typed `Q::Slices<'_>` to `f` (`P-07` + `G-03`).
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
}
