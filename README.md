# CosmicDNS

> **Beta** — pre-1.0. It works, but expect rough edges until v1.0 is tagged.

A panel applet for the **COSMIC** desktop (Pop!_OS) that switches which DNS
servers this device uses — one click to jump between Cloudflare, Google,
your Pi-hole/AdGuard, or any server you save — and one click back to the
router's default. Every switch also **flushes the DNS cache**, so the change
takes effect immediately.

## What it does

- **Panel button** shows the current state at a glance: an orange badge when a
  custom DNS is forced, a grey badge when the router/DHCP default is in use.
- **Popup** (click the button):
  - **Custom DNS toggle** — flip it off to return to the router's DNS
    (DHCP-provided), flip it on to re-apply the last server you used.
  - **Server list** — click any saved server to switch to it; the active one
    is checkmarked.
  - **Flush DNS cache** — flush on demand without changing servers (switching
    already flushes automatically). The row confirms with a checkmark when the
    flush completes.
  - A caption shows which network connection the changes apply to.
- **Follows your network**: the DNS override sticks to the *main* active
  network. Unplug the LAN and join Wi-Fi (or hop to another network) and the
  applet moves your chosen DNS onto the new connection automatically — and
  cleans the override off the one you left. Reactions are instant (`nmcli
  monitor`), not polled. Tunnels/VPNs are deliberately exempt.
- **Settings** (the ⚙ button in the popup header):
  - **Manage the server list** — add entries (a name plus one or more IPv4/IPv6
    addresses, comma-separated), **edit** any entry's name or addresses (✎),
    remove ones you don't want, and **drag rows to reorder** the list. It is
    seeded with Cloudflare, Google, and Quad9 on first run, and if the device
    already had a custom DNS configured, it's imported as an entry too.
  - **Applet self-update** — shows the current version, checks the latest
    [GitHub release](https://github.com/davidboulay/CosmicDNS/releases), and
    **automatically updates the applet** when a new release is out (downloads
    the prebuilt binary, swaps it in place, and the panel respawns the new
    version). Auto-update is **on by default**; the toggle in Settings opts out.

## How it works

Everything goes through the same tools you'd use by hand — no daemon, no root:

- **Switching** targets the NetworkManager connection that owns the **default
  route** (i.e. the network you're actually on): `nmcli connection modify …
  ipv4.dns/ipv6.dns + ignore-auto-dns`, then `nmcli device reapply` so the
  change lands without dropping the link.
- **Switching off** clears those overrides so DHCP/the router's DNS is used
  again.
- **Cache flush** uses systemd-resolved (`resolvectl flush-caches`) and runs
  after every switch, plus on demand from the popup.

Settings are persisted via `cosmic-config`.

**VPNs:** a full-tunnel VPN (e.g. WireGuard) usually takes over DNS for the
whole device while connected. The applet detects this and shows a ⚠ warning in
the popup — your switch is saved on the underlying connection and takes effect
when the VPN disconnects. The network-follow behaviour never migrates the DNS
override onto a tunnel. (Run `cosmic-applet-dns --status` in a terminal to
see what the applet sees.)

## Requirements

- COSMIC desktop (Pop!_OS 24.04 or another distro running COSMIC)
- NetworkManager (`nmcli`) — standard on Pop!_OS
- systemd-resolved (`resolvectl`) for cache flushing — standard on Pop!_OS

## Install

One command — no checkout required:

```sh
curl -fsSL https://raw.githubusercontent.com/davidboulay/CosmicDNS/main/install.sh | bash
```

The installer downloads a **prebuilt binary** from the latest
[release](https://github.com/davidboulay/CosmicDNS/releases) (x86_64, no Rust
needed). On other architectures, or if no release is available, it
automatically **builds from source** instead (requires a Rust toolchain).

Then add it to the panel:
**Settings → Desktop → Panel (or Dock) → Add Applet → "DNS"**

### Build from source

```sh
git clone https://github.com/davidboulay/CosmicDNS.git
cd CosmicDNS
./install.sh
```

## Updating

The applet updates itself: enable **"Automatically update the applet"** in its
Settings, or press **"Check for new version" → "Update now"** whenever a new
release is published. Re-running the installer also works.

## License

GPL-3.0-only
