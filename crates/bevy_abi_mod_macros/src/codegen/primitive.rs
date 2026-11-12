use crate::codegen::{AccessorSpec, ReceiverKind, SlotSpec};
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::Type;

pub(crate) fn spec(
    sys_crate: &TokenStream,
    member_tokens: &TokenStream,
    prim_ty: &Type,
    is_abi_pod: bool,
    field_span: Span,
) -> AccessorSpec {
    let assertion = if is_abi_pod {
        quote_spanned! { field_span =>
            const _: fn() = || {
                fn assert_primitive_pod<T: #sys_crate::AbiPod + ::core::default::Default>() {}
                #sys_crate::assert_abi_pod::<#prim_ty>();
                assert_primitive_pod::<#prim_ty>();
            };
        }
    } else {
        quote_spanned! { field_span =>
            const _: fn() = || {
                fn assert_platform_primitive<
                    T: #sys_crate::RawInsertable + ::core::marker::Copy + ::core::default::Default,
                >() {}
                #sys_crate::assert_raw_insertable::<#prim_ty>();
                assert_platform_primitive::<#prim_ty>();
            };
        }
    };

    AccessorSpec {
        collision_label: "accessor",
        allows_raw_insertable: true,
        allows_abi_pod: is_abi_pod,
        assertions: vec![assertion],
        pod_assertion: None,
        slots: vec![
            SlotSpec {
                prefix: "get",
                receiver: ReceiverKind::Const {
                    on_invalid: quote! { <#prim_ty as ::core::default::Default>::default() },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { #prim_ty },
                valid_body: quote! {
                    unsafe {
                        ::core::ptr::read(::core::ptr::addr_of!((*typed_ptr).#member_tokens))
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
                extra_params_fn: quote! { , val: #prim_ty },
                extra_params_vt: quote! { , #prim_ty },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        ::core::ptr::write(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            val,
                        );
                    }
                    #sys_crate::FfiResult::OK
                },
            },
        ],
    }
}
