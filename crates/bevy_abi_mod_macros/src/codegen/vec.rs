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
        ],
    }
}

use syn::spanned::Spanned as _;
