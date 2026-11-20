//! src/services/analytics — 遥测与事件上报（sink 链 + GrowthBook 结构）。
pub mod config;
pub mod datadog;
pub mod first_party_event_logger;
pub mod first_party_event_logging_exporter;
pub mod growthbook;
pub mod index;
pub mod metadata;
pub mod sink;
pub mod sink_killswitch;
