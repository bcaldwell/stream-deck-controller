use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub struct ChangeEventVar<T: Copy + PartialEq> {
    value: Arc<RwLock<T>>,
    sender: mpsc::Sender<T>,
}

impl<T: Copy + PartialEq> ChangeEventVar<T> {
    pub fn new(init_value: T) -> (ChangeEventVar<T>, mpsc::Receiver<T>) {
        let (tx, rx) = mpsc::channel(1);
        let tracker = ChangeEventVar {
            value: Arc::new(RwLock::new(init_value)),
            sender: tx,
        };
        return (tracker, rx);
    }

    pub async fn get(&self) -> T {
        return *self.value.read().await;
    }

    pub async fn set(&mut self, value: T) -> Result<()> {
        let mut lock = self.value.write().await;
        if value == *lock {
            return Ok(());
        }

        *lock = value;
        self.sender
            .send(value)
            .await
            .map_err(|e| anyhow!("failed to send update: {}", e))?;

        Ok(())
    }
}
