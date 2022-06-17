use anyhow::{anyhow, Result};
use huehue::models::device_type::DeviceType;
use huehue::Hue;
use std::collections::HashMap;
use std::env;
use std::time::Duration;
use tokio::runtime::Builder;
use tokio::sync::mpsc;
use ws::{listen, Handler, Request, Response, Sender};

// use std::error::Error;

// type BoxResult<T> = Result<T, Box<Error>>;

trait Integration {
    fn name(&self) -> &str;
    fn actions(&self) -> Vec<&str>;
    fn execute_action(&self, action: String);

    // fn resync(&self);
}

struct HueIntegration {
    hue: Hue,
    light_name_to_id: HashMap<String, String>,
}

impl HueIntegration {
    async fn new() -> HueIntegration {
        let bridges = Hue::bridges(Duration::from_secs(5)).await;
        let device_type = DeviceType::new("benjamin".to_owned(), "streamdeck".to_owned()).unwrap();

        let hue = Hue::new_with_key(
            bridges.first().unwrap().address,
            device_type,
            env::var("HUE_USERNAME").unwrap(),
        )
        .await
        .expect("Failed to run bridge information.");

        println!(
            "Connected to hue bridge at {}",
            bridges.first().unwrap().address,
        );

        let mut hue_integration = HueIntegration {
            hue: hue,
            light_name_to_id: HashMap::new(),
        };

        hue_integration.sync().await;

        return hue_integration;
    }

    async fn sync(&mut self) {
        let lights = self.hue.lights().await.unwrap();
        self.light_name_to_id.clear();

        for light in lights {
            self.light_name_to_id
                .insert(light.name, light.id.to_string());
        }
    }

    async fn get_light_by_name(&self, name: &str) -> Result<huehue::Light> {
        let id = match self.light_name_to_id.get(name) {
            Some(x) => x.to_string(),
            None => return Err(anyhow!("Light named {} not found", name)),
        };

        Ok(self.hue.lights_by_id(id).await?)
    }

    async fn toggle_light_action(&self, light_name: String) -> Result<()> {
        // let light_name = match options.get("light") {
        //     Some(x) => x,
        //     None => return Err(anyhow!("light is a required option for hue toggle action")),
        // };

        let mut light = self.get_light_by_name(&light_name).await?;
        Ok(light.switch(!light.on).await?)
    }
}

impl HueIntegration {
    // fn name(&self) -> &str {
    //     return "Hue";
    // }

    // fn actions(&self) -> Vec<&str> {
    //     return vec!["toggle_light"];
    // }

    async fn execute_action(&self, action: Actions) -> Result<()> {
        match action {
            Actions::Toggle { light, room } => {
                return Ok(self.toggle_light_action(light).await?);
            }
        };
    }
}

/// Multiple different commands are multiplexed over a single channel.
#[derive(Debug)]
enum Actions {
    Toggle { light: String, room: String },
}

// Server web application handler
struct Server {
    ws: Sender,
    event_processor: mpsc::Sender<Actions>,
}

impl Handler for Server {
    //
    fn on_request(&mut self, req: &Request) -> ws::Result<Response> {
        // Using multiple handlers is better (see router example)
        match req.resource() {
            // The default trait implementation
            "/ws" => Response::from_request(req),
            // // Create a custom response
            // "/" => Ok(Response::new(200, "OK", INDEX_HTML.to_vec())),
            _ => Ok(Response::new(404, "Not Found", b"404 - Not Found".to_vec())),
        }
    }

    fn on_message(&mut self, msg: ws::Message) -> ws::Result<()> {
        println!("Echo handler received a message: {}", msg);

        let light = msg.as_text().unwrap().to_string();
        // todo remove blocking once running in async
        self.event_processor
            .blocking_send(Actions::Toggle {
                light: light,
                room: "".to_string(),
            })
            .unwrap();
        self.ws.send("accepted")
    }
}

// #[tokio::main]
fn main() {
    let (tx, mut rx) = mpsc::channel::<Actions>(32);

    let rt = Builder::new_current_thread().enable_all().build().unwrap();

    std::thread::spawn(move || {
        rt.block_on(async move {
            let hue_integration = HueIntegration::new().await;

            println!("{:?}", hue_integration.light_name_to_id);

            let manager = tokio::spawn(async move {
                // Start receiving messages
                while let Some(action) = rx.recv().await {
                    hue_integration.execute_action(action).await.unwrap();
                }
            });
            manager.await.unwrap();
        });
    });

    listen("127.0.0.1:8000", |out| Server {
        ws: out,
        event_processor: tx.clone(),
    })
    .unwrap()

    // hue_integration
    //     .execute_action(
    //         "toggle",
    //         HashMap::from([("light".to_string(), "Living Room Bottom".to_string())]),
    //     )
    //     .await
    //     .unwrap();
}
