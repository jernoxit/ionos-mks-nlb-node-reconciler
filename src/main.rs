// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The ionos-mks-nlb-node-reconciler Authors

//! ionos-mks-nlb-node-reconciler — keeps the target list of an IONOS Network
//! Load Balancer's forwarding rules in sync with the cluster's Ready worker nodes.
//!
//! Why: on a *public* IONOS Managed Kubernetes (MKS) cluster the Kubernetes node
//! InternalIP is the node's PUBLIC IP, but an IONOS Network Load Balancer needs the
//! node's PRIVATE target-LAN IP as its backend target. That private IP is only
//! readable by enumerating the VDC servers via the IONOS Cloud API (server name ==
//! k8s node name, `nics[].lan` == the target LAN → the private DHCP IP). MKS
//! replaces nodes on its weekly managed maintenance, so those private IPs drift —
//! which makes a continuously-running reconciler necessary. Something (e.g. your
//! Terraform/OpenTofu) owns the static NLB itself (ignoring changes to `targets`);
//! this controller owns EXCLUSIVELY the `targets[]` of the configured forwarding
//! rules.
//!
//! NB: IONOS confusingly also calls its single-node "assign a static public IP to
//! one node" MKS behaviour a "Network Load Balancer". This tool is for the *real*
//! IONOS Managed Network Load Balancer product (forwarding rules + a target pool),
//! not that single-node ingress trick.
//!
//! Design (deliberately minimal): 1 replica, node watch (kube-rs), no CRD, no
//! leader election. Robust despite the single replica: (a) a full reconcile on
//! startup (self-healing after a crash), (b) a periodic resync, (c) watcher restart
//! when the stream ends, (d) a liveness probe (last successful reconcile < N), and
//! (e) the NLB's own TCP health check carries edge liveness independently.

use std::{
    collections::HashMap,
    env,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::{Api, Client, runtime::watcher};
use serde_json::{Value, json};
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::Notify,
};
use tokio_util::sync::CancellationToken;

struct Config {
    api_url: String,
    token: String,
    dc_id: String,
    nlb_id: String,
    target_lan: String,
    rule_ids: Vec<String>,
    resync: Duration,
    health_port: u16,
}

impl Config {
    fn from_env() -> Result<Self> {
        let req = |k: &str| env::var(k).map_err(|_| anyhow!("env {k} is required"));
        let api_url = env::var("IONOS_API_URL")
            .unwrap_or_else(|_| "https://api.ionos.com/cloudapi/v6".to_string());
        let resync = env::var("RESYNC_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        let health_port = env::var("HEALTH_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token: req("IONOS_TOKEN")?,
            dc_id: req("DATACENTER_ID")?,
            nlb_id: req("NLB_ID")?,
            target_lan: req("TARGET_LAN_ID")?,
            rule_ids: req("RULE_IDS")?
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            resync: Duration::from_secs(resync),
            health_port,
        })
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Thin IONOS Cloud API client (no SDK needed — only GET/PATCH).
struct Ionos {
    http: reqwest::Client,
    cfg: Arc<Config>,
}

impl Ionos {
    fn new(cfg: Arc<Config>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, cfg })
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}{path}", self.cfg.api_url))
            .bearer_auth(&self.cfg.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Enumerate the VMs in the VDC → node name (== server name) → private
    /// target-LAN IP (`nics[].lan` == `target_lan`).
    async fn node_private_ips(&self) -> Result<HashMap<String, String>> {
        let v = self
            .get_json(&format!("/datacenters/{}/servers?depth=5", self.cfg.dc_id))
            .await
            .context("VDC server enumeration")?;
        let mut map = HashMap::new();
        for s in v["items"].as_array().into_iter().flatten() {
            let Some(name) = s["properties"]["name"].as_str() else {
                continue;
            };
            for nic in s["entities"]["nics"]["items"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let lan = nic["properties"]["lan"].as_i64().map(|l| l.to_string());
                if lan.as_deref() == Some(self.cfg.target_lan.as_str())
                    && let Some(ip) = nic["properties"]["ips"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                {
                    map.insert(name.to_string(), ip.to_string());
                }
            }
        }
        Ok(map)
    }

    /// The `listenerPort` of a forwarding rule (== the host port of the target).
    async fn rule_listener_port(&self, rule_id: &str) -> Result<u64> {
        let v = self
            .get_json(&format!(
                "/datacenters/{}/networkloadbalancers/{}/forwardingrules/{rule_id}?depth=1",
                self.cfg.dc_id, self.cfg.nlb_id
            ))
            .await?;
        v["properties"]["listenerPort"]
            .as_u64()
            .ok_or_else(|| anyhow!("rule {rule_id}: no listenerPort"))
    }

    async fn patch_targets(&self, rule_id: &str, targets: Value) -> Result<()> {
        self.http
            .patch(format!(
                "{}/datacenters/{}/networkloadbalancers/{}/forwardingrules/{rule_id}",
                self.cfg.api_url, self.cfg.dc_id, self.cfg.nlb_id
            ))
            .bearer_auth(&self.cfg.token)
            .json(&json!({ "targets": targets }))
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("rule {rule_id}: PATCH targets"))?;
        Ok(())
    }
}

fn node_ready(n: &Node) -> bool {
    n.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
        .unwrap_or(false)
}

async fn reconcile(cfg: &Config, ionos: &Ionos, nodes: &Api<Node>) -> Result<()> {
    let list = nodes.list(&Default::default()).await?;
    let ip_map = ionos.node_private_ips().await?;

    let mut ips = Vec::new();
    for n in &list {
        if !node_ready(n) {
            continue;
        }
        let Some(name) = n.metadata.name.as_deref() else {
            continue;
        };
        match ip_map.get(name) {
            Some(ip) => ips.push(ip.clone()),
            None => tracing::warn!(
                node = name,
                "no private target-LAN IP (not in the VDC server list yet?)"
            ),
        }
    }

    // EMPTY-TARGET GUARD: never PATCH an empty set — that would clear the NLB
    // targets and take the whole edge down. During a transient API hiccup or a
    // node replacement the existing (working) target list must be left in place.
    // The next reconcile (event/resync) corrects it. Partial shrinks (N→M, M>0)
    // are DELIBERATELY allowed — that is normal maintenance churn; only the jump
    // to 0 is pathological.
    if ips.is_empty() {
        tracing::warn!("empty target set — PATCH skipped (do not take the edge down)");
        return Ok(());
    }

    for rule_id in &cfg.rule_ids {
        let port = ionos.rule_listener_port(rule_id).await?;
        let targets: Vec<Value> = ips
            .iter()
            .map(|ip| {
                json!({
                    "ip": ip,
                    "port": port,
                    "weight": 1,
                    "proxyProtocol": "v2",
                    "healthCheck": { "check": true, "checkInterval": 2000, "maintenance": false }
                })
            })
            .collect();
        let n = targets.len();
        ionos.patch_targets(rule_id, Value::Array(targets)).await?;
        tracing::info!(rule = rule_id, port, targets = n, "targets set");
    }
    Ok(())
}

/// Minimal liveness responder (no extra dependency, no shell for distroless):
/// HTTP 200 as long as the last successful reconcile is < `max_age` ago,
/// otherwise 503 → the kubelet restarts the pod.
async fn health_server(
    port: u16,
    last_success: Arc<AtomicI64>,
    max_age: i64,
    cancel: CancellationToken,
) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("health port {port} not bindable: {e}");
            return;
        }
    };
    loop {
        let (mut sock, _) = tokio::select! {
            _ = cancel.cancelled() => return,
            a = listener.accept() => match a { Ok(x) => x, Err(_) => continue },
        };
        let stale = now_epoch() - last_success.load(Ordering::Relaxed) >= max_age;
        let (line, body) = if stale {
            ("503 Service Unavailable", "stale")
        } else {
            ("200 OK", "ok")
        };
        let resp = format!(
            "HTTP/1.1 {line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cfg = Arc::new(Config::from_env()?);
    let client = Client::try_default().await?;
    let ionos = Ionos::new(cfg.clone())?;
    let nodes: Api<Node> = Api::all(client.clone());

    let cancel = CancellationToken::new();
    let notify = Arc::new(Notify::new());
    // Seed = now → liveness has grace at boot until the first reconcile.
    let last_success = Arc::new(AtomicI64::new(now_epoch()));
    // Liveness trips if no reconcile succeeded for ~3 resync cycles.
    let max_age = (cfg.resync.as_secs() as i64 * 3).max(300);

    // Shutdown signal (SIGTERM/SIGINT) → CancellationToken (sticky, re-awaitable).
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("SIGTERM handler: {e}");
                    return;
                }
            };
            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("SIGINT handler: {e}");
                    return;
                }
            };
            tokio::select! { _ = sigterm.recv() => {}, _ = sigint.recv() => {} }
            tracing::info!("shutdown signal");
            cancel.cancel();
        });
    }

    tokio::spawn(health_server(
        cfg.health_port,
        last_success.clone(),
        max_age,
        cancel.clone(),
    ));

    // Watcher: every node event triggers a reconcile; re-establish on stream end.
    {
        let nodes = nodes.clone();
        let notify = notify.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                let mut stream = watcher(nodes.clone(), watcher::Config::default()).boxed();
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        ev = stream.next() => match ev {
                            Some(Ok(_)) => notify.notify_one(),
                            Some(Err(e)) => tracing::warn!("watcher: {e}"),
                            None => break, // stream ended → re-establish
                        }
                    }
                }
                tracing::warn!("watcher stream ended — restarting in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    // Periodic resync against missed events + drift.
    {
        let notify = notify.clone();
        let cancel = cancel.clone();
        let mut ticker = tokio::time::interval(cfg.resync);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = ticker.tick() => notify.notify_one(),
                }
            }
        });
    }

    tracing::info!(
        dc = %cfg.dc_id, nlb = %cfg.nlb_id, target_lan = %cfg.target_lan,
        rules = ?cfg.rule_ids, resync = ?cfg.resync, "started"
    );

    notify.notify_one(); // startup: full reconcile (self-healing after a restart).
    loop {
        // Wait for a trigger — shutdown has priority (biased) and interrupts at once.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = notify.notified() => {}
        }
        // Debounce + reconcile — both interruptible by shutdown (timely SIGTERM).
        tokio::select! {
            _ = cancel.cancelled() => break,
            res = async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                reconcile(&cfg, &ionos, &nodes).await
            } => match res {
                Ok(()) => last_success.store(now_epoch(), Ordering::Relaxed),
                Err(e) => {
                    tracing::error!("reconcile: {e:#} — retry in 15s");
                    let notify = notify.clone();
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = cancel.cancelled() => {},
                            _ = tokio::time::sleep(Duration::from_secs(15)) => notify.notify_one(),
                        }
                    });
                }
            }
        }
    }
    tracing::info!("stopped");
    Ok(())
}
