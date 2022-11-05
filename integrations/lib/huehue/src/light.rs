use crate::color::{Color, Component, Temperature, RGB8};
use crate::http::HueError;
use crate::models::lights::{
    GetLightsResponseItem, LightOnRequest, LightSetBrightnessRequest, LightSetColorRequest,
};
use crate::models::GenericResponse;
use crate::{http, Hue};

pub type Lights = Vec<Light>;

// changes:
// - added support for grouped light

#[derive(Debug, Clone)]
pub enum LightResource {
    Light,
    GroupedLight,
}
impl LightResource {
    fn endpoint(&self) -> &str {
        match self {
            LightResource::Light => "light",
            LightResource::GroupedLight => "grouped_light",
        }
    }
    fn from_type(s: &str) -> LightResource {
        match s {
            "light" => LightResource::Light,
            "grouped_light" => LightResource::GroupedLight,
            _ => LightResource::Light,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Light {
    pub hue: Hue,
    pub resource: LightResource,
    pub id: uuid::Uuid,
    pub name: String,
    pub on: bool,
    pub brightness: Option<f32>,
    pub color: Option<Color>,
    pub temperature: Option<Temperature>,
}

impl Light {
    pub fn new(hue: &Hue, light: GetLightsResponseItem) -> Light {
        Light {
            hue: hue.clone(),
            id: light.id,
            resource: LightResource::from_type(&light.r#type),
            name: match light.metadata {
                Some(metadata) => metadata.name.to_string(),
                None => "".to_string(),
            },
            on: light.on.on,
            brightness: light.dimming.map(|dimming| dimming.brightness),
            color: light.color,
            temperature: light.color_temperature,
        }
    }

    pub async fn switch(&mut self, on: bool) -> Result<(), HueError> {
        let url = self
            .hue
            .url(format!("clip/v2/resource/{}/{}", self.resource.endpoint(), self.id).as_str());
        let application_key = self.hue.application_key().clone().unwrap();
        let request_payload = LightOnRequest::new(on);

        match http::put_auth::<GenericResponse, LightOnRequest>(
            application_key,
            url,
            &request_payload,
        )
        .await
        {
            Ok(_) => {
                self.on = on;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn set_color(&mut self, component: Component) -> Result<(), HueError> {
        if self.color.is_none() {
            return Err(HueError::Unsupported);
        }

        let url = self
            .hue
            .url(format!("clip/v2/resource/{}/{}", self.resource.endpoint(), self.id).as_str());
        let application_key = self.hue.application_key().clone().unwrap();
        let request_payload = LightSetColorRequest::new(component.clone());

        match http::put_auth::<GenericResponse, LightSetColorRequest>(
            application_key,
            url,
            &request_payload,
        )
        .await
        {
            Ok(_) => {
                if let Some(color) = &mut self.color {
                    color.xy = component;
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn set_color_rgb(&mut self, rgb: RGB8) -> Result<(), HueError> {
        if let Some(color) = &self.color {
            let xy = color.gamut.xy_from_rgb8(rgb);
            self.set_color(xy).await
        } else {
            Err(HueError::Unsupported)
        }
    }

    pub async fn dimm(&mut self, value: f32) -> Result<(), HueError> {
        if self.brightness.is_none() {
            return Err(HueError::Unsupported);
        }

        let url = self
            .hue
            .url(format!("clip/v2/resource/{}/{}", self.resource.endpoint(), self.id).as_str());
        let application_key = self.hue.application_key().clone().unwrap();
        let request_payload = LightSetBrightnessRequest::new(value.clone());

        match http::put_auth::<GenericResponse, LightSetBrightnessRequest>(
            application_key,
            url,
            &request_payload,
        )
        .await
        {
            Ok(_) => {
                if let Some(brightness) = &mut self.brightness {
                    *brightness = value;
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
