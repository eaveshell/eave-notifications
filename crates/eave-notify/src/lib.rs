//! A notification server: eave's own engine for the freedesktop notification spec
//! (`org.freedesktop.Notifications`, version 1.2), on the session bus.
//!
//! It only receives and reports. What to show, for how long, and what to keep is up to the plugin
//! watching it ([D-48](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)). The server starts with the first
//! [`watch`]er and lives as long as the bar.
//!
//! ```ignore
//! Subscription::run(eave_notify::watch).map(Message::Notify)
//! ```
//!
//! If another notification daemon owns the name, the server waits in the bus's queue and takes
//! over once that daemon quits. It never pushes one out.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use futures_util::{Stream, StreamExt, stream};
use tokio::sync::{OnceCell, broadcast};
use zbus::fdo::RequestNameReply;
use zbus::zvariant::OwnedValue;
use zbus::{MatchRule, MessageStream};

mod markup;

pub use markup::strip;

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// A notification as an app sent it, made plain.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// Unique while the server runs. A notification that replaces another keeps its id.
    pub id: u32,
    /// The app's name, as it gave it. May be empty.
    pub app: String,
    /// An app id to find its icon by: the `desktop-entry` hint, or the app's icon if it named
    /// one rather than a file.
    pub icon: Option<String>,
    pub summary: String,
    /// Plain text: markup stripped, entities decoded.
    pub body: String,
    /// Each action's key and label, in order. The `default` one is what clicking the
    /// notification does. An `inline-reply` one is in [`reply`](Self::reply) instead.
    pub actions: Vec<(String, String)>,
    /// Whether the app takes a typed answer, and how to ask for it.
    pub reply: Option<Reply>,
    pub urgency: Urgency,
    pub timeout: Timeout,
    pub received: SystemTime,
}

impl Notification {
    /// The label of the `default` action, if it has one.
    pub fn default_action(&self) -> Option<&str> {
        self.actions
            .iter()
            .find(|(key, _)| key == "default")
            .map(|(_, label)| label.as_str())
    }
}

/// A notification's inline reply: KDE's extension to the spec, which messengers use. The app
/// offers it as an `inline-reply` action, and gets the answer back as `NotificationReplied`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The action's label: what to call the button that starts an answer. May be empty.
    pub label: String,
    /// What the field says while empty (`x-kde-reply-placeholder-text`).
    pub placeholder: Option<String>,
    /// What to call the button that sends it (`x-kde-reply-submit-button-text`).
    pub submit: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// How long a notification asked to be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// Whatever the server's default is.
    Default,
    /// Until it's dismissed. Critical notifications always are.
    Never,
    After(Duration),
}

/// Why a notification closed, as the spec numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Expired = 1,
    Dismissed = 2,
    /// The app closed it.
    Closed = 3,
    Undefined = 4,
}

/// What the server reports.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// It owns the name: notifications come here.
    Serving,
    /// Another daemon owns the name: its process name, if it could be found. The server waits
    /// in line, and reports `Serving` once that daemon quits.
    Waiting(Option<String>),
    /// It couldn't start, and why.
    Failed(String),
    /// A new notification, or one that replaces an earlier one with the same id.
    Notified(Box<Notification>),
    /// The app closed one of its notifications.
    Closed(u32),
}

/// Every event from now on, starting with whether the server is serving. Starts the server on
/// first use.
pub fn watch() -> impl Stream<Item = Event> {
    stream::once(async {
        // Subscribed before the status is read, so nothing that arrives in between is lost.
        let events = bus().subscribe();
        let status = match server().await {
            Ok(server) => server.status().await,
            Err(error) => Event::Failed(error.to_string()),
        };
        let rest = stream::unfold(events, |mut events| async move {
            loop {
                match events.recv().await {
                    Ok(event) => return Some((event, events)),
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        eprintln!("eave-notify: missed {missed} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        stream::once(async { status }).chain(rest)
    })
    .flatten()
}

/// Tells the app its notification closed, and why. Call it when a notification leaves the
/// screen for good: dismissed, expired, or cleared.
pub async fn close(id: u32, reason: Reason) -> zbus::Result<()> {
    let server = server().await?;
    server
        .connection
        .emit_signal(
            None::<()>,
            PATH,
            NAME,
            "NotificationClosed",
            &(id, reason as u32),
        )
        .await
}

/// Tells the app the user picked the action `key` on its notification.
pub async fn invoke(id: u32, key: &str) -> zbus::Result<()> {
    let server = server().await?;
    server
        .connection
        .emit_signal(None::<()>, PATH, NAME, "ActionInvoked", &(id, key))
        .await
}

/// Sends the app the answer typed into its notification ([`Reply`]).
pub async fn reply(id: u32, text: &str) -> zbus::Result<()> {
    let server = server().await?;
    server
        .connection
        .emit_signal(None::<()>, PATH, NAME, "NotificationReplied", &(id, text))
        .await
}

/// Where the server's events go, for every watcher.
fn bus() -> &'static broadcast::Sender<Event> {
    static BUS: std::sync::OnceLock<broadcast::Sender<Event>> = std::sync::OnceLock::new();
    BUS.get_or_init(|| broadcast::channel(64).0)
}

struct Server {
    connection: zbus::Connection,
}

impl Server {
    /// Whether it owns the name now, or who does.
    async fn status(&self) -> Event {
        let owner: Option<String> = self.ask("GetNameOwner", NAME).await;
        match (owner, self.connection.unique_name()) {
            (Some(owner), Some(ours)) if owner == ours.as_str() => Event::Serving,
            (Some(owner), _) => Event::Waiting(self.process_name(&owner).await),
            (None, _) => Event::Waiting(None),
        }
    }

    /// The name of the process behind `owner` on the bus, from `/proc`.
    async fn process_name(&self, owner: &str) -> Option<String> {
        let pid: u32 = self.ask("GetConnectionUnixProcessID", owner).await?;
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(name.trim().to_string())
    }

    /// Calls `method` on the bus itself.
    async fn ask<T>(&self, method: &str, name: &str) -> Option<T>
    where
        T: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        bus_call(&self.connection, method, &(name,))
            .await
            .ok()?
            .body()
            .deserialize()
            .ok()
    }
}

/// Calls `method` on the bus itself. zbus's own `request_name` takes flags as a type it doesn't
/// export, and watches for the name only after asking for it.
async fn bus_call<B>(
    connection: &zbus::Connection,
    method: &str,
    body: &B,
) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            method,
            body,
        )
        .await
}

/// The server, started once: the session bus connection serving the interface, in line for the
/// name, and watching for the name to come its way.
async fn server() -> zbus::Result<&'static Server> {
    static SERVER: OnceCell<Server> = OnceCell::const_new();
    SERVER
        .get_or_try_init(|| async {
            let connection = zbus::connection::Builder::session()?
                .serve_at(PATH, Notifications)?
                .build()
                .await?;
            // Watching before asking, so the name can't arrive unseen.
            let rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.freedesktop.DBus")?
                .member("NameAcquired")?
                .arg(0, NAME)?
                .build();
            let mut acquired = MessageStream::for_match_rule(rule, &connection, Some(4)).await?;
            // In line rather than giving up: once another daemon quits, the name is ours. No
            // flags, so a daemon started later can't take it away either.
            let reply: RequestNameReply = bus_call(&connection, "RequestName", &(NAME, 0u32))
                .await?
                .body()
                .deserialize()?;
            if reply == RequestNameReply::InQueue {
                // zbus runs on the bar's tokio runtime, and so does this.
                tokio::spawn(async move {
                    if acquired.next().await.is_some() {
                        let _ = bus().send(Event::Serving);
                    }
                });
            }
            Ok(Server { connection })
        })
        .await
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

struct Notifications;

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<&'static str> {
        vec!["actions", "body", "body-markup", "inline-reply"]
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = match replaces_id {
            0 => NEXT_ID.fetch_add(1, Ordering::Relaxed),
            id => id,
        };
        let (reply, actions) = split_reply(pairs(actions), &hints);
        let notification = Notification {
            id,
            reply,
            icon: icon(&app_icon, &hints),
            app: app_name,
            summary: strip(&summary),
            body: strip(&body),
            actions,
            urgency: urgency(&hints),
            timeout: timeout(expire_timeout, urgency(&hints)),
            received: SystemTime::now(),
        };
        let _ = bus().send(Event::Notified(Box::new(notification)));
        id
    }

    async fn close_notification(&self, id: u32) {
        let _ = bus().send(Event::Closed(id));
    }

    fn get_server_information(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        ("eave", "eave", env!("CARGO_PKG_VERSION"), "1.2")
    }
}

/// An app id for the icon: the `desktop-entry` hint, or the app icon if it's a name rather than
/// a path or URI.
fn icon(app_icon: &str, hints: &HashMap<String, OwnedValue>) -> Option<String> {
    let entry = hints
        .get("desktop-entry")
        .and_then(|value| <&str>::try_from(value).ok())
        .filter(|entry| !entry.is_empty());
    let named = Some(app_icon).filter(|icon| !icon.is_empty() && !icon.contains('/'));
    entry.or(named).map(str::to_string)
}

fn urgency(hints: &HashMap<String, OwnedValue>) -> Urgency {
    match hints
        .get("urgency")
        .and_then(|value| u8::try_from(value).ok())
    {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// The spec's `expire_timeout`: −1 for the server's default, 0 for never, else milliseconds.
/// Critical notifications stay until dismissed, whatever they ask.
fn timeout(expire_timeout: i32, urgency: Urgency) -> Timeout {
    match expire_timeout {
        _ if urgency == Urgency::Critical => Timeout::Never,
        0 => Timeout::Never,
        milliseconds if milliseconds > 0 => {
            Timeout::After(Duration::from_millis(milliseconds as u64))
        }
        _ => Timeout::Default,
    }
}

/// The inline reply out of the actions, if the app offered one.
fn split_reply(
    actions: Vec<(String, String)>,
    hints: &HashMap<String, OwnedValue>,
) -> (Option<Reply>, Vec<(String, String)>) {
    let text = |hint: &str| {
        hints
            .get(hint)
            .and_then(|value| <&str>::try_from(value).ok())
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let (replies, actions): (Vec<_>, Vec<_>) = actions
        .into_iter()
        .partition(|(key, _)| key == "inline-reply");
    let reply = replies.into_iter().next().map(|(_, label)| Reply {
        label,
        placeholder: text("x-kde-reply-placeholder-text"),
        submit: text("x-kde-reply-submit-button-text"),
    });
    (reply, actions)
}

/// The spec's flat list of actions, key then label, as pairs.
fn pairs(actions: Vec<String>) -> Vec<(String, String)> {
    let mut actions = actions.into_iter();
    let mut pairs = Vec::new();
    while let (Some(key), Some(label)) = (actions.next(), actions.next()) {
        pairs.push((key, label));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(pairs: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.try_clone().unwrap()))
            .collect()
    }

    #[test]
    fn timeouts_follow_the_spec_and_critical_stays() {
        assert_eq!(timeout(-1, Urgency::Normal), Timeout::Default);
        assert_eq!(timeout(0, Urgency::Normal), Timeout::Never);
        assert_eq!(
            timeout(3000, Urgency::Low),
            Timeout::After(Duration::from_secs(3))
        );
        assert_eq!(timeout(3000, Urgency::Critical), Timeout::Never);
    }

    #[test]
    fn urgency_comes_from_its_hint() {
        assert_eq!(urgency(&hints(&[])), Urgency::Normal);
        assert_eq!(
            urgency(&hints(&[("urgency", 2u8.into())])),
            Urgency::Critical
        );
        assert_eq!(urgency(&hints(&[("urgency", 0u8.into())])), Urgency::Low);
    }

    #[test]
    fn the_icon_is_an_app_id_never_a_path() {
        let entry = hints(&[(
            "desktop-entry",
            OwnedValue::try_from(zbus::zvariant::Value::from("org.gnome.Nautilus")).unwrap(),
        )]);
        assert_eq!(icon("x", &entry).as_deref(), Some("org.gnome.Nautilus"));
        assert_eq!(icon("firefox", &hints(&[])).as_deref(), Some("firefox"));
        assert_eq!(icon("/tmp/a.png", &hints(&[])), None);
        assert_eq!(icon("file:///tmp/a.png", &hints(&[])), None);
        assert_eq!(icon("", &hints(&[])), None);
    }

    #[test]
    fn an_inline_reply_comes_out_of_the_actions() {
        let actions = pairs(vec![
            "default".into(),
            "Open".into(),
            "inline-reply".into(),
            "Reply".into(),
        ]);
        let hint = |text: &str| OwnedValue::try_from(zbus::zvariant::Value::from(text)).unwrap();
        let hints = hints(&[("x-kde-reply-placeholder-text", hint("Message…"))]);
        let (reply, actions) = split_reply(actions, &hints);
        assert_eq!(
            reply,
            Some(Reply {
                label: "Reply".into(),
                placeholder: Some("Message…".into()),
                submit: None,
            })
        );
        assert_eq!(actions, [("default".to_string(), "Open".to_string())]);
    }

    #[test]
    fn actions_pair_up_and_an_odd_one_out_is_dropped() {
        let actions = pairs(vec!["default".into(), "Open".into(), "x".into()]);
        assert_eq!(actions, [("default".to_string(), "Open".to_string())]);
    }
}
