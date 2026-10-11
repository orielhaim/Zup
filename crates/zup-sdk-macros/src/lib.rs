use proc_macro::TokenStream;
use quote::quote;

#[proc_macro_attribute]
pub fn settings(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item: proc_macro2::TokenStream = item.into();
    quote! {
        #[derive(Default)]
        #[derive(zup_sdk::__private::serde::Deserialize)]
        #[derive(zup_sdk::__private::schemars::JsonSchema)]
        #[serde(crate = "zup_sdk::__private::serde")]
        #[schemars(crate = "zup_sdk::__private::schemars")]
        #item
    }
    .into()
}
