use crate::codegen::{AccessorSpec, ReceiverKind, SlotSpec};
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::Type;

pub(crate) fn spec(
    sys_crate: &TokenStream,
    member_tokens: &TokenStream,
    field_ty: &Type,
    elem_ty: &Type,
    field_span: Span,
) -> AccessorSpec {
    let elem_span = elem_ty.span();
    let vec_assertion = quote_spanned! { field_span =>
        const _: fn() = || {
            fn assert_std_vec_field<Elem, V: __BamStdVec<Elem>>() {}
            assert_std_vec_field::<#elem_ty, #field_ty>();
        };
    };
    let elem_assertion = quote_spanned! { elem_span =>
        const _: fn() = || {
            #sys_crate::assert_abi_pod::<#elem_ty>();
        };
    };

    AccessorSpec {
        collision_label: "accessor",
        allows_raw_insertable: false,
        allows_abi_pod: false,
        assertions: vec![vec_assertion, elem_assertion],
        pod_assertion: None,
        slots: vec![
            SlotSpec {
                prefix: "get",
                receiver: ReceiverKind::Const {
                    on_invalid: quote! { #sys_crate::FfiSlice::empty() },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { #sys_crate::FfiSlice<#elem_ty> },
                valid_body: quote! {
                    unsafe {
                        __bam_read_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of!((*typed_ptr).#member_tokens),
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "set",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: Some(quote! {
                        if slice.len > 0 && slice.ptr.is_null() {
                            return #sys_crate::FfiResult::NULL_POINTER;
                        }
                    }),
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , slice: #sys_crate::FfiSlice<#elem_ty> },
                extra_params_vt: quote! { , #sys_crate::FfiSlice<#elem_ty> },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_write_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            slice,
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "len",
                receiver: ReceiverKind::Const {
                    on_invalid: quote! { 0usize },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { usize },
                valid_body: quote! {
                    unsafe {
                        __bam_len_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of!((*typed_ptr).#member_tokens),
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "clear",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: None,
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_clear_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "push",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: None,
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , val: #elem_ty },
                extra_params_vt: quote! { , #elem_ty },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_push_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            val,
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "pop",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiBool::FALSE },
                    pre_align_check: Some(quote! {
                        if out_elem.is_null() || !out_elem.is_aligned() {
                            return #sys_crate::FfiBool::FALSE;
                        }
                    }),
                    on_unaligned: quote! { #sys_crate::FfiBool::FALSE },
                },
                extra_params_fn: quote! { , out_elem: *mut #elem_ty },
                extra_params_vt: quote! { , *mut #elem_ty },
                ret_ty: quote! { #sys_crate::FfiBool },
                valid_body: quote! {
                    unsafe {
                        __bam_pop_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            out_elem,
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "remove",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: None,
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , index: usize, out_elem: *mut #elem_ty },
                extra_params_vt: quote! { , usize, *mut #elem_ty },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_remove_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            index,
                            out_elem,
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "swap_remove",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: None,
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , index: usize, out_elem: *mut #elem_ty },
                extra_params_vt: quote! { , usize, *mut #elem_ty },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_swap_remove_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            index,
                            out_elem,
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "truncate",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: None,
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , new_len: usize },
                extra_params_vt: quote! { , usize },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_truncate_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            new_len,
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "reserve",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: None,
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , additional: usize },
                extra_params_vt: quote! { , usize },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_reserve_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            additional,
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "as_mut_slice",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { ::core::ptr::null_mut() },
                    pre_align_check: Some(quote! {
                        if out_len.is_null() || !out_len.is_aligned() {
                            return ::core::ptr::null_mut();
                        }
                    }),
                    on_unaligned: quote! { ::core::ptr::null_mut() },
                },
                extra_params_fn: quote! { , out_len: *mut usize },
                extra_params_vt: quote! { , *mut usize },
                ret_ty: quote! { *mut #elem_ty },
                valid_body: quote! {
                    unsafe {
                        __bam_as_mut_slice_vec::<#elem_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            out_len,
                        )
                    }
                },
            },
        ],
    }
}

use syn::spanned::Spanned as _;
