//! Notifications, through eave's own notification server ([`eave_notify`]). While there are any,
//! the right island turns into a panel of them. Each goes away after a few seconds, when
//! dismissed, or when one of its actions is picked. The bar shows a bell, with a red badge counting
//! the ones that arrived since the popover was last opened. The popover lists the latest ones. Right click on the bell for do not
//! disturb: notifications only go to the list, except critical ones.
//!
//! If another notification daemon is running, it keeps the notifications, and the bell is dimmed
//! until it quits.
//!
//! Notifications of a thread (a chat, a session) stack on one card in the island. The popover's
//! New tab groups them by app, newest app first, then by thread.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use eave_api::time::{Duration, every, sleep};
use eave_api::ui::{self, Choices, Icon, Item, Notice, Panel, Pixels, Tone, Urgency};
use eave_api::{Effect, Nudge, Registry, Subscription, Task, Widget};
use eave_notify::{Event, Icon as AppIcon, Image, Notification, Reason, Timeout};
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
    /// What's been typed into each one's reply field so far.
    drafts: HashMap<u32, String>,
    /// The options checked so far on each one that takes several.
    picks: HashMap<u32, Vec<String>>,
    /// The ones whose answer just went: they say so a moment before they go.
    sent: HashSet<u32>,
    /// The apps whose group in the New tab shows all its threads.
    unfolded: HashSet<String>,
    /// The time the popover's "ago"s are counted from.
    now: SystemTime,
}

/// How long a sent answer says so before its notification goes.
const SENT_SHOWN: Duration = Duration::from_millis(1500);
/// A group in the New tab shows this many threads until it's unfolded.
const GROUP_SHOWN: usize = 3;
/// The action that goes back to the question before, on one of several (eave's extension).
const BACK: &str = "x-eave-back";

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
    /// The answer to it as typed so far.
    Draft(u32, String),
    SendReply(u32),
    CancelReply(u32),
    /// The answer sent has said so long enough.
    Sent(u32),
    /// An option picked: the answer, or checked or unchecked if it takes several.
    Pick(u32, String),
    /// Sends the options checked.
    SendPicks(u32),
    /// Closes these, a thread or an app's.
    DismissAll(Vec<u32>),
    /// Shows all of an app's threads in the New tab, or only the first few again.
    Unfold(String),
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
            drafts: HashMap::new(),
            picks: HashMap::new(),
            sent: HashSet::new(),
            unfolded: HashSet::new(),
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
                // One that stays, or one of several questions, is the app's to change or close.
                let stays = self
                    .find(id)
                    .is_some_and(|n| n.resident || n.step.is_some());
                if !stays {
                    self.forget(id);
                }
                let effect = Effect::task(
                    Task::future(async move {
                        if let Err(error) = eave_notify::invoke(id, &key).await {
                            eprintln!("eave-notifications: {error}");
                        }
                        if stays {
                            return;
                        }
                        if let Err(error) = eave_notify::close(id, Reason::Dismissed).await {
                            eprintln!("eave-notifications: {error}");
                        }
                    })
                    .discard(),
                );
                if stays {
                    effect
                } else {
                    effect.close_popover()
                }
            }
            Message::Pick(id, key) => {
                let multiple = self
                    .find(id)
                    .and_then(|n| n.choices.as_ref())
                    .is_some_and(|choices| choices.multiple);
                if !multiple {
                    return self.update(Message::Action(id, key));
                }
                let picks = self.picks.entry(id).or_default();
                match picks.iter().position(|picked| *picked == key) {
                    Some(at) => {
                        picks.remove(at);
                    }
                    None => picks.push(key),
                }
                Effect::none()
            }
            Message::SendPicks(id) => {
                let keys = self.picks.remove(&id).unwrap_or_default();
                if keys.is_empty() {
                    return Effect::none();
                }
                let stays = self
                    .find(id)
                    .is_some_and(|n| n.resident || n.step.is_some());
                if !stays {
                    self.forget(id);
                }
                Effect::task(
                    Task::future(async move {
                        if let Err(error) = eave_notify::picked(id, &keys).await {
                            eprintln!("eave-notifications: {error}");
                        }
                        if !stays
                            && let Err(error) = eave_notify::close(id, Reason::Dismissed).await
                        {
                            eprintln!("eave-notifications: {error}");
                        }
                    })
                    .discard(),
                )
            }
            Message::DismissAll(ids) => {
                for id in &ids {
                    self.forget(*id);
                }
                Effect::task(Task::batch(ids.into_iter().map(|id| {
                    Task::future(async move {
                        let _ = eave_notify::close(id, Reason::Dismissed).await;
                    })
                    .discard()
                })))
            }
            Message::Unfold(app) => {
                if !self.unfolded.remove(&app) {
                    self.unfolded.insert(app);
                }
                Effect::none()
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
            Message::Draft(id, text) => {
                self.drafts.insert(id, text);
                Effect::none()
            }
            Message::CancelReply(id) => {
                self.drafts.remove(&id);
                Effect::none()
            }
            Message::SendReply(id) => {
                let Some(text) = self.drafts.get(&id).cloned() else {
                    return Effect::none();
                };
                if text.trim().is_empty() || self.sent.contains(&id) {
                    return Effect::none();
                }
                // It says it went, a moment, then goes.
                self.drafts.remove(&id);
                self.sent.insert(id);
                let reply = Task::future(async move {
                    if let Err(error) = eave_notify::reply(id, &text).await {
                        eprintln!("eave-notifications: {error}");
                    }
                })
                .discard();
                let gone = Task::future(sleep(SENT_SHOWN)).map(move |()| Message::Sent(id));
                Effect::task(Task::batch([reply, gone]))
            }
            Message::Sent(id) => {
                if !self.sent.remove(&id) {
                    return Effect::none();
                }
                self.forget(id);
                close(id, Reason::Dismissed)
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
        // Do not disturb hides the count, not what it counts: turned off, it shows what came.
        let unread = if self.quiet { 0 } else { self.unread.len() };
        let content = ui::counted(bell, u32::try_from(unread).unwrap_or(u32::MAX));
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
            self.groups()
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
        // The log keeps everything but passing news, a replacement in its old one's place.
        if !notification.transient {
            match self.past.iter_mut().find(|n| n.id == id) {
                Some(old) => *old = notification.clone(),
                None => self.past.insert(0, notification.clone()),
            }
            self.past.truncate(self.config.history);
        }
        // A replacement asks anew: what was checked on the old one is gone.
        self.picks.remove(&id);
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
        self.drafts.remove(&id);
        self.picks.remove(&id);
        self.sent.remove(&id);
    }

    fn find(&self, id: u32) -> Option<&Notification> {
        self.inbox.iter().find(|notification| notification.id == id)
    }

    fn notice(&self, notification: &Notification) -> Notice<Message> {
        let id = notification.id;
        let mut notice = Notice::new(id.into(), &notification.summary)
            .urgency(urgency(notification))
            .time(notification.received)
            .on_press(Message::Press(id))
            .on_dismiss(Message::Dismiss(id));
        if !notification.app.is_empty() {
            notice = notice.app(&notification.app);
        }
        if let Some(icon) = icon(notification) {
            notice = notice.app_icon(icon);
        }
        match &notification.image {
            Some(Image::File(url)) => notice = notice.image(url),
            Some(Image::Pixels(pixels)) => {
                if let Some(pixels) = Pixels::new(pixels.width, pixels.height, pixels.rgba.clone())
                {
                    notice = notice.pixels(pixels);
                }
            }
            None => {}
        }
        if let Some(thread) = thread(notification) {
            notice = notice.thread(thread);
        }
        if let Some(name) = &notification.thread_name {
            notice = notice.thread_name(name);
        }
        if let Some(progress) = notification.progress {
            notice = notice.progress(progress);
        }
        if !notification.body.is_empty() {
            notice = notice.body(&notification.body);
            if notification.markdown {
                notice = notice.markdown();
            }
        }
        if let Some(choices) = self.choices(notification) {
            notice = notice.choices(choices);
        }
        if let Some((index, count)) = notification.step {
            let back = (index > 0 && has_action(notification, BACK))
                .then(|| Message::Action(id, BACK.into()));
            notice = notice.step(index as usize, count as usize, back);
        }
        // Its field, ready: answering is what a message is for. The bar takes the keyboard only
        // once it's clicked.
        if let Some(reply) = &notification.reply {
            let field = reply_field(id, reply, self.draft(id)).sent(self.sent.contains(&id));
            notice = notice.reply(field);
        }
        for (key, label) in &notification.actions {
            // The default action is a click on the notification, not a button; going back is
            // the step's own.
            if key != "default" && key != BACK {
                notice = notice.action(label, Message::Action(id, key.clone()));
            }
        }
        let after = match notification.timeout {
            Timeout::Default => Some(Duration::from_secs(self.config.timeout.max(1))),
            Timeout::After(after) => Some(after),
            Timeout::Never => None,
        };
        // One being answered, or that just was, doesn't go by itself meanwhile.
        let busy = self.sent.contains(&id) || self.picks.get(&id).is_some_and(|p| !p.is_empty());
        match after {
            Some(after) if !busy => notice.expires(after, Message::Expired(id)),
            _ => notice,
        }
    }

    /// Its options, with what's checked so far.
    fn choices(&self, notification: &Notification) -> Option<Choices<Message>> {
        let id = notification.id;
        let given = notification.choices.as_ref()?;
        let picks = self.picks.get(&id);
        let mut choices = if given.multiple {
            Choices::many().on_send(Message::SendPicks(id))
        } else {
            Choices::one()
        };
        for option in &given.options {
            let picked = picks.is_some_and(|picks| picks.contains(&option.key));
            let description = Some(option.description.clone()).filter(|d| !d.is_empty());
            choices = choices.option(
                &option.label,
                description,
                picked,
                Message::Pick(id, option.key.clone()),
            );
            if option.recommended {
                choices = choices.recommended();
            }
        }
        Some(choices)
    }

    /// The New tab: critical ones first, then a group per app, the one with the newest first,
    /// each a row per thread ([`thread`]).
    fn groups(&self) -> ui::Node<Message> {
        let mut nodes = Vec::new();
        let critical: Vec<_> = self
            .inbox
            .iter()
            .filter(|n| n.urgency == eave_notify::Urgency::Critical)
            .collect();
        if !critical.is_empty() {
            nodes.push(ui::list(critical.into_iter().map(|n| self.row(&[n]))));
        }
        for (app, threads) in grouped(&self.inbox) {
            let count: usize = threads.iter().map(Vec::len).sum();
            let ids: Vec<u32> = threads.iter().flatten().map(|n| n.id).collect();
            let unfolded = self.unfolded.contains(&app);
            let hidden = threads.len().saturating_sub(GROUP_SHOWN);
            let mut trailing = Vec::new();
            if hidden > 0 {
                let label = if unfolded {
                    "Fewer".to_string()
                } else {
                    format!("{hidden} more")
                };
                trailing.push(ui::button(ui::label(label), Message::Unfold(app.clone())));
            }
            trailing.push(ui::button(ui::icon(Icon::Close), Message::DismissAll(ids)));
            let title = match count {
                1 => app.clone(),
                count => format!("{app} · {count}"),
            };
            nodes.push(
                ui::section(if app.is_empty() {
                    "Other".into()
                } else {
                    title
                })
                .trailing(ui::row(trailing)),
            );
            let shown = if unfolded { threads.len() } else { GROUP_SHOWN };
            nodes.push(ui::list(
                threads.iter().take(shown).map(|thread| self.row(thread)),
            ));
        }
        ui::column(nodes)
    }

    /// A thread in the New tab: its newest notification, and how many it holds. A click shows
    /// all of them, the newest's actions and its reply field.
    fn row(&self, thread: &[&Notification]) -> ui::Node<Message> {
        let notification = thread[0];
        let id = notification.id;
        let ids: Vec<u32> = thread.iter().map(|n| n.id).collect();
        let leading = leading(notification);
        let title = notification
            .thread_name
            .clone()
            .unwrap_or_else(|| notification.summary.clone());
        let line = |n: &Notification| match n.body.lines().find(|line| !line.trim().is_empty()) {
            Some(line) => line.to_string(),
            None => n.summary.clone(),
        };
        let subtitle = if notification.thread_name.is_some() {
            notification.summary.clone()
        } else {
            match notification.body.lines().next() {
                Some(line) if !line.is_empty() => line.to_string(),
                _ => notification.app.clone(),
            }
        };
        let mut trailing = Vec::new();
        if thread.len() > 1 {
            trailing.push(ui::badge(thread.len().to_string()));
        }
        trailing.push(ui::label(ago(notification.received, self.now)));
        trailing.push(ui::button(ui::icon(Icon::Close), Message::DismissAll(ids)));
        let row = ui::list_item(title)
            .subtitle(subtitle)
            .leading(leading)
            .trailing(ui::row(trailing))
            .on_press(Message::Expand(id));
        if self.expanded != Some(id) {
            return row;
        }
        // The earlier ones a line each, then all of the newest.
        let earlier = (thread.len() > 1).then(|| {
            ui::column(thread[1..].iter().rev().map(|n| {
                ui::row([
                    ui::label(line(n)).tone(Tone::Dim),
                    ui::fill(),
                    ui::label(ago(n.received, self.now)).tone(Tone::Dim),
                ])
            }))
        });
        let mut nodes = vec![Some(row), earlier];
        nodes.extend(self.expanded_parts(notification).into_iter().map(Some));
        ui::column(nodes.into_iter().flatten())
    }

    /// What an expanded row shows of its newest notification: all of its body, its options,
    /// its reply field and its actions.
    fn expanded_parts(&self, notification: &Notification) -> Vec<ui::Node<Message>> {
        let id = notification.id;
        // All of the body, then its options, then a button per action, the default one first
        // and main.
        let body = (!notification.body.is_empty()).then(|| body(notification));
        let options = notification.choices.as_ref().map(|choices| {
            let picks = self.picks.get(&id);
            let rows = choices.options.iter().map(|option| {
                let picked = picks.is_some_and(|picks| picks.contains(&option.key));
                let item = ui::list_item(&option.label)
                    .selected(picked)
                    .on_press(Message::Pick(id, option.key.clone()));
                if option.description.is_empty() {
                    item
                } else {
                    item.subtitle(&option.description)
                }
            });
            let send = choices.multiple.then(|| {
                ui::row([
                    ui::fill(),
                    ui::button(ui::label("Send"), Message::SendPicks(id)).prominent(),
                ])
            });
            ui::column([Some(ui::list(rows)), send].into_iter().flatten())
        });
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
        // Its field, ready to type into.
        let field = notification.reply.as_ref().map(|reply| {
            ui::text_area(
                reply.placeholder.as_deref().unwrap_or("Reply"),
                self.draft(id),
                move |text| Message::Draft(id, text),
            )
            .on_submit(Message::SendReply(id))
            .trailing(ui::button(ui::icon(Icon::Forward), Message::SendReply(id)))
        });
        let buttons: Vec<_> = buttons.collect();
        let buttons =
            (!buttons.is_empty()).then(|| ui::row(std::iter::once(ui::fill()).chain(buttons)));
        [body, options, field, buttons]
            .into_iter()
            .flatten()
            .collect()
    }

    /// The answer typed so far into `id`.
    fn draft(&self, id: u32) -> &str {
        self.drafts.get(&id).map_or("", String::as_str)
    }
}

impl Notifications {
    /// A notification in the History tab: what it said and when. A click shows all of it. Its
    /// actions are gone: the app was told it closed.
    fn past_row(&self, notification: &Notification) -> ui::Node<Message> {
        let id = notification.id;
        let leading = leading(notification);
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
        ui::column([row, body(notification)])
    }
}

/// The field to type an answer into, on a notification in the island.
fn reply_field(id: u32, reply: &eave_notify::Reply, draft: &str) -> ui::Reply<Message> {
    ui::Reply::new(
        reply.placeholder.as_deref().unwrap_or("Reply"),
        draft,
        move |text| Message::Draft(id, text),
    )
    .on_submit(Message::SendReply(id))
    .on_cancel(Message::CancelReply(id))
}

/// The app's icon to show: the file or app id it gave, or its name.
fn icon(notification: &Notification) -> Option<String> {
    let given = match &notification.icon {
        Some(AppIcon::App(id) | AppIcon::File(id)) => Some(id.clone()),
        None => None,
    };
    given.or_else(|| Some(notification.app.to_lowercase()).filter(|app| !app.is_empty()))
}

/// Its icon in the popover's lists: its picture, its app's icon, or a bell.
fn leading(notification: &Notification) -> ui::Node<Message> {
    let fallback = match (&notification.icon, icon(notification)) {
        (Some(AppIcon::File(url)), _) => ui::cover(url, ui::icon(Icon::Bell { muted: false })),
        (_, Some(icon)) => ui::app_icon(icon),
        (_, None) => ui::icon(Icon::Bell { muted: false }),
    };
    match &notification.image {
        Some(Image::File(url)) => ui::cover(url, fallback),
        _ => fallback,
    }
}

fn urgency(notification: &Notification) -> Urgency {
    match notification.urgency {
        eave_notify::Urgency::Low => Urgency::Low,
        eave_notify::Urgency::Normal => Urgency::Normal,
        eave_notify::Urgency::Critical => Urgency::Critical,
    }
}

/// The conversation it's part of: the one the app named, or, for one that takes a reply (a
/// message), its sender, which messengers put in the summary. Others stand alone.
fn thread(notification: &Notification) -> Option<String> {
    notification.thread.clone().or_else(|| {
        notification
            .reply
            .as_ref()
            .map(|_| format!("summary:{}", notification.summary))
    })
}

fn has_action(notification: &Notification, key: &str) -> bool {
    notification.actions.iter().any(|(given, _)| given == key)
}

/// `inbox` grouped by app, the app with the newest first, then by thread, newest first: each
/// thread its notifications, newest first. Critical ones are left out: they go above the groups.
fn grouped(inbox: &[Notification]) -> Vec<(String, Vec<Vec<&Notification>>)> {
    let mut groups: Vec<(String, Vec<Vec<&Notification>>)> = Vec::new();
    for notification in inbox {
        if notification.urgency == eave_notify::Urgency::Critical {
            continue;
        }
        let group = match groups.iter().position(|(app, _)| *app == notification.app) {
            Some(at) => &mut groups[at].1,
            None => {
                groups.push((notification.app.clone(), Vec::new()));
                &mut groups.last_mut().expect("just pushed").1
            }
        };
        let key = thread(notification);
        let same = key.as_ref().and_then(|key| {
            group
                .iter()
                .position(|t| thread(t[0]).as_ref() == Some(key))
        });
        match same {
            Some(at) => group[at].push(notification),
            None => group.push(vec![notification]),
        }
    }
    groups
}

/// All of its body, dimmed: Markdown if the app said so.
fn body(notification: &Notification) -> ui::Node<Message> {
    if notification.markdown {
        ui::markdown(&notification.body).tone(Tone::Dim)
    } else {
        ui::label(&notification.body).tone(Tone::Dim)
    }
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
            markdown: false,
            actions: vec![
                ("default".into(), "Open".into()),
                ("reply".into(), "Reply".into()),
            ],
            reply: None,
            urgency,
            timeout: Timeout::Default,
            received: SystemTime::UNIX_EPOCH,
            image: None,
            progress: None,
            thread: None,
            thread_name: None,
            choices: None,
            step: None,
            transient: false,
            resident: false,
        }
    }

    fn arrive_as(widget: &mut Notifications, notification: Notification) {
        let _ = widget.update(Message::Server(Event::Notified(Box::new(notification))));
    }

    fn arrive(widget: &mut Notifications, id: u32) {
        let notification = notification(id, eave_notify::Urgency::Normal);
        let _ = widget.update(Message::Server(Event::Notified(Box::new(notification))));
    }

    /// The count on the bell's badge.
    fn badge(widget: &Notifications) -> u32 {
        let content = widget.item().into_parts().content.expect("a bell");
        match content.kind() {
            ui::Kind::Counted { count, .. } => *count,
            _ => panic!("a counted bell"),
        }
    }

    #[test]
    fn the_badge_counts_unread_and_hides_in_do_not_disturb() {
        let mut widget = notifications();
        assert_eq!(badge(&widget), 0);
        arrive(&mut widget, 1);
        arrive(&mut widget, 2);
        assert_eq!(badge(&widget), 2);
        let _ = widget.update(Message::ToggleQuiet);
        assert_eq!(badge(&widget), 0);
        arrive(&mut widget, 3);
        let _ = widget.update(Message::ToggleQuiet);
        assert_eq!(badge(&widget), 3, "what came while quiet shows after");
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
    fn a_reply_field_shows_at_once_and_sending_takes_the_notification_away() {
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
        assert!(field(&widget), "ready, no Reply button first");
        // Nothing typed: nothing sent.
        let _ = widget.update(Message::SendReply(1));
        assert!(field(&widget));

        let _ = widget.update(Message::Draft(1, "on my way".into()));
        let _ = widget.update(Message::SendReply(1));
        // It says it went, and doesn't expire meanwhile.
        let notice = widget.notices().into_iter().next().expect("still there");
        let parts = notice.into_parts();
        assert!(parts.reply.expect("its field").sent);
        assert!(parts.expires.is_none());
        let _ = widget.update(Message::Sent(1));
        assert!(widget.notices().is_empty());
        assert!(widget.inbox.is_empty());
        assert_eq!(widget.past.len(), 1, "still in the history");
    }

    fn message(id: u32, from: &str, app: &str) -> Notification {
        let mut message = notification(id, eave_notify::Urgency::Normal);
        message.app = app.into();
        message.summary = from.into();
        message.reply = Some(eave_notify::Reply {
            label: "Reply".into(),
            placeholder: None,
            submit: None,
        });
        message
    }

    /// The New tab's groups: each app, and its threads' ids.
    fn groups(widget: &Notifications) -> Vec<(String, Vec<Vec<u32>>)> {
        grouped(&widget.inbox)
            .into_iter()
            .map(|(app, threads)| {
                let threads = threads
                    .into_iter()
                    .map(|thread| thread.into_iter().map(|n| n.id).collect())
                    .collect();
                (app, threads)
            })
            .collect()
    }

    #[test]
    fn the_new_tab_groups_by_app_then_by_thread() {
        let mut widget = notifications();
        arrive_as(&mut widget, message(1, "Maria", "Chats"));
        arrive(&mut widget, 2);
        arrive_as(&mut widget, message(3, "Tom", "Chats"));
        arrive_as(&mut widget, message(4, "Maria", "Chats"));
        let mut session = notification(5, eave_notify::Urgency::Normal);
        session.thread = Some("s1".into());
        arrive_as(&mut widget, session);
        arrive_as(&mut widget, notification(6, eave_notify::Urgency::Critical));
        assert_eq!(
            groups(&widget),
            [
                ("App".to_string(), vec![vec![5], vec![2]]),
                ("Chats".to_string(), vec![vec![4, 1], vec![3]]),
            ],
            "newest app first, critical ones apart"
        );
    }

    #[test]
    fn a_message_s_sender_is_its_thread_on_the_card() {
        let mut widget = notifications();
        arrive_as(&mut widget, message(1, "Maria", "Chats"));
        arrive(&mut widget, 2);
        let threads: Vec<_> = widget
            .notices()
            .into_iter()
            .map(|notice| notice.into_parts().thread)
            .collect();
        assert_eq!(threads, [None, Some("summary:Maria".to_string())]);
    }

    #[test]
    fn passing_news_stays_out_of_the_history() {
        let mut widget = notifications();
        let mut passing = notification(1, eave_notify::Urgency::Normal);
        passing.transient = true;
        arrive_as(&mut widget, passing);
        assert_eq!(widget.inbox.len(), 1);
        assert!(widget.past.is_empty());
    }

    #[test]
    fn a_resident_one_and_a_question_of_several_stay_after_an_action() {
        let mut widget = notifications();
        let mut resident = notification(1, eave_notify::Urgency::Normal);
        resident.resident = true;
        arrive_as(&mut widget, resident);
        let mut step = notification(2, eave_notify::Urgency::Normal);
        step.step = Some((0, 2));
        arrive_as(&mut widget, step);
        let _ = widget.update(Message::Action(1, "reply".into()));
        let _ = widget.update(Message::Action(2, "reply".into()));
        assert_eq!(widget.inbox.len(), 2);
    }

    fn choosing(id: u32, multiple: bool) -> Notification {
        let mut question = notification(id, eave_notify::Urgency::Normal);
        let option = |key: &str| eave_notify::Choice {
            key: key.into(),
            label: key.to_uppercase(),
            description: String::new(),
            recommended: key == "b",
        };
        question.choices = Some(eave_notify::Choices {
            options: vec![option("a"), option("b"), option("c")],
            multiple,
        });
        question
    }

    #[test]
    fn options_checked_show_and_send_together() {
        let mut widget = notifications();
        arrive_as(&mut widget, choosing(1, true));
        let _ = widget.update(Message::Pick(1, "c".into()));
        let _ = widget.update(Message::Pick(1, "a".into()));
        let _ = widget.update(Message::Pick(1, "c".into()));
        assert_eq!(widget.picks[&1], ["a"], "picked again, unchecked");
        let parts = widget.notices().remove(0).into_parts();
        let choices = parts.choices.expect("its options");
        let picked: Vec<_> = choices.options.iter().map(|o| o.picked).collect();
        assert_eq!(picked, [true, false, false]);
        assert!(choices.options[1].recommended);
        assert!(parts.expires.is_none(), "doesn't go while being answered");
        let _ = widget.update(Message::SendPicks(1));
        assert!(widget.inbox.is_empty(), "sent, it goes");
    }

    #[test]
    fn one_pick_is_the_answer() {
        let mut widget = notifications();
        arrive_as(&mut widget, choosing(1, false));
        let _ = widget.update(Message::Pick(1, "b".into()));
        assert!(widget.inbox.is_empty());
        assert!(widget.picks.is_empty());
    }

    #[test]
    fn a_question_of_several_says_where_it_is_and_goes_back() {
        let mut widget = notifications();
        let mut second = choosing(1, false);
        second.step = Some((1, 3));
        second.actions.push((BACK.into(), "Back".into()));
        arrive_as(&mut widget, second);
        let parts = widget.notices().remove(0).into_parts();
        let step = parts.step.expect("a step");
        assert_eq!((step.index, step.count), (1, 3));
        assert!(matches!(step.on_back, Some(Message::Action(1, ref key)) if key == BACK));
        assert!(
            parts.actions.iter().all(|(label, _)| label != "Back"),
            "back is the step's, not an action's"
        );
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
