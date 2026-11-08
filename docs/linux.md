# Gentle Dot on Linux

What the Linux desktop app does, how it differs from macOS, and why. Install steps are in [install.md](install.md#linux); the tester checklist is in [linux-testing.md](linux-testing.md); the full design is in [design.md](design.md) (S24.12 and "Linux" under the desktop shell).

## At a glance

| | COSMIC (Pop!_OS, Wayland) | X11 session | GNOME on Wayland (XWayland) | Hyprland (Omarchy) |
|---|---|---|---|---|
| Rose, panel, tray, chat, voice, connectors | Yes, as layer-shell surfaces | Yes | Yes | Yes, with the window rules in `scripts/linux/hyprland/` |
| Rose stays on top, drag and snap to the edge | Yes | Yes | Yes | Placed by the rules, no snapping |
| Toggle shortcut | Bind `gentle-dot --toggle` | Alt+Space | Custom shortcut to `gentle-dot --toggle` | `bind` from the scripts |
| Computer control | Yes | Yes | No (refused, see below) | No (refused) |
| Panic shortcut | Bind Alt+Shift+Esc to `gentle-dot --panic` | Alt+Shift+Esc | — | — |

## Computer control

The assistant gets the same tools, session, guards, and dialogs as on macOS: a 30-minute session the user allows in a native dialog, a pink glow while it works, a blocklist, a rate limit, a confirmation before risky clicks (Send, Delete, Buy…) with the refusing answer as the default, and a panic stop. Everything is enforced by the desktop app, outside the assistant's reach.

How each platform piece is done:

| Need | COSMIC Wayland | X11 |
|---|---|---|
| Windows, front app, activation | `zcosmic_toplevel_info_v1` / `zcosmic_toplevel_manager_v1`, workspaces from `ext_workspace_manager_v1` | EWMH |
| Screenshots | xdg-desktop-portal Screenshot (non-interactive), cropped to one monitor, the portal's file deleted | `GetImage` |
| Keys and typing | `zwp_virtual_keyboard_v1` with a keymap written for each chunk, so any Unicode text types exactly | XTest |
| Pointer (move, click, drag, scroll) | A `/dev/uinput` device, only after the user turns on **full desktop control** | XTest |
| What is under the pointer (risk checks) | AT-SPI accessibility | AT-SPI accessibility |
| Apps (`list_apps`, `open_app`) | `.desktop` entries | `.desktop` entries |

**Full desktop control** is the Linux counterpart of the macOS Accessibility permission: off by default, turned on in Connectors → Computer (Grant) behind a native confirmation, saved as `full_control` in `~/.gentle-dot/desktop.json`. It needs write access to `/dev/uinput`; the app names the udev rule when it lacks it. Turning it off is setting `full_control` to `false` and restarting the app, as revoking Accessibility is done in System Settings on macOS.

**Fail closed.** Whatever the app cannot know makes it ask or refuse, never act blindly:

- Other Wayland desktops (GNOME, Hyprland) do not tell apps which window is in front, so actions are refused there.
- An AT-SPI query that does not answer in time gives an unknown element, and the risk checks then ask.
- On X11, a menu or tooltip (an override-redirect window) is treated as an unknown app instead of the window below it.
- On COSMIC, windows on other workspaces never count as the target of a click.
- Approval dialogs refuse by default and go away by themselves when the request they answer is stale.

**Multiple monitors.** Wayland gives apps no pointer position, so a screenshot covers the monitor under the assistant's last pointer move, or the front window's monitor when another window came to the front since then (the user brought one forward, or `open_app` did).

**Pointer details measured on COSMIC.** An absolute uinput device maps onto the monitor under the pointer, so moves combine relative pushes and per-monitor absolute moves. A drag first leads past the drag threshold and waits 100 ms, so apps start their drag before the drop. The kernel drops an absolute value equal to the last one, so a repeated target sends a neighbouring value first. A scroll sends one frame per wheel notch and counts a notch as 120 px, so pages scroll about as far as asked.

## Desktop shell on COSMIC

COSMIC runs the app as a native Wayland client, where apps cannot place windows or keep them on top. The rose and the panel are therefore `gtk-layer-shell` surfaces (a new package dependency): the app places them by margins on a monitor, follows the rose's drag in GTK with a pointer grab, snaps it to the edge, and remembers its position. Native dialogs (grant, full control, risky-action confirmation) are layer surfaces too, so they cannot fall behind the window the assistant is working in. The glow lets every click through, and the panel shown again after an action does not take the focus from the app being controlled.

Wayland desktops own global shortcuts, so the app registers none there: `gentle-dot --toggle` opens or closes the panel of the running app, and `gentle-dot --panic` ends computer control (bind both in the desktop's keyboard settings). The tray also offers **Stop computer control** and **Yolo mode**, as on macOS and Windows.

## Other fixes found while testing on Linux

These change shared code and help every platform:

- The computer-control banner warns when the chosen model cannot see images, since it would act blind.
- The assistant keeps its memories in its own Engram project, `gentle-dot-assistant`, so it no longer reads the notes of developers who work on a clone of this repository; a chat resumed after the change moves to a new session in that project.
- A connector signed in while the assistant runs is visible to it without **Restart assistant**, and a connector stays reachable after the engine gives up on one request.
- Pasting an image-only clipboard (a Flameshot capture) adds the image, and a middle-click paste of selected text still works.
- One press of Esc always closes the panel.

## Known limits

- Computer control needs COSMIC or an X11 session; GNOME and Hyprland on Wayland refuse it until they offer a way to know the front window.
- Screenshots cannot leave a window out on Linux, so the rose, the panel, and the glow are hidden around each capture.
- Connector secrets live in the desktop keyring (Secret Service); while it is unlocked, any program running as the user can read them (see [install.md](install.md#linux)).
