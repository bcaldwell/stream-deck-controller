use serde::{Deserialize, Deserializer, Serialize};

use crate::color::{Color, Component, Temperature};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct On {
	pub on: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dimming {
	pub brightness: f32,
	pub min_dim_level: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetLightsResponse {
	pub data: Option<Vec<GetLightsResponseItem>>,
	pub error: Option<crate::models::Error>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetLightsResponseItem {
	#[serde(rename = "type")]
	pub r#type: String,

	pub id: uuid::Uuid,
	pub metadata: Option<super::generic::Metadata>,
	pub dimming: Option<Dimming>,
	pub on: On,

	// default is needed to make optional optional fields: https://stackoverflow.com/questions/44301748/how-can-i-deserialize-an-optional-field-with-custom-functions-using-serde
	// copy the deserialize with function from: https://stackoverflow.com/questions/69458092/how-to-properly-handle-empty-null-and-valid-json
	#[serde(default)]
	#[serde(deserialize_with = "object_empty_as_none")]
	pub color: Option<Color>,
	#[serde(default)]
	#[serde(deserialize_with = "object_empty_as_none")]
	pub color_temperature: Option<Temperature>,
}

pub fn object_empty_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
	D: Deserializer<'de>,
	for<'a> T: Deserialize<'a>,
{
	#[derive(Deserialize, Debug)]
	#[serde(deny_unknown_fields)]
	struct Empty {}

	#[derive(Deserialize, Debug)]
	#[serde(untagged)]
	enum Aux<T> {
		T(T),
		Empty(Empty),
		Null,
	}

	match Deserialize::deserialize(deserializer)? {
		Aux::T(t) => Ok(Some(t)),
		Aux::Empty(_) | Aux::Null => Ok(None),
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightOnRequest {
	pub on: On,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightSetColorRequestXY {
	pub xy: Component,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightSetColorRequest {
	pub color: LightSetColorRequestXY,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightSetBrightnessRequestBrightness {
	pub brightness: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightSetBrightnessRequest {
	pub dimming: LightSetBrightnessRequestBrightness,
}

impl LightOnRequest {
	pub fn new(on: bool) -> LightOnRequest {
		LightOnRequest { on: On { on } }
	}
}

impl LightSetColorRequest {
	pub fn new(color: Component) -> LightSetColorRequest {
		LightSetColorRequest {
			color: LightSetColorRequestXY { xy: color },
		}
	}
}

impl LightSetBrightnessRequest {
	pub fn new(brightness: f32) -> LightSetBrightnessRequest {
		LightSetBrightnessRequest {
			dimming: LightSetBrightnessRequestBrightness {
				brightness: brightness.max(0.0).min(100.0),
			},
		}
	}
}
