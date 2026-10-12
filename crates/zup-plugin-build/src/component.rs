use wit_component::ComponentEncoder;

#[derive(Debug, thiserror::Error)]
pub enum ComponentError {
    #[error("this is already a component rather than a core module")]
    AlreadyAComponent,
    #[error(
        "this does not build a WebAssembly module for a plugin; compile it with \
         `--target wasm32-unknown-unknown` and a `cdylib` crate type"
    )]
    NotAModule,
    #[error("the module does not implement the plugin world: {0}")]
    Incompatible(#[from] anyhow::Error),
}

pub fn componentize(module: &[u8]) -> Result<Vec<u8>, ComponentError> {
    if module.starts_with(b"\0asm\x0d\0\x01\0") {
        return Err(ComponentError::AlreadyAComponent);
    }
    if !module.starts_with(b"\0asm\x01\0\0\0") {
        return Err(ComponentError::NotAModule);
    }

    Ok(ComponentEncoder::default()
        .module(module)?
        .validate(true)
        .encode()?)
}
