use anyhow::{anyhow, Ok, Result};
use reqwest;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::{sync::Arc, time};
use tokio::sync::RwLock;
use url::Url;

#[derive(Debug, Clone)]
pub struct Homebridge {
    endpoint: Url,
    auth: Arc<RwLock<HomebridgeAuth>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct HomebridgeAuth {
    username: String,
    password: String,
    #[serde(skip)]
    token: Option<String>,
    #[serde(skip)]
    expires_at: Option<time::SystemTime>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct HomebridgeAuthResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HomebridgeDeviceResponse {
    #[serde(rename = "uuid")]
    uuid: String,
    #[serde(rename = "type")]
    utype: String,
    unique_id: String,
    human_type: String,
    service_name: String,
    values: HomebridgeValues,
}

#[derive(Debug)]
pub struct HomebridgeDevice {
    homebridge: Homebridge,
    response: HomebridgeDeviceResponse,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HomebridgeValues {
    on: Option<u64>,
    brightness: Option<u64>,
    color_temperature: Option<u64>,
}

impl Homebridge {
    pub async fn new(endpoint: &str, username: &str, password: &str) -> Result<Homebridge> {
        let endpoint = Url::parse(endpoint)?;
        let hb = Homebridge {
            endpoint: endpoint,
            auth: Arc::new(RwLock::new(HomebridgeAuth {
                username: username.to_string(),
                password: password.to_string(),
                token: None,
                expires_at: None,
            })),
        };
        hb.autheniticate().await?;
        return Ok(hb);
    }

    async fn autheniticate(&self) -> Result<()> {
        let client = reqwest::Client::new();
        let url = self.url_for_path("api/auth/login");
        let mut auth = self.auth.write().await;
        let response = client
            .post(url)
            .json(&*auth)
            .send()
            .await?
            .json::<HomebridgeAuthResponse>()
            .await?;
        auth.token = Some(response.access_token);
        auth.expires_at =
            Some(time::SystemTime::now() + time::Duration::from_secs(response.expires_in));
        return Ok(());
    }

    pub async fn device_by_id(&self, uuid: String) -> Result<HomebridgeDevice> {
        return self
            .make_get_request(&format!("api/accessories/{}", &uuid))
            .await
            .map(|r| HomebridgeDevice::new(self.clone(), r));
    }

    pub async fn devices(&self) -> Result<Vec<HomebridgeDevice>> {
        let response: Vec<HomebridgeDeviceResponse> =
            self.make_get_request("api/accessories").await?;

        return Ok(response
            .into_iter()
            .map(|r| HomebridgeDevice::new(self.clone(), r))
            .collect());
    }

    async fn client(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let token = self.auth_token().await?;
        let url = self.url_for_path(path);

        let client = reqwest::Client::new();
        return Ok(client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", token)));
    }

    async fn make_put_request<T: DeserializeOwned, U: Serialize>(
        &self,
        path: &str,
        data: &U,
    ) -> Result<T> {
        let r = self
            .client(reqwest::Method::PUT, path)
            .await?
            .json(data)
            .send()
            .await?;

        return r.json::<T>().await.map_err(anyhow::Error::msg);
    }

    async fn make_get_request<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let r = self
            .client(reqwest::Method::GET, path)
            .await?
            .send()
            .await?;
        return r.json::<T>().await.map_err(anyhow::Error::msg);
    }

    async fn auth_token(&self) -> Result<String> {
        // determine if the token is valid in a block to only lock the reference for this time to avoid a deadlock
        let has_valid_token = {
            let auth = self.auth.read().await;
            auth.has_valid_token()
        };

        if !has_valid_token {
            self.autheniticate().await?;
        }

        // get the lock back, in case autheniticate needed it
        let auth = self.auth.read().await;
        match &auth.token {
            Some(token) => Ok(token.to_string()),
            None => Err(anyhow!("unable to get auth token for homebridge")),
        }
    }

    fn url_for_path(&self, path: &str) -> String {
        let mut url = self.endpoint.clone();
        let path = std::path::Path::new(url.path()).join(path);
        // Any non-Unicode sequences are replaced with U+FFFD REPLACEMENT CHARACTER.
        let path = path.to_string_lossy();
        url.set_path(path.as_ref());
        return url.to_string();
    }
}

impl HomebridgeAuth {
    fn has_valid_token(&self) -> bool {
        match &self.token {
            Some(_token) => self.has_token_expired(),
            None => false,
        }
    }

    fn has_token_expired(&self) -> bool {
        match &self.expires_at {
            Some(expires_at) => {
                *expires_at - time::Duration::from_secs(60 * 60 * 2) > time::SystemTime::now()
            }
            // assume if expires_at is not set, it never expires
            None => false,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessoriesPutRequest {
    characteristic_type: String,
    value: serde_json::value::Value,
}

impl HomebridgeDevice {
    fn new(homebridge: Homebridge, response: HomebridgeDeviceResponse) -> HomebridgeDevice {
        return HomebridgeDevice {
            homebridge: homebridge,
            response: response,
        };
    }

    fn endpoint(&self) -> String {
        return format!("api/accessories/{}", self.response.unique_id);
    }

    pub fn name(&self) -> String {
        return self.response.service_name.clone();
    }

    pub fn unique_id(&self) -> String {
        return self.response.unique_id.clone();
    }

    pub fn on(&self) -> Option<bool> {
        match self.response.values.on {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        }
    }

    pub fn brightness(&self) -> Option<u64> {
        return self.response.values.brightness;
    }

    pub async fn switch(&mut self, on: bool) -> Result<()> {
        let value: u64 = match on {
            true => 1,
            false => 0,
        };

        if self.response.values.on.is_none() {
            return Err(anyhow!("device does not impliment switch"));
        }

        let data = AccessoriesPutRequest {
            characteristic_type: "On".to_string(),
            value: serde_json::value::Value::Number(serde_json::value::Number::from(value)),
        };

        self.response = self
            .homebridge
            .make_put_request(&self.endpoint(), &data)
            .await?;

        Ok(())
    }

    pub async fn dimm(&mut self, brightness: u64) -> Result<()> {
        if self.response.values.brightness.is_none() {
            return Err(anyhow!("device does not impliment dimm"));
        }

        let data = AccessoriesPutRequest {
            characteristic_type: "Brightness".to_string(),
            value: serde_json::value::Value::Number(serde_json::value::Number::from(brightness)),
        };

        self.response = self
            .homebridge
            .make_put_request(&self.endpoint(), &data)
            .await?;

        Ok(())
    }
}
