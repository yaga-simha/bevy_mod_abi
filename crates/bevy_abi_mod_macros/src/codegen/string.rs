use crate::codegen::{AccessorSpec, ReceiverKind, SlotSpec};
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::Type;

pub(crate) fn spec(
    sys_crate: &TokenStream,
    member_tokens: &TokenStream,
    str_ty: &Type,
    field_span: Span,
) -> AccessorSpec {
    let assertion = quote_spanned! { field_span =>
        const _: fn() = || {
            fn assert_std_string_field<T: __BamStdString>() {}
            assert_std_string_field::<#str_ty>();
        };
    };

    AccessorSpec {
        collision_label: "accessor",
        allows_raw_insertable: false,
        allows_abi_pod: false,
        assertions: vec![assertion],
        pod_assertion: None,
        slots: vec![
            SlotSpec {
                prefix: "get",
                receiver: ReceiverKind::Const {
                    on_invalid: quote! { #sys_crate::StrView::empty() },
                },
                extra_params_fn: quote! {},
                extra_params_vt: quote! {},
                ret_ty: quote! { #sys_crate::StrView },
                valid_body: quote! {
                    unsafe {
                        __bam_read_string::<#str_ty>(
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
                        if view.len > 0 && view.ptr.is_null() {
                            return #sys_crate::FfiResult::NULL_POINTER;
                        }
                    }),
                    on_unaligned: quote! { #sys_crate::FfiResult::INVALID_ARGUMENT },
                },
                extra_params_fn: quote! { , view: #sys_crate::StrView },
                extra_params_vt: quote! { , #sys_crate::StrView },
                ret_ty: quote! { #sys_crate::FfiResult },
                valid_body: quote! {
                    unsafe {
                        __bam_write_string::<#str_ty>(
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                            view,
                        )
                    }
                },
            },
        ],
    }
}
