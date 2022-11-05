use crate::models::generic::{GenericIdentifier, Metadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRoomsResponse {
    pub data: Option<Vec<GetRoomsResponseItem>>,
    pub error: Option<crate::models::Error>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRoomsResponseItem {
    #[serde(rename = "type")]
    pub r#type: String,

    pub id: uuid::Uuid,
    pub metadata: Metadata,
    pub children: Option<Vec<GenericIdentifier>>,
    pub services: Option<Vec<GenericIdentifier>>,
}
