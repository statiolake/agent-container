//! Detect which Docker engine flavour is hosting us.
//!
//! The Rust broker listens on the host's loopback (see `server::spawn`);
//! the in-container tinyproxy reaches it through a hostname that has to
//! resolve, *from inside the container*, to *the host's loopback* — and
//! the right hostname is engine-specific.
//!
//! - **Docker Desktop** (Mac/Windows): `host.docker.internal` is injected
//!   into the container's resolver and routes through Docker Desktop's
//!   VM NAT straight to host loopback.
//! - **Rancher Desktop** (Mac/Windows, moby backend): `host.docker.internal`
//!   is also injected, but it points at the Lima VM's docker0 gateway —
//!   a different machine, where nothing is listening. The hostname that
//!   actually reaches host loopback is `host.lima.internal` (alias
//!   `host.rancher-desktop.internal`), which resolves to the Lima slirp
//!   gateway (~ `192.168.5.2`). slirp is a userspace NAT that forwards
//!   TCP into the host's loopback only — it is not bound to any
//!   physical NIC, so the broker stays unreachable from the LAN.
//! - **Native Linux Docker**: `host.docker.internal` is not defined by
//!   default. `compose.yml` adds `extra_hosts: host.docker.internal:
//!   host-gateway`, which resolves to the docker bridge gateway IP —
//!   the host on the bridge subnet, including loopback.

use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKind {
    DockerDesktop,
    RancherDesktop,
    NativeLinux,
}

impl HostKind {
    /// Probe the running Docker engine once at startup. We deliberately
    /// avoid a runtime fallback (e.g. try one hostname, fall back to the
    /// other on failure): mis-routing AWS credentials or MCP traffic
    /// silently is much worse than a clear, single-source-of-truth
    /// decision made up front.
    pub fn detect() -> Result<Self> {
        let out = Command::new("docker")
            .args(["info", "--format", "{{.Name}}"])
            .output()
            .context("failed to invoke `docker info` — is Docker (or Rancher Desktop) running?")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "`docker info --format '{{{{.Name}}}}'` failed: {}",
                stderr.trim()
            );
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Self::from_node_name(&name)
    }

    /// Map the Docker engine's reported node name onto the engine flavour.
    ///
    /// Both Docker Desktop and Rancher Desktop report a stable, well-known
    /// node name; anything else is assumed to be a native Linux docker
    /// daemon (where the node name is just the host's `uname -n`).
    pub fn from_node_name(name: &str) -> Result<Self> {
        match name {
            "docker-desktop" => Ok(Self::DockerDesktop),
            "lima-rancher-desktop" => Ok(Self::RancherDesktop),
            "" => {
                bail!("`docker info` returned an empty node name; cannot determine engine flavour")
            }
            _ => Ok(Self::NativeLinux),
        }
    }

    /// Hostname that resolves, from inside the compose project, to the
    /// host's loopback interface — i.e. where the broker is listening.
    ///
    /// Always paired with the broker's `127.0.0.1` bind in `server::spawn`;
    /// see the module-level docs for the per-engine reasoning.
    pub fn broker_host_name(self) -> &'static str {
        match self {
            Self::DockerDesktop => "host.docker.internal",
            // Rancher Desktop's own `host.docker.internal` resolves to the
            // Lima VM's docker0 gateway (typically 172.17.0.1), which is
            // the VM itself — *not* the host. The Lima slirp gateway
            // (typically 192.168.5.2), exposed as `host.lima.internal`,
            // is what tunnels back to the host's loopback listener.
            Self::RancherDesktop => "host.lima.internal",
            Self::NativeLinux => "host.docker.internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_desktop_name_recognised() {
        assert_eq!(
            HostKind::from_node_name("docker-desktop").unwrap(),
            HostKind::DockerDesktop
        );
    }

    #[test]
    fn rancher_desktop_name_recognised() {
        assert_eq!(
            HostKind::from_node_name("lima-rancher-desktop").unwrap(),
            HostKind::RancherDesktop
        );
    }

    #[test]
    fn arbitrary_node_name_falls_through_to_native_linux() {
        // `docker info`'s `Name` on a native Linux daemon is just the
        // host's `uname -n`. Anything that isn't one of the two known
        // hosted-VM markers belongs in this bucket.
        for n in ["some-laptop", "ci-runner-7", "ip-10-0-1-2"] {
            assert_eq!(HostKind::from_node_name(n).unwrap(), HostKind::NativeLinux);
        }
    }

    #[test]
    fn empty_node_name_is_an_error() {
        assert!(HostKind::from_node_name("").is_err());
    }

    #[test]
    fn host_name_differs_only_for_rancher() {
        // Sanity: Docker Desktop and native Linux both use
        // `host.docker.internal`; only Rancher Desktop diverges.
        assert_eq!(
            HostKind::DockerDesktop.broker_host_name(),
            "host.docker.internal"
        );
        assert_eq!(
            HostKind::NativeLinux.broker_host_name(),
            "host.docker.internal"
        );
        assert_eq!(
            HostKind::RancherDesktop.broker_host_name(),
            "host.lima.internal"
        );
    }
}
