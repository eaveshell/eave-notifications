//! Notifications, through eave's own notification server ([`eave_notify`]). While there are any,
//! the right island turns into a panel of them. Each goes away after a few seconds, when
//! dismissed, or when one of its actions is picked. The bar shows a bell, with a count of the ones that arrived since the
//! popover was last opened. The popover lists the latest ones. Right click on the bell for do not
//! disturb: notifications only go to the list, except critical ones.
//!
//! If another notification daemon is running, it keeps the notifications, and the bell is dimmed
//! until it quits.

use std::time::SystemTime;

use eave_api::time::{Duration, every};
use eave_api::ui::{self, Icon, Item, Notice, Panel, Tone, Urgency};
use eave_api::{Effect, Nudge, Registry, Subscription, Task, Widget};
use eave_notify::{Event, Notification, Reason, Timeout};
use serde::Deserialize;

pub fn register(registry: &mut Registry) {
    registry.widget::<Notifications>("notifications");
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seconds a notification shows, for notifications that don't say.
    timeout: u64,
    /// How many notifications the popover's History tab keeps, and its first tab at most.
    history: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: 5,
            history: 100,
        }
    }
}

/// Whether the server gets the notifications.
#[derive(Debug, Clone, PartialEq)]
enum Status {
    Starting,
    Serving,
    /// Another daemon has them: its name, if known.
    Waiting(Option<String>),
    Failed(String),
}

pub struct Notifications {
    config: Config,
    status: Status,
    /// The notifications you haven't seen in the popover yet, newest first: its first tab.
    inbox: Vec<Notification>,
    /// Every notification that arrived, newest first, whatever became of it: the History tab.
    past: Vec<Notification>,
    /// The ones shown in the island, newest first.
    shown: Vec<u32>,
    /// The ones that arrived since the popover was last opened, and that you haven't dealt with
    /// yourself: closed, clicked, or picked an action of. Ones that expired still count.
    unread: Vec<u32>,
    quiet: bool,
    open: bool,
    /// The one the popover's list shows all of, with its actions.
    expanded: Option<u32>,
    /// The one the History tab shows all of.
    expanded_past: Option<u32>,
    /// The one being answered, and the answer so far.
    answering: Option<(u32, String)>,
    /// The time the popover's "ago"s are counted from.
    now: SystemTime,
}

#[derive(Debug, Clone)]
pub enum Message {
    Server(Event),
    /// One has shown long enough.
    Expired(u32),
    /// Closed by its close button.
    Dismiss(u32),
    /// One of its actions, by key.
    Action(u32, String),
    /// A click on it in the island: its default action, or else dismissing it.
    Press(u32),
    /// A click on it in the popover's list: shows all of it, and its actions, or hides them again.
    Expand(u32),
    /// The same, in the History tab.
    ExpandPast(u32),
    /// Its Reply button: shows a field to type the answer in.
    StartReply(u32),
    /// The answer as typed so far.
    Draft(String),
    SendReply,
    CancelReply,
    Clear,
    ToggleQuiet,
    Opened,
    Closed,
    Tick,
}

impl Widget for Notifications {
    type Config = Config;
    type Message = Message;

    fn new(config: Config) -> (Self, Task<Message>) {
        let notifications = Self {
            config,
            status: Status::Starting,
            inbox: Vec::new(),
            past: Vec::new(),
            shown: Vec::new(),
            unread: Vec::new(),
            quiet: false,
            open: false,
            expanded: None,
            expanded_past: None,
            answering: None,
            now: SystemTime::now(),
        };
        (notifications, Task::none())
    }

    fn update(&mut self, message: Message) -> Effect<Message> {
        match message {
            Message::Server(event) => self.heard(event),
            Message::Expired(id) => {
                // Off the screen, still in the list.
                self.shown.retain(|shown| *shown != id);
                close(id, Reason::Expired)
            }
            Message::Dismiss(id) => {
                self.forget(id);
                close(id, Reason::Dismissed)
            }
            Message::Action(id, key) => {
                self.forget(id);
                Effect::task(
                    Task::future(async move {
                        if let Err(error) = eave_notify::invoke(id, &key).await {
                            eprintln!("eave-notifications: {error}");
                        }
                        if let Err(error) = eave_notify::close(id, Reason::Dismissed).await {
                            eprintln!("eave-notifications: {error}");
                        }
                    })
                    .discard(),
                )
                .close_popover()
            }
            Message::Press(id) => {
                let default = self
                    .inbox
                    .iter()
                    .find(|notification| notification.id == id)
                    .and_then(Notification::default_action);
                match default {
                    Some(_) => self.update(Message::Action(id, "default".into())),
                    None => self.update(Message::Dismiss(id)),
                }
            }
            Message::StartReply(id) => {
                self.answering = Some((id, String::new()));
                Effect::none()
            }
            Message::Draft(text) => {
                if let Some((_, draft)) = &mut self.answering {
                    *draft = text;
                }
                Effect::none()
            }
            Message::CancelReply => {
                self.answering = None;
                Effect::none()
            }
            Message::SendReply => {
                let Some((id, text)) = self.answering.take() else {
                    return Effect::none();
                };
                if text.trim().is_empty() {
                    self.answering = Some((id, text));
                    return Effect::none();
                }
                self.forget(id);
                Effect::task(
                    Task::future(async move {
                        if let Err(error) = eave_notify::reply(id, &text).await {
                            eprintln!("eave-notifications: {error}");
                        }
                        if let Err(error) = eave_notify::close(id, Reason::Dismissed).await {
                            eprintln!("eave-notifications: {error}");
                        }
                    })
                    .discard(),
                )
                .close_popover()
            }
            Message::Expand(id) => {
                self.expanded = (self.expanded != Some(id)).then_some(id);
                Effect::none()
            }
            Message::ExpandPast(id) => {
                self.expanded_past = (self.expanded_past != Some(id)).then_some(id);
                Effect::none()
            }
            Message::Clear => self.clear(),
            Message::ToggleQuiet => {
                self.quiet = !self.quiet;
                if self.quiet {
                    let inbox = &self.inbox;
                    self.shown.retain(|id| {
                        inbox
                            .iter()
                            .any(|n| n.id == *id && n.urgency == eave_notify::Urgency::Critical)
                    });
                }
                Effect::none()
            }
            Message::Opened => {
                self.open = true;
                self.unread.clear();
                self.now = SystemTime::now();
                Effect::none()
            }
            Message::Closed => {
                self.open = false;
                self.expanded = None;
                self.expanded_past = None;
                self.answering = None;
                // Seen, so read: they go on to the history, and the next open starts empty.
                self.clear()
            }
            Message::Tick => {
                self.now = SystemTime::now();
                Effect::none()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let server = Subscription::run(eave_notify::watch).map(Message::Server);
        if !self.open {
            return server;
        }
        // Keeps the "ago"s current while they show.
        let tick = every(Duration::from_secs(30)).map(|_| Message::Tick);
        Subscription::batch([server, tick])
    }

    fn item(&self) -> Item<Message> {
        let bell = ui::icon(Icon::Bell { muted: self.quiet });
        let content = match self.unread.len() {
            0 => bell,
            unread => ui::row([bell, ui::counter(unread.min(99) as i64, 0..=99, "")]),
        };
        let content = if self.status == Status::Serving {
            content
        } else {
            content.tone(Tone::Dim)
        };
        Item::new(content).on_secondary(Message::ToggleQuiet)
    }

    fn popover(&self) -> Option<Panel<Message>> {
        let mut header = ui::header("Notifications");
        if !self.inbox.is_empty() {
            header = header.trailing(ui::button(ui::label("Clear"), Message::Clear));
        }
        let status = match &self.status {
            Status::Starting | Status::Serving => None,
            Status::Waiting(Some(owner)) => Some(format!(
                "{owner} is showing notifications. Quit it to have eave show them."
            )),
            Status::Waiting(None) => Some(
                "Another notification daemon is running. Quit it to have eave show them.".into(),
            ),
            Status::Failed(error) => Some(format!("Can't receive notifications: {error}")),
        };
        let quiet = ui::row([
            ui::label("Do not disturb"),
            ui::fill(),
            ui::toggle(self.quiet, Message::ToggleQuiet),
        ]);
        let list = if self.inbox.is_empty() {
            ui::label("No notifications").tone(Tone::Dim)
        } else {
            ui::list(self.inbox.iter().map(|notification| self.row(notification)))
        };
        let past = if self.past.is_empty() {
            ui::label("Nothing yet").tone(Tone::Dim)
        } else {
            ui::list(
                self.past
                    .iter()
                    .map(|notification| self.past_row(notification)),
            )
        };
        let tabs = ui::tabs([("New", list), ("History", past)]);
        let content = ui::column(
            [
                Some(header),
                status.map(|text| ui::label(text).tone(Tone::Warning)),
            ]
            .into_iter()
            .flatten()
            .chain([quiet, ui::separator(), tabs]),
        );
        Some(
            Panel::new(content)
                .on_open(Message::Opened)
                .on_close(Message::Closed),
        )
    }

    fn notices(&self) -> Vec<Notice<Message>> {
        self.shown
            .iter()
            .filter_map(|id| self.inbox.iter().find(|n| n.id == *id))
            .map(|notification| self.notice(notification))
            .collect()
    }
}

impl Notifications {
    fn heard(&mut self, event: Event) -> Effect<Message> {
        match event {
            Event::Serving => self.status = Status::Serving,
            Event::Waiting(owner) => self.status = Status::Waiting(owner),
            Event::Failed(error) => {
                eprintln!("eave-notifications: {error}");
                self.status = Status::Failed(error);
            }
            Event::Closed(id) => {
                self.forget(id);
                return close(id, Reason::Closed);
            }
            Event::Notified(notification) => return self.arrived(*notification),
        }
        Effect::none()
    }

    fn arrived(&mut self, notification: Notification) -> Effect<Message> {
        let id = notification.id;
        let critical = notification.urgency == eave_notify::Urgency::Critical;
        // The log keeps everything, a replacement in its old one's place.
        match self.past.iter_mut().find(|n| n.id == id) {
            Some(old) => *old = notification.clone(),
            None => self.past.insert(0, notification.clone()),
        }
        self.past.truncate(self.config.history);
        // A replacement takes the old one's place, and isn't news.
        let replaced = match self.inbox.iter_mut().find(|n| n.id == id) {
            Some(old) => {
                *old = notification;
                true
            }
            None => {
                self.inbox.insert(0, notification);
                false
            }
        };
        for gone in self
            .inbox
            .split_off(self.inbox.len().min(self.config.history))
        {
            self.shown.retain(|id| *id != gone.id);
            self.unread.retain(|id| *id != gone.id);
        }
        if !replaced && !self.open {
            self.unread.push(id);
        }
        if self.quiet && !critical {
            // Not shown, so the bell says it instead.
            return if replaced {
                Effect::none()
            } else {
                Effect::none().nudge(Nudge::Down)
            };
        }
        if !self.shown.contains(&id) {
            self.shown.insert(0, id);
        }
        Effect::none()
    }

    /// Takes everything out of the list and off the screen. The history keeps them.
    fn clear(&mut self) -> Effect<Message> {
        let ids: Vec<_> = self.inbox.drain(..).map(|n| n.id).collect();
        self.shown.clear();
        Effect::task(Task::batch(ids.into_iter().map(|id| {
            Task::future(async move {
                let _ = eave_notify::close(id, Reason::Dismissed).await;
            })
            .discard()
        })))
    }

    /// Off the screen and out of the list, and no longer news.
    fn forget(&mut self, id: u32) {
        self.shown.retain(|shown| *shown != id);
        self.inbox.retain(|notification| notification.id != id);
        self.unread.retain(|unread| *unread != id);
        if self
            .answering
            .as_ref()
            .is_some_and(|(answering, _)| *answering == id)
        {
            self.answering = None;
        }
    }

    fn notice(&self, notification: &Notification) -> Notice<Message> {
        let id = notification.id;
        let mut notice = Notice::new(id.into(), &notification.summary)
            .urgency(match notification.urgency {
                eave_notify::Urgency::Low => Urgency::Low,
                eave_notify::Urgency::Normal => Urgency::Normal,
                eave_notify::Urgency::Critical => Urgency::Critical,
            })
            .on_press(Message::Press(id))
            .on_dismiss(Message::Dismiss(id));
        if !notification.app.is_empty() {
            notice = notice.app(&notification.app);
        }
        if let Some(icon) = icon(notification) {
            notice = notice.app_icon(icon);
        }
        if !notification.body.is_empty() {
            notice = notice.body(&notification.body);
        }
        // Answering it: a field. Otherwise its Reply button comes first: answering is what a
        // message is for.
        match (&notification.reply, self.draft(id)) {
            (Some(reply), Some(draft)) => notice = notice.reply(reply_field(reply, draft)),
            (Some(reply), None) => {
                notice = notice.action(reply_label(reply), Message::StartReply(id));
            }
            (None, _) => {}
        }
        for (key, label) in &notification.actions {
            // The default action is a click on the notification, not a button.
            if key != "default" {
                notice = notice.action(label, Message::Action(id, key.clone()));
            }
        }
        let after = match notification.timeout {
            Timeout::Default => Some(Duration::from_secs(self.config.timeout.max(1))),
            Timeout::After(after) => Some(after),
            Timeout::Never => None,
        };
        match after {
            Some(after) => notice.expires(after, Message::Expired(id)),
            None => notice,
        }
    }

    fn row(&self, notification: &Notification) -> ui::Node<Message> {
        let id = notification.id;
        let leading = match icon(notification) {
            Some(icon) => ui::app_icon(icon),
            None => ui::icon(Icon::Bell { muted: false }),
        };
        let subtitle = match notification.body.lines().next() {
            Some(line) if !line.is_empty() => line.to_string(),
            _ => notification.app.clone(),
        };
        let trailing = ui::row([
            ui::label(ago(notification.received, self.now)),
            ui::button(ui::icon(Icon::Close), Message::Dismiss(id)),
        ]);
        let row = ui::list_item(&notification.summary)
            .subtitle(subtitle)
            .leading(leading)
            .trailing(trailing)
            .on_press(Message::Expand(id));
        if self.expanded != Some(id) {
            return row;
        }
        // All of the body, then a button per action, the default one first and main.
        let body =
            (!notification.body.is_empty()).then(|| ui::label(&notification.body).tone(Tone::Dim));
        let mut actions: Vec<_> = notification.actions.iter().collect();
        actions.sort_by_key(|(key, _)| key != "default");
        let buttons = actions.into_iter().map(|(key, label)| {
            let label = match label.as_str() {
                "" | "default" => "Open",
                label => label,
            };
            let button = ui::button(ui::label(label), Message::Action(id, key.clone()));
            if key == "default" {
                button.prominent()
            } else {
                button
            }
        });
        // Answering it: a field. Otherwise its Reply button comes first.
        let (field, reply) = match (&notification.reply, self.draft(id)) {
            (Some(reply), Some(draft)) => {
                let field = ui::text_input(
                    reply.placeholder.as_deref().unwrap_or("Reply"),
                    draft,
                    Message::Draft,
                )
                .on_submit(Message::SendReply)
                .trailing(ui::button(ui::icon(Icon::Forward), Message::SendReply));
                (Some(field), None)
            }
            (Some(reply), None) => (
                None,
                Some(ui::button(
                    ui::label(reply_label(reply)),
                    Message::StartReply(id),
                )),
            ),
            (None, _) => (None, None),
        };
        let buttons: Vec<_> = reply.into_iter().chain(buttons).collect();
        let buttons =
            (!buttons.is_empty()).then(|| ui::row(std::iter::once(ui::fill()).chain(buttons)));
        ui::column([Some(row), body, field, buttons].into_iter().flatten())
    }

    /// The answer typed so far into `id`, if it's being answered.
    fn draft(&self, id: u32) -> Option<&str> {
        self.answering
            .as_ref()
            .filter(|(answering, _)| *answering == id)
            .map(|(_, draft)| draft.as_str())
    }
}

impl Notifications {
    /// A notification in the History tab: what it said and when. A click shows all of it. Its
    /// actions are gone: the app was told it closed.
    fn past_row(&self, notification: &Notification) -> ui::Node<Message> {
        let id = notification.id;
        let leading = match icon(notification) {
            Some(icon) => ui::app_icon(icon),
            None => ui::icon(Icon::Bell { muted: false }),
        };
        let expanded = self.expanded_past == Some(id);
        let subtitle = match notification.body.lines().next() {
            Some(line) if !line.is_empty() && !expanded => line.to_string(),
            _ => notification.app.clone(),
        };
        let row = ui::list_item(&notification.summary)
            .subtitle(subtitle)
            .leading(leading)
            .trailing(ui::label(ago(notification.received, self.now)))
            .on_press(Message::ExpandPast(id));
        if !expanded || notification.body.is_empty() {
            return row;
        }
        ui::column([row, ui::label(&notification.body).tone(Tone::Dim)])
    }
}

/// What to call the button that starts an answer.
fn reply_label(reply: &eave_notify::Reply) -> &str {
    match reply.label.as_str() {
        "" | "inline-reply" => "Reply",
        label => label,
    }
}

/// The field to type an answer into, on a notification in the island.
fn reply_field(reply: &eave_notify::Reply, draft: &str) -> ui::Reply<Message> {
    ui::Reply::new(
        reply.placeholder.as_deref().unwrap_or("Reply"),
        draft,
        Message::Draft,
    )
    .on_submit(Message::SendReply)
    .on_cancel(Message::CancelReply)
}

/// The app id to show the icon of: the one it gave, or its name.
fn icon(notification: &Notification) -> Option<String> {
    notification
        .icon
        .clone()
        .or_else(|| Some(notification.app.to_lowercase()).filter(|app| !app.is_empty()))
}

/// Tells the app its notification is gone.
fn close(id: u32, reason: Reason) -> Effect<Message> {
    Effect::task(
        Task::future(async move {
            if let Err(error) = eave_notify::close(id, reason).await {
                eprintln!("eave-notifications: {error}");
            }
        })
        .discard(),
    )
}

/// How long ago `then` was, shortly: "now", "4 min", "2 h", "3 d".
fn ago(then: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(then).unwrap_or_default().as_secs();
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{} min", seconds / 60),
        3600..86400 => format!("{} h", seconds / 3600),
        _ => format!("{} d", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notifications() -> Notifications {
        Notifications::new(Config::default()).0
    }

    fn notification(id: u32, urgency: eave_notify::Urgency) -> Notification {
        Notification {
            id,
            app: "App".into(),
            icon: None,
            summary: format!("summary {id}"),
            body: String::new(),
            actions: vec![
                ("default".into(), "Open".into()),
                ("reply".into(), "Reply".into()),
            ],
            reply: None,
            urgency,
            timeout: Timeout::Default,
            received: SystemTime::UNIX_EPOCH,
        }
    }

    fn arrive(widget: &mut Notifications, id: u32) {
        let notification = notification(id, eave_notify::Urgency::Normal);
        let _ = widget.update(Message::Server(Event::Notified(Box::new(notification))));
    }

    #[test]
    fn ago_is_short() {
        let then = SystemTime::UNIX_EPOCH;
        let after = |seconds| then + Duration::from_secs(seconds);
        assert_eq!(ago(then, after(5)), "now");
        assert_eq!(ago(then, after(4 * 60 + 5)), "4 min");
        assert_eq!(ago(then, after(2 * 3600)), "2 h");
        assert_eq!(ago(then, after(3 * 86400)), "3 d");
        // A clock that went back.
        assert_eq!(ago(after(10), then), "now");
    }

    #[test]
    fn arrivals_show_and_stay_in_the_list_after_expiring() {
        let mut widget = notifications();
        arrive(&mut widget, 1);
        arrive(&mut widget, 2);
        let keys: Vec<_> = widget.notices().iter().map(Notice::key).collect();
        assert_eq!(keys, [2, 1]);
        assert_eq!(widget.unread.len(), 2);

        let _ = widget.update(Message::Expired(2));
        let keys: Vec<_> = widget.notices().iter().map(Notice::key).collect();
        assert_eq!(keys, [1]);
        assert_eq!(widget.inbox.len(), 2);

        let _ = widget.update(Message::Dismiss(1));
        assert!(widget.notices().is_empty());
        assert_eq!(widget.inbox.len(), 1);
    }

    #[test]
    fn a_replacement_updates_in_place_without_counting() {
        let mut widget = notifications();
        arrive(&mut widget, 1);
        arrive(&mut widget, 2);
        let mut newer = notification(1, eave_notify::Urgency::Normal);
        newer.summary = "changed".into();
        let _ = widget.update(Message::Server(Event::Notified(Box::new(newer))));
        assert_eq!(widget.inbox.len(), 2);
        assert_eq!(widget.inbox[1].summary, "changed");
        assert_eq!(widget.unread.len(), 2);
    }

    #[test]
    fn quiet_keeps_them_away_except_critical_ones() {
        let mut widget = notifications();
        let _ = widget.update(Message::ToggleQuiet);
        arrive(&mut widget, 1);
        assert!(widget.notices().is_empty());
        assert_eq!(widget.inbox.len(), 1);
        let critical = notification(2, eave_notify::Urgency::Critical);
        let _ = widget.update(Message::Server(Event::Notified(Box::new(critical))));
        let keys: Vec<_> = widget.notices().iter().map(Notice::key).collect();
        assert_eq!(keys, [2]);
    }

    #[test]
    fn only_notifications_left_alone_count_as_new() {
        let mut widget = notifications();
        for id in 1..=4 {
            arrive(&mut widget, id);
        }
        // Dealt with: closed, picked an action of, clicked.
        let _ = widget.update(Message::Dismiss(1));
        let _ = widget.update(Message::Action(2, "reply".into()));
        let _ = widget.update(Message::Press(3));
        // Left alone until it went by itself.
        let _ = widget.update(Message::Expired(4));
        assert_eq!(widget.unread, [4]);
    }

    #[test]
    fn a_click_in_the_list_expands_and_collapses_without_removing() {
        let mut widget = notifications();
        arrive(&mut widget, 1);
        arrive(&mut widget, 2);
        let _ = widget.update(Message::Expand(1));
        assert_eq!(widget.expanded, Some(1));
        let _ = widget.update(Message::Expand(2));
        assert_eq!(widget.expanded, Some(2), "one at a time");
        let _ = widget.update(Message::Expand(2));
        assert_eq!(widget.expanded, None);
        assert_eq!(widget.inbox.len(), 2);
    }

    #[test]
    fn history_keeps_everything_that_arrived_whatever_became_of_it() {
        let mut widget = Notifications::new(Config {
            history: 3,
            ..Config::default()
        })
        .0;
        for id in 1..=4 {
            arrive(&mut widget, id);
        }
        let _ = widget.update(Message::Dismiss(4));
        let _ = widget.update(Message::Action(3, "reply".into()));
        let _ = widget.update(Message::Server(Event::Closed(2)));
        let _ = widget.update(Message::Clear);
        assert!(widget.inbox.is_empty());
        let past: Vec<_> = widget.past.iter().map(|n| n.id).collect();
        assert_eq!(
            past,
            [4, 3, 2],
            "newest first, the oldest past its length gone"
        );

        // A replacement updates its entry instead of adding one.
        let mut newer = notification(3, eave_notify::Urgency::Normal);
        newer.summary = "changed".into();
        let _ = widget.update(Message::Server(Event::Notified(Box::new(newer))));
        assert_eq!(widget.past.len(), 3);
        assert_eq!(widget.past[1].summary, "changed");
    }

    #[test]
    fn answering_shows_a_field_and_sending_takes_the_notification_away() {
        let mut widget = notifications();
        let mut message = notification(1, eave_notify::Urgency::Normal);
        message.reply = Some(eave_notify::Reply {
            label: "Reply".into(),
            placeholder: None,
            submit: None,
        });
        let _ = widget.update(Message::Server(Event::Notified(Box::new(message))));
        let field = |widget: &Notifications| {
            widget
                .notices()
                .into_iter()
                .next()
                .unwrap()
                .into_parts()
                .reply
                .is_some()
        };
        assert!(!field(&widget), "only once asked");

        let _ = widget.update(Message::StartReply(1));
        assert!(field(&widget));
        // Nothing typed: nothing sent.
        let _ = widget.update(Message::SendReply);
        assert!(field(&widget));

        let _ = widget.update(Message::Draft("on my way".into()));
        let _ = widget.update(Message::SendReply);
        assert!(widget.notices().is_empty());
        assert!(widget.inbox.is_empty());
        assert_eq!(widget.past.len(), 1, "still in the history");
    }

    #[test]
    fn opening_the_popover_reads_everything() {
        let mut widget = notifications();
        arrive(&mut widget, 1);
        let _ = widget.update(Message::Opened);
        assert!(widget.unread.is_empty());
        arrive(&mut widget, 2);
        assert!(widget.unread.is_empty(), "seen while open");
    }

    #[test]
    fn closing_the_popover_hands_new_over_to_history() {
        let mut widget = notifications();
        arrive(&mut widget, 1);
        let _ = widget.update(Message::Opened);
        assert_eq!(widget.inbox.len(), 1, "still new while it's open");
        arrive(&mut widget, 2);
        let _ = widget.update(Message::Closed);
        assert!(widget.inbox.is_empty());
        assert!(widget.notices().is_empty());
        let past: Vec<_> = widget.past.iter().map(|n| n.id).collect();
        assert_eq!(past, [2, 1]);
    }

    #[test]
    fn the_list_keeps_only_the_latest() {
        let mut widget = Notifications::new(Config {
            history: 2,
            ..Config::default()
        })
        .0;
        for id in 1..=3 {
            arrive(&mut widget, id);
        }
        let ids: Vec<_> = widget.inbox.iter().map(|n| n.id).collect();
        assert_eq!(ids, [3, 2]);
        assert!(!widget.shown.contains(&1), "its card went with it");
    }

    #[test]
    fn the_app_closing_one_takes_it_away() {
        let mut widget = notifications();
        arrive(&mut widget, 1);
        let _ = widget.update(Message::Server(Event::Closed(1)));
        assert!(widget.inbox.is_empty());
        assert!(widget.notices().is_empty());
    }

    #[test]
    fn config_has_defaults() {
        let config: Config = toml::from_str("timeout = 8").unwrap();
        assert_eq!((config.timeout, config.history), (8, 100));
    }
}
