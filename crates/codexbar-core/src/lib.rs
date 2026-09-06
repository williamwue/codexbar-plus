//! CodexBar for Windows — provider engine.
//!
//! Windows port of the platform-agnostic half of upstream CodexBar
//! (`Sources/CodexBarCore`, MIT, https://github.com/steipete/CodexBar).
//! The canonical models, HTTP retry contract, provider wire formats and the adaptive
//! refresh table are kept deliberately identical so numbers match the macOS app.

pub mod adaptive;
pub mod config;
pub mod cookies;
pub mod cost;
pub mod format;
pub mod http;
pub mod jwt;
pub mod model;
pub mod paths;
pub mod plugin;
pub mod providers;
pub mod secret;
pub mod secure;
pub mod settings;
pub mod status;
pub mod subprocess;

pub use config::Config;
pub use cookies::{CookieError, CookieSource};
pub use cost::{CostStore, CostSummary};
pub use http::HttpClient;
pub use model::{
    Confidence, CreditsSnapshot, DetailRow, FetchKind, FetchResult, Identity, NamedRateWindow,
    RateWindow, UsageSnapshot,
};
pub use providers::{descriptor, descriptors, fetch, ids, ProviderDescriptor};
pub use secret::SecretStore;
pub use settings::Settings;
pub use status::{Indicator, ProviderStatus};
