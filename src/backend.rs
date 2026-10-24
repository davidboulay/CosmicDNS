// SPDX-License-Identifier: GPL-3.0-only
//
// DNS switching via NetworkManager (`nmcli`) and cache flushing via
// systemd-resolved (`resolvectl`). We shell out to the same tools an admin
// would use by hand, so behaviour is predictable and nothing needs root:
// NetworkManager's polkit policy already lets the active desktop user modify
// connections, and systemd-resolved lets any local user flush the cache.
//
// All changes target the connection that owns the default route — "the
// network this device is actually using" — so plugging into a different
// network or VPN doesn't silently get stale settings.

use tokio::process::Command;

/// The DNS state of the connection that owns the default route.
#[derive(Debug, Clone, Default)]
pub struct DnsState {
    /// Network device carrying the default route (e.g. "enp5s0").
    pub device: String,
    /// NetworkManager connection profile active on that device.
    pub connection: String,
    /// DNS servers currently forced on the connection. `None` means the
    /// connection uses whatever DHCP / the router advertises.
    pub custom: Option<Vec<String>>,
    /// Set when some *other* link (typically a VPN) is answering the device's
    /// DNS queries right now, so changes here won't take effect until it
    /// disconnects. Holds that link's name.
    pub dns_owner: Option<String>,
    /// True when the default route itself belongs to a tunnel (full-tunnel
    /// VPN). DNS overrides are never migrated onto tunnels.
    pub tunnel: bool,
    /// True when the connection forces DNS over TLS (`connection.dns-over-tls
    /// yes`), which systemd-resolved then uses for the servers above.
    pub dns_over_tls: bool,
}

/// NetworkManager property that makes systemd-resolved talk DNS over TLS to
/// the connection's servers. NM 1.34+; on older NM it does not exist, which
/// is why it is only ever written on its own, best-effort, when resetting.
const DOT_PROPERTY: &str = "connection.dns-over-tls";

/// Reset the connection's DNS-over-TLS flag to NetworkManager's default.
/// Best-effort: an NM too old to know the property just leaves it unset.
async fn clear_dot(connection: &str) {
    if let Err(e) = run(
        "nmcli",
        &["connection", "modify", connection, DOT_PROPERTY, "default"],
    )
    .await
    {
        tracing::debug!("could not reset {DOT_PROPERTY} on {connection}: {e}");
    }
}

async fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(cmd).args(args).output().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("`{cmd}` not found — is NetworkManager installed?")
        } else {
            format!("{cmd} failed to start: {e}")
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let what = args.first().copied().unwrap_or("");
        return Err(if stderr.is_empty() {
            format!("{cmd} {what} failed")
        } else {
            format!("{stderr}")
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The device that carries the default route, e.g. "enp5s0".
async fn default_route_device() -> Result<String, String> {
    let out = run("ip", &["route", "show", "default"]).await?;
    // "default via 192.168.1.1 dev enp5s0 proto dhcp ..."
    for line in out.lines() {
        let mut words = line.split_whitespace();
        while let Some(w) = words.next() {
            if w == "dev"
                && let Some(dev) = words.next()
            {
                return Ok(dev.to_string());
            }
        }
    }
    Err("no default route — is the network up?".to_string())
}

/// The NetworkManager connection profile active on `device`, plus whether the
/// device is a tunnel (wireguard/tun/vpn) rather than a real interface.
async fn connection_for(device: &str) -> Result<(String, bool), String> {
    let out = run(
        "nmcli",
        &["-g", "GENERAL.CONNECTION,GENERAL.TYPE", "device", "show", device],
    )
    .await?;
    let mut lines = out.lines();
    let name = lines.next().unwrap_or("").trim().to_string();
    let dtype = lines.next().unwrap_or("").trim().to_lowercase();
    if name.is_empty() || name == "--" {
        return Err(format!("no NetworkManager connection on {device}"));
    }
    let tunnel = matches!(dtype.as_str(), "wireguard" | "tun" | "tap" | "vpn" | "ip-tunnel");
    // `-g` escapes separator characters in values.
    Ok((name.replace("\\:", ":"), tunnel))
}

/// Remove any DNS override from a connection profile *without* touching the
/// live device — used to clean up the network we just left when the override
/// moves to a new one (the profile may well be inactive by then).
pub async fn clear_profile(connection: &str) -> Result<(), String> {
    run(
        "nmcli",
        &[
            "connection",
            "modify",
            connection,
            "ipv4.dns",
            "",
            "ipv4.ignore-auto-dns",
            "no",
            "ipv6.dns",
            "",
            "ipv6.ignore-auto-dns",
            "no",
        ],
    )
    .await?;
    clear_dot(connection).await;
    Ok(())
}

/// Read the current DNS state of the default-route connection.
pub async fn status() -> Result<DnsState, String> {
    let device = default_route_device().await?;
    let (connection, tunnel) = connection_for(&device).await?;

    let out = run(
        "nmcli",
        &[
            "-g",
            "ipv4.dns,ipv4.ignore-auto-dns,ipv6.dns,ipv6.ignore-auto-dns,connection.dns-over-tls",
            "connection",
            "show",
            &connection,
        ],
    )
    .await?;
    let mut lines = out.lines();
    let v4_dns = lines.next().unwrap_or("").trim().replace("\\:", ":");
    let v4_ignore = lines.next().unwrap_or("").trim() == "yes";
    let v6_dns = lines.next().unwrap_or("").trim().replace("\\:", ":");
    let v6_ignore = lines.next().unwrap_or("").trim() == "yes";
    // `-g` prints the enum numerically: -1 default, 0 no, 1 opportunistic,
    // 2 yes (older nmcli spell it out).
    let dns_over_tls = matches!(lines.next().unwrap_or("").trim(), "2" | "yes");

    let mut servers: Vec<String> = Vec::new();
    for list in [v4_dns, v6_dns] {
        servers.extend(
            list.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
    }

    let custom = ((v4_ignore || v6_ignore) && !servers.is_empty()).then_some(servers);
    let dns_owner = dns_owner(&device).await;
    Ok(DnsState {
        device,
        connection,
        custom,
        dns_owner,
        tunnel,
        dns_over_tls,
    })
}

/// If systemd-resolved routes the device's DNS through a different link than
/// `device` (a full-tunnel VPN, say), return that link's name. Best-effort:
/// on systems without resolvectl this simply reports None.
async fn dns_owner(device: &str) -> Option<String> {
    let out = run("resolvectl", &["dns"]).await.ok()?;
    let mut ours_has_dns = false;
    let mut other: Option<String> = None;
    for line in out.lines() {
        // "Link 5 (hk-office): 192.168.4.1" / "Link 4 (enp63s0):"
        let Some((head, servers)) = line.rsplit_once(':') else {
            continue;
        };
        let name = head
            .trim()
            .rsplit_once('(')
            .and_then(|(_, n)| n.strip_suffix(')'))
            .unwrap_or(head.trim());
        if servers.trim().is_empty() {
            continue;
        }
        if name == device {
            ours_has_dns = true;
        } else if !name.eq_ignore_ascii_case("global") && other.is_none() {
            other = Some(name.to_string());
        }
    }
    if ours_has_dns { None } else { other }
}

/// Push the modified profile onto the live device. `device reapply` updates
/// in place without dropping the link; if NetworkManager refuses (some
/// property combinations need a full re-activation), bounce the connection.
async fn reapply(device: &str, connection: &str) -> Result<(), String> {
    if run("nmcli", &["device", "reapply", device]).await.is_ok() {
        return Ok(());
    }
    tracing::warn!("`nmcli device reapply {device}` refused; re-activating {connection}");
    run("nmcli", &["connection", "up", connection]).await.map(|_| ())
}

/// Drop every cached lookup so the next resolution hits the new servers.
/// Best-effort by design: the switch itself already happened, so a missing
/// resolver tool shouldn't fail the operation — callers that *only* flush
/// surface the error instead.
pub async fn flush_cache() -> Result<(), String> {
    match run("resolvectl", &["flush-caches"]).await {
        Ok(_) => Ok(()),
        // Older name for the same tool, e.g. pre-24.04 systems.
        Err(first) => run("systemd-resolve", &["--flush-caches"])
            .await
            .map(|_| ())
            .map_err(|_| format!("could not flush DNS cache: {first}")),
    }
}

/// Force `servers` as the connection's DNS (IPv4 and IPv6 sorted into their
/// respective settings), suppress the router-advertised servers, apply live,
/// and flush the cache. Returns the resulting state.
///
/// A server may carry a TLS server name after `#` (`45.90.28.0#abc123.dns.nextdns.io`,
/// systemd-resolved's syntax, accepted verbatim by NetworkManager); with
/// `dns_over_tls` the connection is also switched to strict DNS over TLS, which
/// is how a NextDNS profile (or any DoT resolver) is reached.
pub async fn apply(servers: &[String], dns_over_tls: bool) -> Result<DnsState, String> {
    let device = default_route_device().await?;
    let (connection, _tunnel) = connection_for(&device).await?;

    // The SNI part has no colons, so an address is IPv6 iff its IP part has one.
    let (v6, v4): (Vec<&str>, Vec<&str>) = servers
        .iter()
        .map(String::as_str)
        .partition(|s| s.split('#').next().unwrap_or(s).contains(':'));

    let v4 = v4.join(",");
    let v6 = v6.join(",");
    let mut args = vec![
        "connection",
        "modify",
        connection.as_str(),
        "ipv4.dns",
        v4.as_str(),
        "ipv4.ignore-auto-dns",
        "yes",
        "ipv6.dns",
        v6.as_str(),
        "ipv6.ignore-auto-dns",
        "yes",
    ];
    if dns_over_tls {
        // Part of the main modify on purpose: if NM is too old for the
        // property the switch must fail loudly rather than silently send the
        // queries in the clear to a server that expects TLS.
        args.extend([DOT_PROPERTY, "yes"]);
    }
    run("nmcli", &args).await?;
    if !dns_over_tls {
        clear_dot(&connection).await;
    }

    reapply(&device, &connection).await?;
    if let Err(e) = flush_cache().await {
        tracing::warn!("{e}");
    }
    status().await
}

/// Clear the forced DNS and fall back to whatever the router/DHCP provides,
/// apply live, and flush the cache. Returns the resulting state.
pub async fn reset() -> Result<DnsState, String> {
    let device = default_route_device().await?;
    let (connection, _tunnel) = connection_for(&device).await?;

    run(
        "nmcli",
        &[
            "connection",
            "modify",
            &connection,
            "ipv4.dns",
            "",
            "ipv4.ignore-auto-dns",
            "no",
            "ipv6.dns",
            "",
            "ipv6.ignore-auto-dns",
            "no",
        ],
    )
    .await?;
    clear_dot(&connection).await;

    reapply(&device, &connection).await?;
    if let Err(e) = flush_cache().await {
        tracing::warn!("{e}");
    }
    status().await
}

#[cfg(test)]
mod live_tests {
    //! Round-trips the real default-route connection through DNS over TLS.
    //! Changes the machine's DNS for a few seconds, so it is `#[ignore]`d:
    //! run with `cargo test --release -- --ignored --nocapture live_dot_roundtrip`.
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn live_dot_roundtrip() {
        let before = status().await.expect("status");
        println!("before: {before:#?}");
        let original = before.custom.clone();

        let quad9: Vec<String> = ["9.9.9.9#dns.quad9.net", "149.112.112.112#dns.quad9.net"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let dot = apply(&quad9, true).await.expect("apply DoT");
        println!("after DoT: {dot:#?}");
        assert!(dot.dns_over_tls, "connection.dns-over-tls must read back as yes");
        assert_eq!(dot.custom.as_deref(), Some(&quad9[..]));

        let restored = match original {
            Some(servers) => apply(&servers, before.dns_over_tls).await.expect("re-apply original"),
            None => reset().await.expect("reset"),
        };
        println!("restored: {restored:#?}");
        assert_eq!(restored.custom, before.custom);
        assert_eq!(restored.dns_over_tls, before.dns_over_tls);
    }
}
