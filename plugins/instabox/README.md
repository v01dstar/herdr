# herdr-instabox

A herdr plugin for instabox cloud machines. Sign in, create, start, suspend, stop,
copy and delete machines from a settings pane modelled on herdr's own
Settings → remotes / snapshots / account; running machines appear in herdr's
sidebar on their own, like any saved SSH machine. herdr itself is unmodified.

It talks to the instabox API (`https://api.box.instacloud.com`, or
`INSTABOX_SERVER`) directly and drives the `herdr` CLI (saved machines,
workspaces, notifications). The instabox CLI is not needed; the plugin has its
own sign-in, separate from the CLI's. Linux and macOS.

## Install

```bash
herdr plugin link .                      # from a checkout (run `go build -o bin/herdr-instabox .` first)
herdr plugin install v01dstar/herdr/plugins/instabox --ref instabox-plugin   # needs Go to build
```

Open it with the `instabox settings` action, or bind keys in herdr's config. Pick
keys herdr does not already use: a custom binding that conflicts with a built-in
one is disabled.

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "v01dstar.instabox.open"
description = "instabox settings"

[[keys.command]]
key = "prefix+shift+m"
type = "plugin_action"
command = "v01dstar.instabox.new-workspace"
description = "new workspace on the default machine"
```

## What it does

- **remotes**: Local, your instabox machines and your own SSH remotes, each with
  its actions: Start / Resume / Suspend… / Stop…, Test connection, Edit… (a
  herdr-only name), Use as default, Hide from / Show in sidebar, Copy machine…
  (clone now, or save as snapshot), Delete machine…. `+ Add remote` creates a
  machine from the herdr template or one of your snapshots, or adds an SSH remote.
- **snapshots**: your snapshots; New machine from snapshot…, Delete snapshot….
- **account**: Sign in with GitHub or Google (browser, or a GitHub device code
  without one), Switch account…, Sign out…, and storage/machine/snapshot usage.
- **New workspace** opens a workspace on the default machine (Local unless you
  chose another with Use as default). It never starts a machine.

Long operations (create, start, stop, copy, delete) run as background processes:
closing the pane does not interrupt them, and a result that arrives while the
pane is closed shows as a herdr notification.

## How machines get into herdr

The plugin reconciles herdr's saved machines with instabox when herdr starts, at
most once a minute as you switch workspaces, and every 15 seconds while the
pane is open:

| instabox machine | herdr |
|---|---|
| running | saved (`herdr machine add`, which starts its `herdr-remote` session) and enabled |
| stopped, suspended, starting… | disabled (it stays in the sidebar) |
| hidden | removed from herdr, still listed in the pane |
| deleted | removed, with its SSH config and plugin state |

A machine that has not been running since you signed in cannot be saved yet
(herdr checks the remote when it saves one), so it appears once it runs.

SSH: each machine gets a Host block `herdr-instabox-<machine id>` in the plugin's
state directory, pulled in by one `Include` line that signing in adds to the top
of `~/.ssh/config` (a backup is kept as `~/.ssh/config.herdr-instabox.bak`). The
plugin connects with its own SSH key and 24-hour certificates the API issues
for it; a `Match exec` in front of each block renews the certificate before a
connection when less than 10 minutes are left. The gateway checks a
certificate only when a connection opens, so an open connection outlives it.

Signing out revokes the sign-in and removes every local trace: the herdr
machines, the Host blocks, the SSH key and certificates, and the `Include` line. The machines keep running on instabox, and their herdr-only
names, hidden state and the default are kept for the next sign-in.

## Differences from built-in support

Things a plugin cannot do with herdr's current plugin API:

- The pane is a terminal overlay (a popup once herdr ships popup placement),
  not tabs inside herdr's Settings, and it cannot read herdr's theme.
- New workspace cannot switch the client to another machine; after creating
  a remote workspace it points you to the sidebar.
- Between reconciliations herdr may briefly show a machine that changed state
  elsewhere (for example, auto-suspended) as reconnecting.
- herdr's sidebar has no instabox actions; use the pane.
- Key bindings for the plugin's actions work only while Local is the active
  machine. herdr sends a custom key binding to the active machine's server,
  and a remote machine's server has neither the binding nor the plugin.
  Switch to Local in the sidebar first.

## Development

```bash
go test ./...        # runs against a fake instabox API and the fake herdr CLI in testdata/fake
go build -o bin/herdr-instabox .
```
