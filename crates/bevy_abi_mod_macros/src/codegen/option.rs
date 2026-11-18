use crate::codegen::{AccessorSpec, ReceiverKind, SlotSpec};
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::{Type, spanned::Spanned as _};

pub(crate) fn spec(
    sys_crate: &TokenStream,
    member_tokens: &TokenStream,
    field_ty: &Type,
    inner_ty: &Type,
    field_span: Span,
) -> AccessorSpec {
    let inner_span = inner_ty.span();
    let option_assertion = quote_spanned! { field_span =>
        const _: fn() = || {
            fn assert_ffi_option_field<Inner, Opt: __BamFfiOption<Inner>>() {}
            assert_ffi_option_field::<#inner_ty, #field_ty>();
        };
    };
    let inner_assertion = quote_spanned! { inner_span =>
        const _: fn() = || {
            #sys_crate::assert_abi_pod::<#inner_ty>();
        };
    };
    let pod_assertion = Some(quote_spanned! { field_span =>
        #sys_crate::assert_abi_pod::<#field_ty>();
    });

    AccessorSpec {
        collision_label: "accessor",
        allows_raw_insertable: true,
        allows_abi_pod: true,
        assertions: vec![option_assertion, inner_assertion],
        pod_assertion,
        slots: vec![
            SlotSpec {
                prefix: "get",
                receiver: ReceiverKind::Const {
                    on_invalid: quote! { __bam_none_option::<#inner_ty>() },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { #sys_crate::FfiOption<#inner_ty> },
                valid_body: quote! {
                    unsafe {
                        __bam_read_option::<#inner_ty, #field_ty>(
                            ::core::ptr::addr_of!((*typed_ptr).#member_tokens),
                        )
                    }
                },
            },
            SlotSpec {
                prefix: "set",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { #sys_crate::FfiResult::NULL_POINTER },
                    pre_align_check: None,
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , val: #sys_crate::FfiOption<#inner_ty> },
                extra_params_vt: quote! { , #sys_crate::FfiOption<#inner_ty> },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_write_option::<#inner_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            val,
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
                        __bam_clear_option::<#inner_ty, #field_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                        )
                    }
                },
            },
        ],
    }
}
