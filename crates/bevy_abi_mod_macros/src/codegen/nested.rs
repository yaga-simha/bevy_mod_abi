use crate::codegen::{AccessorSpec, ReceiverKind, SlotSpec};
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::Type;

pub(crate) fn spec(
    sys_crate: &TokenStream,
    opaque: bool,
    member_tokens: &TokenStream,
    nested_ty: &Type,
    is_abi_pod_candidate: bool,
    field_span: Span,
) -> AccessorSpec {
    let assertion = if opaque {
        quote_spanned! { field_span =>
            const _: fn() = || {
                fn assert_nested_mod_component<T: #sys_crate::ModComponent>() {}
                assert_nested_mod_component::<#nested_ty>();
            };
        }
    } else {
        quote_spanned! { field_span =>
            const _: fn() = || {
                #sys_crate::assert_raw_insertable::<#nested_ty>();
            };
        }
    };

    let pod_assertion = is_abi_pod_candidate.then(|| {
        quote_spanned! { field_span =>
            #sys_crate::assert_abi_pod::<#nested_ty>();
        }
    });

    AccessorSpec {
        collision_label: "interior-pointer accessor",
        allows_raw_insertable: !opaque,
        allows_abi_pod: !opaque && is_abi_pod_candidate,
        assertions: vec![assertion],
        pod_assertion,
        slots: vec![
            SlotSpec {
                prefix: "ptr",
                receiver: ReceiverKind::Const {
                    on_invalid: quote! { ::core::ptr::null() },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { *const ::core::ffi::c_void },
                valid_body: quote! {
                    unsafe {
                        ::core::ptr::addr_of!((*typed_ptr).#member_tokens)
                            .cast::<::core::ffi::c_void>()
                    }
                },
            },
            SlotSpec {
                prefix: "ptr_mut",
                receiver: ReceiverKind::Mut {
                    on_null: quote! { ::core::ptr::null_mut() },
                    pre_align_check: None,
                    on_unaligned: quote! { ::core::ptr::null_mut() },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { *mut ::core::ffi::c_void },
                valid_body: quote! {
                    unsafe {
                        ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens)
                            .cast::<::core::ffi::c_void>()
                    }
                },
            },
        ],
    }
}
