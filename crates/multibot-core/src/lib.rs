pub mod account;
pub mod browser_pane;
pub mod cache;
pub mod changelog;
pub mod chat_scroll;
pub mod daemon_client;
pub mod daemon_deploy;
pub mod footprint;
pub mod outbox;
pub mod settings;
pub mod voice;
pub mod workspaces;
pub mod zigshel_import;

pub use daemon_client::{DaemonConnection, DaemonTarget};
pub use footprint::{FootprintSnapshot, sample_self_footprint};
pub use settings::{
    MultibotSettings, SidebarMode, SshHostConfig, sanitize_identity_file,
    validate_identity_file_str,
};
pub use zigshel_import::{
    import_ssh_hosts_readonly, import_zigshel_bundle_readonly, merge_zigshel_bundle,
};
