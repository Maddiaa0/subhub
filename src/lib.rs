mod cli;
mod codex;
mod credentials;
mod error;
mod gateway;
mod observability;
mod output;
mod paths;
mod provider;
mod service;
pub mod usage;

pub use error::{Error, Result};

// Internal surface shared across modules. Modules deeper in the tree import
// from their concrete homes; these re-exports exist for the handful of items
// used almost everywhere.
pub(crate) use credentials::gateway_credentials;
pub(crate) use credentials::index::{index_path, load_index};
pub(crate) use credentials::oauth::{claude_version, refresh_claude_credential};
pub(crate) use credentials::vault::{
    VAULT_SERVICE, credential_delete, credential_read, credential_write,
};
pub(crate) use paths::{config_base_path, save_json_file};
pub(crate) use provider::Provider;

pub fn run() -> Result<()> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(Error::Message(
            "subhub currently requires macOS or Linux".into(),
        ));
    }

    cli::dispatch(cli::Cli::parse_args())
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Runtime::new()
        .map_err(|error| Error::Message(format!("could not start async runtime: {error}")))
}
