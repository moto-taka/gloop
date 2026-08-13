//! Extensible command and HTTP provider adapters.

pub mod adapter;
mod command;
pub mod config;
mod http;
pub mod models;
pub mod registry;

pub use adapter::{
    AdapterCapabilities, AdapterCapability, AdapterError, AdapterErrorClass, AdapterEvent,
    AdapterEventKind, AdapterEventSender, AdapterOutput, AdapterRequest, AdapterResponse,
    OutputFormat, ProviderAdapter, TokenUsage,
};
pub use config::{
    AnthropicProfile, CommandProfile, CommandPromptMode, ConfigError, OpenAiProfile,
    PROJECT_CONFIG_PATH, Profile, ProfileKind, ProfileStore, SecretRef, USER_CONFIG_FILE,
};
pub use models::{
    CatalogFamily, CatalogModel, ModelDiscovery, catalog_family_for_argv0,
    discover_models_for_argv0, merge_profile_models, parse_model_list,
};
pub use registry::{
    ModelOrigin, ProbeFailure, ProbeResult, ProviderRegistry, ProviderSelection, RegistryResponse,
    ResolvedAdapter, SelectionOrigin,
};
