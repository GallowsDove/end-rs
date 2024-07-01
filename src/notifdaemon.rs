#![allow(clippy::too_many_arguments)]
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use zbus::fdo::Result;
use zbus::interface;
use zbus::object_server::SignalContext;
use zvariant::Value;

use crate::config::Config;
use crate::ewwface::{
    eww_close_history, eww_close_notifications, eww_close_window, eww_toggle_history,
    eww_update_history, eww_update_notifications,
};
use crate::utils::{find_icon, save_icon};

pub struct Notification {
    pub app_name: String,
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub timeout_cancelled: bool,
    pub timeout_future: Option<JoinHandle<()>>,
}

pub struct HistoryNotification {
    pub app_name: String,
    pub icon: String,
    pub summary: String,
    pub body: String,
}

pub struct NotificationDaemon {
    pub config: Arc<Mutex<Config>>,
    pub notifications: Arc<Mutex<HashMap<u32, Notification>>>,
    pub notifications_history: Arc<Mutex<Vec<HistoryNotification>>>,
    pub connection: Arc<Mutex<zbus::Connection>>,
    pub next_id: u32,
}

#[interface(name = "org.freedesktop.Notifications")]
impl NotificationDaemon {
    pub async fn notify(
        &mut self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<&str, zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> Result<u32> {
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next_id += 1;
            self.next_id
        };
        let config_main = self.config.lock().await;
        let icon = hints
            .get("image_data")
            .and_then(|value| match value {
                Value::Structure(icon_data) => save_icon(icon_data, id),
                _ => None,
            })
            .or_else(|| {
                hints.get("image-data").and_then(|value| match value {
                    Value::Structure(icon_data) => save_icon(icon_data, id),
                    _ => None,
                })
            })
            .or_else(|| {
                if !app_name.is_empty() {
                    find_icon(app_icon, &config_main).or_else(|| Some(app_icon.to_string()))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| app_icon.to_string());

        let mut expire_timeout = expire_timeout;
        if expire_timeout < 0 {
            let urgency = hints.get("urgency").and_then(|value| match value {
                Value::U8(urgency) => Some(*urgency),
                _ => None,
            });
            match urgency {
                Some(0) => expire_timeout = config_main.timeout.low as i32 * 1000,
                Some(1) => expire_timeout = config_main.timeout.normal as i32 * 1000,
                Some(2) => expire_timeout = config_main.timeout.critical as i32 * 1000,
                _ => expire_timeout = config_main.timeout.normal as i32 * 1000,
            }
        }

        // create a actions vector of type Vec<(String, String)> where even elements are keys and
        // odd elements are values
        let actions: Vec<(String, String)> = actions
            .chunks(2)
            .map(|chunk| {
                let key = chunk.first().unwrap_or(&"").to_string();
                let value = chunk.get(1).unwrap_or(&"").to_string();
                (key, value)
            })
            .collect();

        let history_notification = HistoryNotification {
            app_name: app_name.to_string(),
            icon: icon.clone(),
            summary: summary.to_string(),
            body: body.to_string(),
        };
        let mut notifications_history = self.notifications_history.lock().await;
        notifications_history.push(history_notification);
        // Release the lock before updating the notifications
        if notifications_history.len() > config_main.max_notifications as usize {
            notifications_history.remove(0);
        }
        drop(notifications_history);

        let mut join_handle = None;
        if expire_timeout != 0 {
            // Spawn a task to handle timeout
            let notifications = Arc::clone(&self.notifications);
            let config_thread = Arc::clone(&self.config);
            join_handle = Some(tokio::spawn(async move {
                sleep(Duration::from_millis(expire_timeout as u64)).await;
                let mut notifications = notifications.lock().await;
                if let Some(notif) = notifications.remove(&id) {
                    if let Ok(config) = config_thread.try_lock() {
                        if !notif.timeout_cancelled {
                            eww_update_notifications(&config, &notifications);
                            if notifications.is_empty() {
                                eww_close_notifications(&config);
                            }
                        }
                    }
                }
            }));
        }

        let notification = Notification {
            app_name: app_name.to_string(),
            icon: icon.clone(),
            actions,
            summary: summary.to_string(),
            body: body.to_string(),
            timeout_cancelled: false,
            timeout_future: join_handle,
        };

        let mut notifications = self.notifications.lock().await;
        notifications.insert(id, notification);
        eww_update_notifications(&config_main, &notifications);

        Ok(id)
    }

    pub async fn close_notification(&self, id: u32) -> Result<()> {
        let mut notifications = self.notifications.lock().await;
        if notifications.remove(&id).is_some() {
            println!("Notification with ID {} closed", id);
            let config = self.config.try_lock();
            if config.is_err() {
                println!("Failed to lock config");
                return Err(zbus::fdo::Error::Failed(
                    "Failed to lock config".to_string(),
                ));
            }
            let config = config.unwrap();
            eww_update_notifications(&config, &notifications);
            if notifications.is_empty() {
                eww_close_notifications(&config);
            }
            let dest: Option<&str> = None;
            let conn = self.connection.lock().await;
            conn.emit_signal(
                dest,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "NotificationClosed",
                &(id, 3_u32),
            )
            .await
            .unwrap();
        }
        Ok(())
    }

    pub fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_string(), "actions".to_string()]
    }

    pub fn get_server_information(&self) -> Result<(String, String, String, String)> {
        Ok((
            "NotificationDaemon".to_string(),
            "1.0".to_string(),
            "end-rs".to_string(),
            "1.0".to_string(),
        ))
    }

    pub async fn open_history(&self) -> Result<()> {
        println!("Getting history");
        let config = self.config.try_lock();
        if config.is_err() {
            println!("Failed to lock config");
            return Err(zbus::fdo::Error::Failed(
                "Failed to lock config".to_string(),
            ));
        }
        let config = config.unwrap();
        let history = self.notifications_history.lock().await;
        eww_update_history(&config, &history);
        Ok(())
    }

    pub async fn close_history(&self) -> Result<()> {
        println!("Closing history");
        let config = self.config.try_lock();
        if config.is_err() {
            println!("Failed to lock config");
            return Err(zbus::fdo::Error::Failed(
                "Failed to lock config".to_string(),
            ));
        }
        let config = config.unwrap();
        eww_close_history(&config);
        Ok(())
    }

    pub async fn toggle_history(&self) -> Result<()> {
        println!("Toggling history");
        let config = self.config.try_lock();
        if config.is_err() {
            println!("Failed to lock config");
            return Err(zbus::fdo::Error::Failed(
                "Failed to lock config".to_string(),
            ));
        }
        let config = config.unwrap();
        let history = self.notifications_history.lock().await;
        eww_toggle_history(&config, &history);
        Ok(())
    }

    pub async fn reply_close(&self, id: u32) -> Result<()> {
        println!("Closing reply window");
        let mut notifications = self.notifications.lock().await;
        let config = self.config.try_lock();
        if config.is_err() {
            println!("Failed to lock config");
            return Err(zbus::fdo::Error::Failed(
                "Failed to lock config".to_string(),
            ));
        }
        let config = config.unwrap();
        if let Some(notification) = notifications.get_mut(&id) {
            notification.actions.clear();
            eww_update_notifications(&config, &notifications);
        }
        eww_close_window(&config, "notification-reply").map_err(|e| {
            eprintln!("Failed to close reply window: {}", e);
            zbus::fdo::Error::Failed("Failed to close reply window".to_string())
        })?;
        Ok(())
    }

    #[zbus(signal)]
    pub async fn action_invoked(
        ctx: &SignalContext<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn notification_closed(
        ctx: &SignalContext<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn notification_replied(
        ctx: &SignalContext<'_>,
        id: u32,
        message: &str,
    ) -> zbus::Result<()>;
}

impl NotificationDaemon {
    pub async fn disable_timeout(&self, id: u32) -> Result<()> {
        let mut notifications = self.notifications.lock().await;
        if let Some(notification) = notifications.get_mut(&id) {
            notification.timeout_cancelled = true;
        }
        Ok(())
    }
}
