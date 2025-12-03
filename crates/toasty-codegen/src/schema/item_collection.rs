#[derive(Debug)]
pub(crate) struct ItemCollection {
    pub(crate) ty: syn::Type,
}

impl ItemCollection {
    pub(super) fn from_ast(attr: &syn::Attribute) -> syn::Result<Self> {
        let ty: syn::Type = attr.parse_args()?;
        Ok(Self { ty })
    }
}
