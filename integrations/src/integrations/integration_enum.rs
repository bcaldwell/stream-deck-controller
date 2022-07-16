use crate::integrations::{airplay, homebridge, http, hue};
use crate::integrations::{
    Integration, IntegrationConfiguration, IntegrationResult, IntoIntegration,
};

use anyhow::Result;
use enum_dispatch::enum_dispatch;

#[enum_dispatch(Integration)]
pub enum IntegrationEnum {
    Hue(hue::Integration),
    Homebridge(homebridge::Integration),
    Http(http::Integration),
    Airplay(airplay::Integration),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
#[enum_dispatch(IntoIntegration)]
pub enum IntegrationsConfigurationEnum {
    Hue(IntegrationConfiguration<hue::IntegrationConfig>),
    Homebridge(IntegrationConfiguration<homebridge::IntegrationConfig>),
    Airplay(IntegrationConfiguration<airplay::IntegrationConfig>),
    Http(IntegrationConfiguration<http::IntegrationConfig>),
}

impl std::fmt::Display for IntegrationsConfigurationEnum {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            IntegrationsConfigurationEnum::Hue(_) => write!(f, "hue"),
            IntegrationsConfigurationEnum::Homebridge(_) => write!(f, "homebridge"),
            IntegrationsConfigurationEnum::Airplay(_) => write!(f, "airplay"),
            IntegrationsConfigurationEnum::Http(_) => write!(f, "http"),
        }
    }
}
