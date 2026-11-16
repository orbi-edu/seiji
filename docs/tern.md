# Seiji in Tern

This guide shows how to see and drive your Seiji simulators from [Tern](https://tern.sh). The Seiji plugin adds a panel with your budget, your devices and your runs. It opens a device's screen in Tern's browser, beside the panel, where you watch it live and tap, swipe and type on it.

## Contents

- [What you get](#what-you-get)
- [Requirements](#requirements)
- [Install](#install)
- [Sign in](#sign-in)
- [Example: check a sign-in screen](#example-check-a-sign-in-screen)
- [The panel](#the-panel)
- [The screen](#the-screen)
- [Links from agents](#links-from-agents)
- [Keys](#keys)
- [Limits](#limits)
- [How it works](#how-it-works)

## What you get

| Where | What |
|---|---|
| Palette | **Seiji: Devices** (⌃⌥D), **Seiji: Runs**, **Seiji: Sign in**, **Seiji: Sign out** |
| Status line | `Seiji · 1 device · 42 min left`; amber under 25 % of your minutes, red under 10 %. A click opens the panel. |
| Panel, head | Minutes used this month, minutes left, devices held, estimated cost. |
| Panel, Devices | Every device you hold. Open its screen, float it, take a screenshot, copy a view-only link, release it, or boot a new one. |
| Panel, Runs | Your recorded runs, with the Markdown summary inline. Copy it for a pull request or download `evidence.zip`. |
| Screen | The simulator, live, in Tern's browser. Click to tap, drag to swipe, type to type. |

## Requirements

- Tern desktop. Browser blocks exist only there, not in Tern web or the iOS app.
- The `seiji` CLI on the Mac that runs Tern. It holds your token in the macOS keychain; the plugin asks it for the token and keeps it in memory only.
- A Seiji server you can reach, and a token from its admin (`seiji user add`).
- The screen works for iOS simulators. Android devices show in the panel but have no live screen yet.

## Install

You need Rust (`rustup`) and Tern desktop on your Mac. No clone is needed.

**1. Install the `seiji` CLI.** It signs you in and keeps your token in the keychain. The plugin asks it for the token.

```sh
cargo install --git https://github.com/wiljafor1/seiji
seiji --version
```

**2. Install the plugin.**

```sh
tern plugin install github.com/wiljafor1/seiji/tern-plugin
tern plugin list            # seiji 0.1.0 Seiji — 1 blocks, 0 lenses, window  ready
```

Tern loads it at once; no restart.

**3. Let Tern find the CLI.** Tern started from the Dock may not have `~/.cargo/bin` on its PATH. When the panel says *The seiji CLI is missing*, point Tern at it and start Tern again:

```sh
launchctl setenv SEIJI_BIN "$HOME/.cargo/bin/seiji"
```

To work on the plugin itself, use the copy in a clone instead: `tern plugin link tern-plugin` from the repository's root. Tern then reads the files where they are. `tern plugin remove seiji` (installed) or `tern plugin unlink seiji` (linked) takes it out.

## Sign in

Run **Seiji: Sign in** from the palette, or press enter in the panel. Enter the server's URL. Tern opens a tab that runs `seiji login --url <server>`, which asks for your token without echoing it. The panel picks up the sign-in by itself within a few seconds.

In a terminal:

```sh
seiji login --url https://seiji.example.com    # paste the token, then enter
seiji token print --json                        # what the plugin reads
seiji logout
```

`SEIJI_URL` and `SEIJI_TOKEN` override the keychain, for CI and tests.

## Example: check a sign-in screen

This example walks through a typical session. You boot an iPhone, try the sign-in screen by hand, then let an agent record the same check as a run with evidence. The pictures come from Seiji's test harness, which uses a fake simulator.

**1. Open the panel.** Press ⌃⌥D, or run **Seiji: Devices** from the palette. The head shows your minutes this month.

**2. Boot a device.** Press `n` and pick `iphone-16`. The row counts the seconds while it boots (30-90 s). When it says `ready`, install your app the usual way, for example by asking your agent to call `install_app`, or run your dev server through `seiji forward` ([apps.md](apps.md)).

**3. Look at it.** Press `s`. A screenshot appears in the device card.

![The Devices tab with a screenshot in the device card](images/tern-devices.png)

**4. Drive it.** Press `o`. The screen opens in Tern's browser beside the panel. Click **Email** and type an address, click **Password** and type, then click **Sign In**. Press `f` instead of `o` to float the screen over the panel while you work in another pane.

![The screen in Tern's browser](images/tern-screen.png)

**5. Let an agent record it.** In an agent pane (Claude Code, Codex, omp) connected to Seiji's MCP endpoint, ask:

```text
Call share_screen on my Seiji iPhone so I can watch. Then run_actions named
"Sign in works": tap "Email", type "ana@example.com", tap "Sign In", and
assert "Welcome, Ana!" is visible.
```

The agent prints a viewer link. Click it in the agent's pane: the screen opens beside that pane, and you watch the steps as they run. The link is view-only unless the agent passes `control: true`.

**6. Use the evidence.** Press `tab` for **Runs**. The run's summary shows in its card: the verdict, every step and the files. Press `c` to copy the Markdown for your pull request, or `d` to save `evidence.zip` to `~/Downloads`.

![The Runs tab with a run's summary](images/tern-runs.png)

**7. Release the device.** Back in **Devices**, press `x`, then enter. The device stops counting against your minutes. The run and its evidence stay.

## The panel

The panel polls `budget_status` every 3 s and `list_runs` every 10 s while the Runs tab is open. Each device row shows its model, its lease and when it ends. The detail card beside the list says when Seiji releases it: at the latest at the lease's maximum length, or after the idle time with no calls.

- **New device** opens a picker with every model the host offers and how many are free. Booting takes 30-90 s; the row shows the time so far.
- **Screenshot** shows the screen inside the card. A click on the picture zooms it.
- **Share link** copies a view-only viewer link. It opens once, within 60 s.
- **Release** asks first, then deletes the device. Open screens on it close. Runs and their evidence stay.
- In **Runs**, **Copy summary** copies the run's Markdown summary (`export_evidence`). **Download evidence** saves `evidence.zip` to `~/Downloads`. Past 16 MiB, Tern cannot download it; the plugin copies a `curl` command that does.

## The screen

**Open screen** (o) shows the device in Tern's browser beside the panel. **Float** (f) shows it as a picture in picture over the panel instead.

| You do | The device gets |
|---|---|
| Click | A tap where you clicked |
| Hold for 400 ms or more | A long press |
| Drag | A swipe along the drag |
| Scroll the wheel | One swipe per gesture |
| Type | The text, through AXe |
| Enter, Esc, Backspace, Tab, arrows | The matching key |
| Paste (⌘V) | The clipboard's text |
| ⌘⇧H, ⌘L | Home, Lock |
| ⌘S, or **Screenshot** | A JPEG download of the screen |

The dot in the toolbar is green while the stream is live, amber while a run uses the device, and red when the device is gone. While a run holds the device, the screen keeps playing but input waits: a tap answers "A run is using the device" instead of mixing with the run's steps.

A view-only screen shows a **View only** badge and sends no input.

## Links from agents

An agent can call the MCP tool `share_screen` to give you a link to its simulator. The link is view-only unless the agent passes `control: true`. Click it in any Tern pane: the plugin opens it beside that pane. Alt-click floats it. Outside Tern, any browser opens it.

## Keys

| Key | Devices | Runs |
|---|---|---|
| tab | Go to Runs | Go to Devices |
| ↑ ↓ | Select a device | Select a run |
| o, enter | Open the screen | Copy the summary |
| f | Float the screen | |
| s | Screenshot | |
| l | Copy a view-only link | |
| x | Release (asks first) | |
| d | | Download evidence |
| n | New device | New device |
| r | Refresh | Refresh |

## Limits

- iOS simulators only, for the screen.
- One viewer link opens one viewer session. A session lasts `viewer.sessionHours` (12 h by default) and never past its lease. Open the device again from the panel to get a new link.
- A lease takes at most `viewer.maxViewersPerLease` open screens (4 by default).
- Watching a device keeps it from idle release: the stream counts as activity once a minute. It does not stop the maximum lease length.
- Typed text goes through AXe's `type`, with its limits on characters.

## How it works

```mermaid
sequenceDiagram
    participant P as Seiji panel (Tern host)
    participant W as Tern window
    participant S as seiji serve
    participant A as axe stream-video
    P->>S: POST /v1/leases/{id}/viewer (bearer)
    S-->>P: /view/open/{ticket}, valid 60 s
    P->>W: open the link (route)
    W->>S: GET /view/open/{ticket} in the browser block
    S-->>W: cookie for /view/s/{sid}, 303 to the page
    W->>S: GET /view/s/{sid}/stream
    S->>A: start one stream for the lease
    A-->>S: JPEG frames
    S-->>W: multipart/x-mixed-replace
    W->>S: POST /view/s/{sid}/input (tap, swipe, type)
```

The token never reaches the browser. The panel uses it to ask for a one-time ticket; the browser trades the ticket for a session cookie scoped to that session's path. Every request checks the session, the user and the lease. For the full model, read [security.md](security.md#the-viewer). For the endpoints, read [mcp.md](mcp.md). For the settings, read [configuration.md](configuration.md#viewer).
