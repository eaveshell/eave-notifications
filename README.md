# eave-notifications

[![CI](https://github.com/eaveshell/eave-notifications/actions/workflows/ci.yml/badge.svg)](https://github.com/eaveshell/eave-notifications/actions/workflows/ci.yml)

Notifications for [eave](https://github.com/eaveshell/eave), the Hyprland status bar. It makes eave
your notification daemon: while there are notifications, the bar's right island morphs into a
panel of them, the way it turns into a popover.

```toml
# ~/.config/eave/config.toml
[[right]]
widget = "eaveshell/eave-notifications"
```

Saving rebuilds the bar with it (eave-rebuild.path). Options: `timeout` (seconds a notification
shows when it doesn't say, default 5) and `history` (how many the History tab keeps, default 100).

- **In the island:** every notification, newest first, with a line between each, its actions as
  buttons, and a bar across its top for the time it has left. Pointing at them stops the clock.
  A click runs its default action, or dismisses it.
- **Answering:** messengers that offer a reply (KDE's `inline-reply`, which Telegram and KDE
  Connect use) get a field to type into, right on the notification: Enter sends, Shift+Enter
  starts a new line. The bar takes the keyboard only once you click it, and Esc gives it back.
- **Icons and Markdown:** an app's icon comes from your icon theme, or from a picture file the app
  names (`image-path`, or a path as its icon). An app that sends the `x-eave-markdown` hint gets its
  body set as Markdown.
- **The bell:** a red badge on its corner counts the ones that arrived since you last looked and
  that you didn't deal with yourself. Right click for do not disturb: only critical ones show, the
  rest only go to the list, and the badge hides until you turn it off again. In its popover: the ones waiting for you (click one for all of it and its actions), and a
  History tab with every one that arrived. Once you've looked, they're read: closing the popover
  moves them on to the history.

Only one program can be the notification daemon. If mako, dunst or swaync is running, eave waits
in line and takes over once it quits: stop starting it, then quit it. Notifications sent while
the bar restarts are lost, and the lists start empty after a restart.

## What's here

| | |
|---|---|
| `crates/eave-notify` | the server: `org.freedesktop.Notifications` (1.2) on the session bus, inside the bar. It only receives and reports; it doesn't depend on eave. [On crates.io](https://crates.io/crates/eave-notify). |
| `plugins/notifications` | the widget: turns what the server reports into eave notices, the bell and its popover |

The notice API itself is eave's (`Widget::notices`), so any other engine could feed the same
island through a plugin of its own. The decisions behind all this are D-47 to D-50 in
[eave's docs/decisions.md](https://github.com/eaveshell/eave/blob/main/docs/decisions.md).

## Developing

```fish
cargo test --workspace
```

To try a change in a bar, point config.toml at the checkout: `widget = "~/code/eave-notifications"`.
