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
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use futures_util::{Stream, StreamExt, stream};
use tokio::sync::{OnceCell, broadcast};
use zbus::fdo::RequestNameReply;
use zbus::zvariant::{OwnedValue, Value};
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
    /// The app's icon: an app id to find it by, or a file.
    pub icon: Option<Icon>,
    /// A picture of its own: an avatar, a screenshot.
    pub image: Option<Image>,
    pub summary: String,
    /// Plain text, markup stripped and entities decoded. Or Markdown as the app wrote it, if
    /// [`markdown`](Self::markdown).
    pub body: String,
    /// The body is Markdown: the app said so with the `x-eave-markdown` hint.
    pub markdown: bool,
    /// Each action's key and label, in order. The `default` one is what clicking the
    /// notification does. An `inline-reply` one is in [`reply`](Self::reply) instead.
    pub actions: Vec<(String, String)>,
    /// Whether the app takes a typed answer, and how to ask for it.
    pub reply: Option<Reply>,
    pub urgency: Urgency,
    pub timeout: Timeout,
    pub received: SystemTime,
    /// How far along what it's about is, from 0 to 1: the `value` hint, a percentage.
    pub progress: Option<f32>,
    /// The conversation it's part of (`x-eave-thread`), and what to call it
    /// (`x-eave-thread-name`).
    pub thread: Option<String>,
    pub thread_name: Option<String>,
    /// Options to pick from (`x-eave-choices`), instead of or besides actions.
    pub choices: Option<Choices>,
    /// It's question `index` (from 0) of `count`, asked one at a time (`x-eave-step`). Going back
    /// is the `x-eave-back` action.
    pub step: Option<(u32, u32)>,
    /// Not to be kept once it's off the screen (`transient`).
    pub transient: bool,
    /// Stays after an action is picked, until the app or the user closes it (`resident`).
    pub resident: bool,
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

/// Where a notification's app icon comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Icon {
    /// An app id to find it in the icon theme by: the `desktop-entry` hint, or an `app_icon`
    /// (or `image-path`) that names an icon.
    App(String),
    /// A file, as a `file://` URL: the `x-eave-icon` hint. An `app_icon` that is a path is taken
    /// as the notification's picture instead, as `notify-send -i` means it.
    File(String),
}

/// A notification's own picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// A file, as a `file://` URL: the `image-path` hint, or an `app_icon` that is a path.
    File(String),
    /// Pixels the app sent: the `image-data` hint.
    Pixels(Pixels),
}

/// A picture as its pixels: RGBA, 8 bits a channel, row after row with nothing between them.
#[derive(Clone, PartialEq, Eq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

impl std::fmt::Debug for Pixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pixels({}×{})", self.width, self.height)
    }
}

/// Options to pick from, eave's extension to the spec (`x-eave-choices`, `x-eave-multiple`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices {
    pub options: Vec<Choice>,
    /// Several can be picked, and sent together ([`picked`]). Otherwise a pick is an action
    /// ([`invoke`]) with the option's key.
    pub multiple: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub key: String,
    pub label: String,
    /// What it means, if the app said. May be empty.
    pub description: String,
    /// The one the app suggests.
    pub recommended: bool,
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

/// Sends the app the options picked on its notification, by key ([`Choices::multiple`]).
pub async fn picked(id: u32, keys: &[String]) -> zbus::Result<()> {
    let server = server().await?;
    server
        .connection
        .emit_signal(None::<()>, PATH, NAME, "EaveChoicesPicked", &(id, keys))
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
        vec![
            "actions",
            "body",
            "body-markup",
            "icon-static",
            "inline-reply",
            "x-eave-choices",
            "x-eave-icon",
            "x-eave-markdown",
            "x-eave-thread",
        ]
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
        let markdown = flag(&hints, "x-eave-markdown");
        let urgency = urgency(&hints);
        let notification = Notification {
            id,
            reply,
            icon: icon(&app_icon, &hints),
            image: image(&app_icon, &hints),
            app: app_name,
            summary: strip(&summary),
            body: if markdown { body } else { strip(&body) },
            markdown,
            actions,
            urgency,
            timeout: timeout(expire_timeout, urgency),
            received: SystemTime::now(),
            progress: progress(&hints),
            thread: text(&hints, "x-eave-thread"),
            thread_name: text(&hints, "x-eave-thread-name"),
            choices: choices(&hints),
            step: step(&hints),
            transient: flag(&hints, "transient"),
            resident: flag(&hints, "resident"),
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

/// A text hint, if it's there and not empty.
fn text(hints: &HashMap<String, OwnedValue>, hint: &str) -> Option<String> {
    hints
        .get(hint)
        .and_then(|value| <&str>::try_from(value).ok())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// `path` as a `file://` URL, if it's a path.
fn file(path: &str) -> Option<String> {
    if path.starts_with("file://") {
        Some(path.to_string())
    } else if path.starts_with('/') {
        Some(format!("file://{path}"))
    } else {
        None
    }
}

/// The app's icon: a file eave was given for it (`x-eave-icon`), else an app id (the
/// `desktop-entry` hint, or its icon's name).
fn icon(app_icon: &str, hints: &HashMap<String, OwnedValue>) -> Option<Icon> {
    if let Some(url) = text(hints, "x-eave-icon").as_deref().and_then(file) {
        return Some(Icon::File(url));
    }
    // An image-path may also name an icon, as app_icon can.
    let named = |name: &str| (!name.is_empty() && !name.contains('/')).then(|| name.to_string());
    text(hints, "desktop-entry")
        .or_else(|| named(app_icon))
        .or_else(|| text(hints, "image-path").as_deref().and_then(named))
        .map(Icon::App)
}

/// Its own picture, in the spec's order: pixels it sent, a file it named, a path as its icon.
fn image(app_icon: &str, hints: &HashMap<String, OwnedValue>) -> Option<Image> {
    let pixels = ["image-data", "image_data", "icon_data"]
        .iter()
        .find_map(|hint| pixels(hints.get(*hint)?));
    if let Some(pixels) = pixels {
        return Some(Image::Pixels(pixels));
    }
    text(hints, "image-path")
        .or_else(|| text(hints, "image_path"))
        .as_deref()
        .and_then(file)
        .or_else(|| file(app_icon))
        .map(Image::File)
}

/// The largest side of a picture taken as pixels, so a hostile one can't take all the memory.
const MAX_SIDE: i32 = 4096;

/// The spec's `(iiibiiay)`: width, height, bytes a row, alpha or not, bits a sample (8), samples a
/// pixel (3 or 4), then the rows. Made RGBA with nothing between rows. `None` if it doesn't add up.
fn pixels(value: &OwnedValue) -> Option<Pixels> {
    let structure = match &**value {
        Value::Structure(structure) => structure,
        _ => return None,
    };
    let fields = structure.fields();
    let int = |at: usize| match fields.get(at)? {
        Value::I32(n) => Some(*n),
        _ => None,
    };
    let (width, height, stride) = (int(0)?, int(1)?, int(2)?);
    let alpha = match fields.get(3)? {
        Value::Bool(alpha) => *alpha,
        _ => return None,
    };
    let (bits, channels) = (int(4)?, int(5)?);
    let data: Vec<u8> = match fields.get(6)? {
        Value::Array(array) => array
            .iter()
            .map(|byte| match byte {
                Value::U8(byte) => Some(*byte),
                _ => None,
            })
            .collect::<Option<_>>()?,
        _ => return None,
    };
    let sized = (1..=MAX_SIDE).contains(&width) && (1..=MAX_SIDE).contains(&height);
    if !sized || bits != 8 || channels != if alpha { 4 } else { 3 } {
        return None;
    }
    let (width, height, stride, channels) = (
        width as usize,
        height as usize,
        stride as usize,
        channels as usize,
    );
    // The last row may stop where its pixels do.
    let needed = stride * (height - 1) + width * channels;
    if stride < width * channels || data.len() < needed {
        return None;
    }
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in data.chunks(stride).take(height) {
        for pixel in row[..width * channels].chunks_exact(channels) {
            rgba.extend_from_slice(&pixel[..3]);
            rgba.push(if alpha { pixel[3] } else { 255 });
        }
    }
    Some(Pixels {
        width: width as u32,
        height: height as u32,
        rgba: rgba.into(),
    })
}

/// The `value` hint, a percentage, as a fraction.
fn progress(hints: &HashMap<String, OwnedValue>) -> Option<f32> {
    let value = hints.get("value")?;
    let percent = i64::try_from(value)
        .ok()
        .or_else(|| i32::try_from(value).ok().map(i64::from))
        .or_else(|| u32::try_from(value).ok().map(i64::from))
        .or_else(|| u8::try_from(value).ok().map(i64::from))?;
    Some(percent.clamp(0, 100) as f32 / 100.0)
}

/// `x-eave-choices`, `a(sssb)`: each option's key, label, description, and whether it's the one
/// suggested. `x-eave-multiple` says several can be picked.
fn choices(hints: &HashMap<String, OwnedValue>) -> Option<Choices> {
    let array = match &**hints.get("x-eave-choices")? {
        Value::Array(array) => array,
        _ => return None,
    };
    let options: Vec<Choice> = array
        .iter()
        .filter_map(|option| {
            let Value::Structure(option) = option else {
                return None;
            };
            let text = |at: usize| match option.fields().get(at)? {
                Value::Str(text) => Some(text.to_string()),
                _ => None,
            };
            let recommended = matches!(option.fields().get(3), Some(Value::Bool(true)));
            let (key, label) = (text(0)?, text(1)?);
            (!key.is_empty() && !label.is_empty()).then(|| Choice {
                key,
                label,
                description: text(2).unwrap_or_default(),
                recommended,
            })
        })
        .collect();
    (!options.is_empty()).then(|| Choices {
        options,
        multiple: flag(hints, "x-eave-multiple"),
    })
}

/// `x-eave-step`, `(uu)`: which question of how many.
fn step(hints: &HashMap<String, OwnedValue>) -> Option<(u32, u32)> {
    let Value::Structure(step) = &**hints.get("x-eave-step")? else {
        return None;
    };
    match step.fields() {
        [Value::U32(index), Value::U32(count)] if index < count => Some((*index, *count)),
        _ => None,
    }
}

/// A boolean hint, false if it's missing or not a boolean.
fn flag(hints: &HashMap<String, OwnedValue>, hint: &str) -> bool {
    hints
        .get(hint)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
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
    let text = |hint: &str| text(hints, hint);
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

    fn text(text: &str) -> OwnedValue {
        OwnedValue::try_from(zbus::zvariant::Value::from(text)).unwrap()
    }

    #[test]
    fn the_icon_is_an_app_id_or_a_file() {
        let entry = hints(&[("desktop-entry", text("org.gnome.Nautilus"))]);
        let app = |id: &str| Some(Icon::App(id.into()));
        assert_eq!(icon("x", &entry), app("org.gnome.Nautilus"));
        assert_eq!(icon("firefox", &hints(&[])), app("firefox"));
        assert_eq!(icon("/tmp/a.png", &hints(&[])), None, "a path is a picture");
        assert_eq!(icon("", &hints(&[])), None);
        assert_eq!(
            icon("", &hints(&[("image-path", text("mail"))])),
            app("mail")
        );
        let file = hints(&[
            ("desktop-entry", text("claude")),
            ("x-eave-icon", text("/run/claude.png")),
        ]);
        assert_eq!(
            icon("", &file),
            Some(Icon::File("file:///run/claude.png".into()))
        );
    }

    #[test]
    fn the_picture_is_pixels_then_a_named_file_then_a_path_as_icon() {
        let file = |url: &str| Some(Image::File(url.into()));
        assert_eq!(image("/tmp/a.png", &hints(&[])), file("file:///tmp/a.png"));
        assert_eq!(
            image("file:///tmp/a.png", &hints(&[])),
            file("file:///tmp/a.png")
        );
        assert_eq!(image("firefox", &hints(&[])), None);
        let named = hints(&[("image-path", text("/run/shot.png"))]);
        assert_eq!(image("/tmp/a.png", &named), file("file:///run/shot.png"));
        let both = hints(&[
            ("image-path", text("/run/shot.png")),
            ("image-data", data(1, 1, 3, false, vec![1, 2, 3])),
        ]);
        assert!(matches!(image("", &both), Some(Image::Pixels(_))));
    }

    fn data(width: i32, height: i32, stride: i32, alpha: bool, bytes: Vec<u8>) -> OwnedValue {
        let channels = if alpha { 4 } else { 3 };
        OwnedValue::try_from(Value::from((
            width, height, stride, alpha, 8i32, channels, bytes,
        )))
        .unwrap()
    }

    #[test]
    fn pixels_are_made_rgba_row_by_row() {
        // Two pixels a row, with a byte of padding after each row but the last.
        let rgb = data(
            2,
            2,
            7,
            false,
            vec![1, 2, 3, 4, 5, 6, 0, 7, 8, 9, 10, 11, 12],
        );
        let pixels = pixels(&rgb).expect("pixels");
        assert_eq!((pixels.width, pixels.height), (2, 2));
        assert_eq!(
            &*pixels.rgba,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
        let rgba = data(1, 1, 4, true, vec![1, 2, 3, 4]);
        assert_eq!(&*pixels_of(&rgba), [1, 2, 3, 4]);
    }

    fn pixels_of(value: &OwnedValue) -> Arc<[u8]> {
        pixels(value).unwrap().rgba
    }

    #[test]
    fn pixels_that_dont_add_up_are_dropped() {
        assert!(
            pixels(&data(2, 2, 6, false, vec![0; 11])).is_none(),
            "short"
        );
        assert!(
            pixels(&data(2, 2, 5, false, vec![0; 12])).is_none(),
            "rows overlap"
        );
        assert!(pixels(&data(0, 2, 6, false, Vec::new())).is_none(), "empty");
        assert!(
            pixels(&data(5000, 1, 15000, false, vec![0; 15000])).is_none(),
            "huge"
        );
        assert!(pixels(&text("no")).is_none());
    }

    #[test]
    fn progress_is_a_clamped_percentage() {
        assert_eq!(progress(&hints(&[("value", 40i32.into())])), Some(0.4));
        assert_eq!(progress(&hints(&[("value", 140u32.into())])), Some(1.0));
        assert_eq!(progress(&hints(&[])), None);
    }

    #[test]
    fn choices_and_steps_come_from_eave_s_hints() {
        let options = vec![
            ("a", "Apple", "Sweet", true),
            ("p", "Pear", "", false),
            ("", "Nothing", "", false),
        ];
        let hints = hints(&[
            (
                "x-eave-choices",
                OwnedValue::try_from(Value::from(options)).unwrap(),
            ),
            ("x-eave-multiple", true.into()),
            (
                "x-eave-step",
                OwnedValue::try_from(Value::from((1u32, 3u32))).unwrap(),
            ),
        ]);
        let choices = choices(&hints).expect("choices");
        assert!(choices.multiple);
        let keys: Vec<_> = choices.options.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["a", "p"], "one without a key is dropped");
        assert!(choices.options[0].recommended);
        assert_eq!(choices.options[0].description, "Sweet");
        assert_eq!(step(&hints), Some((1, 3)));
    }

    #[test]
    fn markdown_is_a_flag() {
        assert!(flag(
            &hints(&[("x-eave-markdown", true.into())]),
            "x-eave-markdown"
        ));
        assert!(!flag(
            &hints(&[("x-eave-markdown", text("yes"))]),
            "x-eave-markdown"
        ));
        assert!(!flag(&hints(&[]), "x-eave-markdown"));
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
