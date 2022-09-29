#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
pub enum WsActions {
    ButtonPressed { profile: Option<String>, button: u8 },
    SetButtons { buttons: Vec<SetButtonUI> },
    SetButton { index: u8, button: SetButtonUI },
}

impl WsActions {
    pub fn type_string(&self) -> String {
        match self {
            WsActions::ButtonPressed { .. } => "Button Pressed",
            WsActions::SetButtons { .. } => "Set Buttons",
            WsActions::SetButton { .. } => "Set Button",
        }
        .to_string()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct SetButtonUI {
    pub image: Option<String>,
    pub color: Option<String>,
}
