# herdr-hangar

A herdr plugin for hangar cloud machines. Sign in, create, start, suspend, stop,
copy and delete machines from a settings pane modelled on herdr's own
Settings → remotes / images / account; running machines appear in herdr's
sidebar on their own, like any saved SSH machine. herdr itself is unmodified.

It drives the `hangar` CLI (sign-in, API, certificates) and the `herdr` CLI
(saved machines, workspaces, notifications). Linux and macOS.

## Install

```bash
herdr plugin link .                      # from a checkout (run `go build -o bin/herdr-hangar .` first)
herdr plugin install v01dstar/herdr/plugins/hangar --ref hangar-plugin   # needs Go to build
```

Open it with the `hangar settings` action, or bind keys in herdr's config. Pick
keys herdr does not already use: a custom binding that conflicts with a built-in
one is disabled.

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "v01dstar.hangar.open"
description = "hangar settings"

[[keys.command]]
key = "prefix+shift+m"
type = "plugin_action"
command = "v01dstar.hangar.new-workspace"
description = "new workspace on the default machine"
```

## What it does

- **remotes**: Local, your hangar machines and your own SSH remotes, each with
  its actions: Start / Resume / Suspend… / Stop…, Test connection, Edit… (a
  herdr-only name), Use as default, Hide from / Show in sidebar, Copy machine…
  (clone now, or save as image), Delete machine…. `+ Add remote` creates a
  machine from the herdr template or one of your images, or adds an SSH remote.
- **images**: your images; New machine from image…, Delete image….
- **account**: Sign in (browser, or a device code without one), Sign up with
  invite code…, Switch account…, Sign out…, and storage/machine/image usage.
- **New workspace** opens a workspace on the default machine (Local unless you
  chose another with Use as default). It never starts a machine.

Long operations (create, start, stop, copy, delete) run as background processes:
closing the pane does not interrupt them, and a result that arrives while the
pane is closed shows as a herdr notification.

## How machines get into herdr

The plugin reconciles herdr's saved machines with hangar when herdr starts, at
most once a minute as you switch workspaces, and every 15 seconds while the
pane is open:

| hangar machine | herdr |
|---|---|
| running | saved (`herdr machine add`, which starts its `herdr-remote` session) and enabled |
| stopped, suspended, starting… | disabled (it stays in the sidebar) |
| hidden | removed from herdr, still listed in the pane |
| deleted | removed, with its SSH config and plugin state |

A machine that has not been running since you signed in cannot be saved yet
(herdr checks the remote when it saves one), so it appears once it runs.

SSH: each machine gets a Host block `herdr-hangar-<machine id>` in the plugin's
state directory, pulled in by one `Include` line that signing in adds to the top
of `~/.ssh/config` (a backup is kept as `~/.ssh/config.herdr-hangar.bak`). A
`Match exec` in front of each block renews the short-lived certificate before a
connection when it is close to expiry.

Signing out removes every local trace: the herdr machines, the Host blocks and
the `Include` line. The machines keep running on hangar, and their herdr-only
names, hidden state and the default are kept for the next sign-in.

## Differences from built-in support

Things a plugin cannot do with herdr's current plugin API:

- The pane is a terminal overlay (a popup once herdr ships popup placement),
  not tabs inside herdr's Settings, and it cannot read herdr's theme.
- New workspace cannot switch the client to another machine; after creating
  a remote workspace it points you to the sidebar.
- Between reconciliations herdr may briefly show a machine that changed state
  elsewhere (for example, auto-suspended) as reconnecting.
- herdr's sidebar has no hangar actions; use the pane.
- Key bindings for the plugin's actions work only while Local is the active
  machine. herdr sends a custom key binding to the active machine's server,
  and a remote machine's server has neither the binding nor the plugin.
  Switch to Local in the sidebar first.

## Development

```bash
go test ./...        # runs against the fake hangar/herdr CLIs in testdata/fake
go build -o bin/herdr-hangar .
```
