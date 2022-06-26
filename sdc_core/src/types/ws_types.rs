#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
pub enum WsActions {
    ButtonPressed { profile: Option<String>, button: u8 },
    SetButtons { buttons: Vec<SetButtonUI> },
    SetButton { index: u8, button: SetButtonUI },
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct SetButtonUI {
    pub image: Option<String>,
    pub color: Option<String>,
}
