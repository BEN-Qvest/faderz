//! The configuration file: listeners, auth and API settings of
//! samplez-service, where the replicated state lives, the shape, the
//! declared consoles, and the MXL flows the engine reads and writes. The
//! auth settings in [`SETTINGS`] can also be given as command-line flags
//! or environment variables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use samplez_replica::membership::Voters;
use samplez_service::config::{ApiConfig, AuthConfig, AuthMode, Listener, Listeners, TlsMode};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::contract::{check, contract};
use crate::mix::Wiring;
use crate::shape::{ChannelTone, Shape, loudness_channels};
use crate::state::Console;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub listeners: Listeners,
    /// In `proxy` mode the API listener takes the user and groups the
    /// identity headers name, so it has to be reachable from the
    /// authenticating proxy alone: [`Config::parse`] refuses an API bind
    /// address other than loopback, where the proxy runs beside faderz.
    ///
    /// In `oidc` mode faderz signs users in against an OpenID Connect
    /// provider itself (samplez-service `OidcConfig`); `auth.oidc.operatorRole`
    /// defaults to [`DEFAULT_OPERATOR_ROLE`].
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub replica: ReplicaConfig,
    /// Model `Shape`.
    pub shape: Value,
    /// Consoles known before any registers: in `/consoles` from the start,
    /// named by this list, never removed. `protocol` defaults to `mcu`,
    /// `transport` to `bridge`, `strips` to 8.
    #[serde(default)]
    pub consoles: Vec<Console>,
    /// The MXL flows of the audio. Absent, faderz runs no engine: the
    /// state is served, nothing is mixed, and `/readyz` fails.
    #[serde(default)]
    pub mxl: Option<Mxl>,
    /// samplez's engine timing: `sampleRate`, `blockSize`,
    /// `readerMarginSamples`, `maxCatchupBlocks`, `pruneAfterBlocks`,
    /// `rampSamples`, `delayBudgetSamples`. samplez checks the values and
    /// supplies defaults, but for `readerMarginSamples`, [`READER_MARGIN`]
    /// here, and `delayBudgetSamples`, [`Config::delay_budget`].
    #[serde(default)]
    pub engine: Map<String, Value>,
    /// samplez's RT thread settings (`enabled`, `priority`, `rtCpu`,
    /// `isolateRtCpu`). With the RT thread pinned to `rtCpu`,
    /// `isolateRtCpu` (default true) keeps every thread started after it,
    /// faderz's own included, off that CPU.
    #[serde(default)]
    pub rt: Map<String, Value>,
    /// NMOS registration and IS-05 connections over MXL.
    #[serde(default)]
    pub nmos: Nmos,
    /// The engine configuration [`Config::parse`] built from the above,
    /// with a matrix of zeros: the matrix comes from the live state.
    #[serde(skip)]
    pub audio: Option<samplez::Config>,
    /// Each channel's input a and b flows and their domain, from the
    /// `mxl` section; empty without one.
    #[serde(skip)]
    pub wiring: Vec<Wiring>,
    /// The replica set [`Config::parse`] read from `replica`; none for a
    /// set of one.
    #[serde(skip)]
    pub set: Option<Set>,
    /// What [`Config::parse`] passed over, for the log.
    #[serde(skip)]
    pub notes: Vec<String>,
}

/// The variable naming the marker ConfigMap (`replica.marker`).
pub const MARKER_ENV: &str = "FADERZ_REPLICA_MARKER";
/// The variable carrying the pod's `controller-revision-hash` label, the
/// revision a rollout of a set compares with its StatefulSet's.
pub const REVISION_ENV: &str = "FADERZ_POD_REVISION";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mxl {
    /// The libmxl shared library, loaded at start.
    #[serde(default = "default_library_path")]
    pub library_path: PathBuf,
    pub domains: Vec<Domain>,
    /// Each channel's input, by channel id: the flows it reads, or a tone
    /// generator; every channel has one.
    pub inputs: BTreeMap<String, Input>,
    /// The flows each output of the shape writes, by output id; every
    /// output has them.
    pub outputs: BTreeMap<String, Output>,
}

fn default_library_path() -> PathBuf {
    PathBuf::from("/opt/mxl/lib/libmxl.so")
}

/// A mounted MXL domain.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Domain {
    pub name: String,
    pub path: PathBuf,
    /// The domain's MXL id, for NMOS; the domain's own `domain_def.json`
    /// gives it otherwise.
    #[serde(default)]
    pub id: Option<String>,
}

/// NMOS: an IS-04 node registering the channels' receivers and the
/// outputs' senders, and the IS-05 Connection API over MXL.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Nmos {
    #[serde(default)]
    pub enabled: bool,
    /// What every resource id derives from: the replica's name in a set
    /// of one, the StatefulSet's in a set of more, when absent.
    #[serde(default)]
    pub seed: Option<String>,
    /// The host controllers reach the NMOS listener at.
    #[serde(default)]
    pub advertise_host: String,
    /// 0 advertises the NMOS listener's port.
    #[serde(default)]
    pub advertise_port: u16,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_s: u64,
    #[serde(default)]
    pub registry: Registry,
    #[serde(default)]
    pub asset: Asset,
    /// Per ordinal, the `host:port` the other replicas of a set reach
    /// that replica's NMOS listener at, to forward staging to the
    /// leader; by default each `replica.peers` host on
    /// `listeners.nmos.port`.
    #[serde(default)]
    pub peers: Option<Vec<String>>,
}

impl Default for Nmos {
    fn default() -> Nmos {
        Nmos {
            enabled: false,
            seed: None,
            advertise_host: String::new(),
            advertise_port: 0,
            heartbeat_s: default_heartbeat(),
            registry: Registry::default(),
            asset: Asset::default(),
            peers: None,
        }
    }
}

fn default_heartbeat() -> u64 {
    5
}

/// The Registration API: `url`, or DNS-SD in `dnsSdDomain` or the
/// resolver's search domains, then mDNS.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registry {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub dns_sd_domain: Option<String>,
}

/// BCP-002-02 asset tags besides product and instance.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Asset {
    #[serde(default = "default_manufacturer")]
    pub manufacturer: String,
}

impl Default for Asset {
    fn default() -> Asset {
        Asset {
            manufacturer: default_manufacturer(),
        }
    }
}

fn default_manufacturer() -> String {
    "danielclasen".into()
}

/// The most seconds between two heartbeats `nmos.heartbeatS` takes: a
/// registry drops a node after 12 s without one.
const MAX_HEARTBEAT_S: u64 = 10;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    /// The flow of input a; required but for a tone input.
    #[serde(default)]
    pub flow: Option<String>,
    /// Makes the channel's input a line-up tone generator in place of a
    /// flow: it reads none, so it takes no `flow`, `altFlow`, `domain`
    /// or `channels`, and has no NMOS receiver.
    #[serde(default)]
    pub tone: Option<ToneInput>,
    /// The flow of input b; required exactly for a channel with
    /// `altInput`.
    #[serde(default)]
    pub alt_flow: Option<String>,
    /// Required with more than one domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// The 1-based flow channel each leg of the channel reads, in leg
    /// order, as many as the channel's width, each at most
    /// [`MAX_FLOW_CHANNEL`]; a number may repeat, so `[1, 1]` feeds one
    /// flow channel to both legs of a stereo channel. `flow` and `altFlow`
    /// are then read through the same selection and need at least as
    /// many channels as the highest number. Absent, each flow has exactly
    /// the channel's width in channels, read in order. The channel's
    /// `source` label in the shape gains the selection, as in `ch 7-8`.
    #[serde(default)]
    pub channels: Option<Vec<u32>>,
    /// The channel's delay line: the longest delay it takes, in ms at
    /// the shape's sample rate, at most 5 s at 48 kHz
    /// (`samplez_dsp::MAX_DELAY_SAMPLES`). Absent or 0, the channel has
    /// none and takes no delay but 0. Every line is allocated at start
    /// and counts against `engine.delayBudgetSamples`.
    #[serde(default)]
    pub max_delay_ms: Option<f64>,
}

/// `mxl.inputs.<channel>.tone`: the frequency and level the generator
/// starts at, within samplez's ranges (`samplez_dsp::TONE_FREQUENCY_HZ`,
/// `samplez_dsp::TONE_LEVEL_DB`); absent, samplez's 1 kHz at -18 dBFS.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToneInput {
    #[serde(default)]
    pub frequency: Option<u32>,
    #[serde(default)]
    pub level: Option<f64>,
}

/// The highest flow channel `mxl.inputs.<channel>.channels` may name:
/// samplez reads selections from the first 64 channels of a flow.
pub const MAX_FLOW_CHANNEL: u32 = 64;

/// The `source` label part naming the 1-based flow channels `legs`:
/// `ch 7`, a rising run as `ch 7-8`, others listed as `ch 1,4`.
fn source_channels(legs: &[u32]) -> String {
    let run = legs.windows(2).all(|w| w[1] == w[0] + 1);
    match legs {
        [one] => format!("ch {one}"),
        [first, .., last] if run => format!("ch {first}-{last}"),
        _ => {
            let list: Vec<String> = legs.iter().map(u32::to_string).collect();
            format!("ch {}", list.join(","))
        }
    }
}

/// The longest channel `source` label model `ChannelShape` takes.
const MAX_SOURCE: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Output {
    /// The flow a set of one replica writes.
    #[serde(default)]
    pub flow: Option<String>,
    /// The member flows of a set of more than one replica, by ordinal:
    /// as many as `replica.replicas`, each a flow no other output or
    /// input names. Each replica writes its own; a set writes no flow
    /// together.
    #[serde(default)]
    pub flows: Option<Vec<String>>,
    /// Required with more than one domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// The flow that merges the member flows of a set of more than one
    /// replica, which its NMOS sender names; required there with NMOS
    /// enabled.
    #[serde(default)]
    pub merged_flow: Option<String>,
    /// The output's delay line, for the delay of the bus or aux it
    /// carries: the longest delay it takes, in ms at the shape's sample
    /// rate, at most 5 s at 48 kHz, the same on every output carrying
    /// that element. Absent or 0, the output has none and the element
    /// takes no delay but 0. Allocated at start, it counts against
    /// `engine.delayBudgetSamples` once per leg of the output.
    #[serde(default)]
    pub max_delay_ms: Option<f64>,
}

/// `readerMarginSamples` when the configuration sets none: 32 ms. An
/// input read through a copy of a flow on another node has needed 1303
/// to 1399 samples, which samplez's default of 576 reads almost wholly
/// late; the margin delays every output by as much.
pub const READER_MARGIN: u32 = 1536;

/// The `engine` keys faderz passes to samplez; the others it sets itself.
const ENGINE_KEYS: [&str; 7] = [
    "sampleRate",
    "blockSize",
    "readerMarginSamples",
    "maxCatchupBlocks",
    "pruneAfterBlocks",
    "rampSamples",
    "delayBudgetSamples",
];

/// `delayBudgetSamples` when the configuration sets none: samplez's
/// default, 2^22. faderz sets it on the engine itself, so the engine and
/// the replica's peer connections, which read a donor's delay lines
/// within it, always take one value.
pub const DELAY_BUDGET: u64 = 1 << 22;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaConfig {
    /// The replica's id towards clients (model `ReplicaRef.id`); the host
    /// name when absent. In a set it is left out, so that each pod takes
    /// its pod name and the ordinal that name ends in; one given there
    /// needs `ordinal` too.
    pub name: Option<String>,
    /// Where the replicated log and snapshots live.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// The voters of the replica set, an odd count; 1, the default, is a
    /// set of one, which takes none of the keys below. A set starts on
    /// empty volumes: the data directory of a set of one cannot join a
    /// set, nor one of a set run alone, so changing between the two needs
    /// a new volume and leaves the state behind.
    #[serde(default = "one")]
    pub replicas: u16,
    /// This replica's StatefulSet ordinal; absent, the number after the
    /// last `-` of the replica name, as in a StatefulSet pod's name.
    pub ordinal: Option<u16>,
    /// Every replica's peer address, `host:port`, by ordinal: as many as
    /// `replicas`, each once. The StatefulSet's pods reach each other by
    /// their stable names, `<pod>.<headless service>:<port>`.
    #[serde(default)]
    pub peers: Vec<String>,
    /// The peer listener the other replicas reach this one on, on a port
    /// other than 0: TLS in `files` mode with a `clientCaFile`, whose
    /// certificates every replica's certificate is issued by. Each
    /// replica's certificate names the host of its `peers` address, which
    /// the others check.
    pub peer: Option<Listener>,
    /// The ConfigMap the set starts from, in the pod's namespace: ordinal
    /// 0 creates it, the others read it to join, so the pods' service
    /// account may get and create ConfigMaps there. `FADERZ_REPLICA_MARKER`
    /// sets it when the file does not; a set of one ignores the variable.
    /// Ordinal 0 starts a set only while a majority of the voters answer,
    /// so the StatefulSet starts its pods in parallel
    /// (`podManagementPolicy: Parallel`): one at a time, each waiting to be
    /// ready, the first would wait for the others forever.
    pub marker: Option<String>,
    /// The most a set's apply deadline may lead the engine's processed
    /// index, ms, in [`MAX_APPLY_LEAD_MS`]: samplez-replica's
    /// `maxApplyLeadMs`, read by a set of more than one replica that runs
    /// an engine.
    #[serde(default = "default_max_apply_lead_ms")]
    pub max_apply_lead_ms: u64,
}

/// The values `replica.maxApplyLeadMs` takes.
pub const MAX_APPLY_LEAD_MS: std::ops::RangeInclusive<u64> = 30..=120;

fn default_max_apply_lead_ms() -> u64 {
    50
}

impl Default for ReplicaConfig {
    fn default() -> Self {
        ReplicaConfig {
            name: None,
            data_dir: default_data_dir(),
            replicas: 1,
            ordinal: None,
            peers: Vec::new(),
            peer: None,
            marker: None,
            max_apply_lead_ms: default_max_apply_lead_ms(),
        }
    }
}

fn one() -> u16 {
    1
}

/// A replica set of more than one voter, as [`Config::parse`] checked it.
#[derive(Debug, Clone)]
pub struct Set {
    pub voters: Voters,
    pub ordinal: u16,
    /// By ordinal.
    pub peers: Vec<String>,
    pub peer: Listener,
    pub marker: String,
    /// The StatefulSet the replica's pod belongs to: the replica name
    /// without its `-<ordinal>`, when it ends in one.
    pub statefulset: Option<String>,
    /// The pod's `controller-revision-hash` label, from
    /// [`REVISION_ENV`].
    pub revision: Option<String>,
    /// `replica.maxApplyLeadMs`.
    pub max_apply_lead: std::time::Duration,
}

impl Set {
    /// This replica's own peer address.
    pub fn addr(&self) -> &str {
        &self.peers[usize::from(self.ordinal)]
    }
}

impl ReplicaConfig {
    /// The set these keys describe for the replica named `name`; none for
    /// a set of one, which takes none of the set's keys.
    fn set(
        &self,
        name: &str,
        env_marker: Option<String>,
        revision: Option<String>,
    ) -> Result<Option<Set>, String> {
        if self.replicas == 1 {
            let given = [
                ("ordinal", self.ordinal.is_some()),
                ("peers", !self.peers.is_empty()),
                ("peer", self.peer.is_some()),
                ("marker", self.marker.is_some()),
            ];
            return match given.iter().find(|(_, set)| *set) {
                Some((key, _)) => Err(format!(
                    "replica.{key}: a set of one replica takes none; set replica.replicas"
                )),
                None => Ok(None),
            };
        }
        let voters =
            Voters::new(self.replicas, false).map_err(|e| format!("replica.replicas: {e}"))?;
        let ordinal = match self.ordinal {
            Some(o) => o,
            // Every pod would read the same name, and take the same slot.
            None if self.name.is_some() => {
                return Err(
                    "replica.ordinal: required with replica.name in a set of more than one \
                     replica; leave both out to take them from the pod name"
                        .into(),
                );
            }
            None => name
                .rsplit_once('-')
                .and_then(|(_, n)| n.parse().ok())
                .ok_or(format!(
                    "replica.ordinal: absent, and the replica name {name:?} ends in no -<ordinal>"
                ))?,
        };
        if ordinal >= self.replicas {
            return Err(format!(
                "replica.ordinal {ordinal}: the set has ordinals 0 to {}",
                self.replicas - 1
            ));
        }
        if self.peers.len() != usize::from(self.replicas) {
            return Err(format!(
                "replica.peers: names {} addresses, the set has {} replicas",
                self.peers.len(),
                self.replicas
            ));
        }
        for (i, addr) in self.peers.iter().enumerate() {
            let port = addr
                .rsplit_once(':')
                .map(|(host, port)| (host, port.parse::<u16>()));
            if !matches!(port, Some((host, Ok(port))) if !host.is_empty() && port != 0) {
                return Err(format!("replica.peers[{i}] {addr:?}: not host:port"));
            }
            if self.peers[..i].contains(addr) {
                return Err(format!("replica.peers[{i}] {addr:?}: named twice"));
            }
        }
        let peer = self
            .peer
            .clone()
            .ok_or("replica.peer: required for a set of more than one replica")?;
        if peer.tls.mode != TlsMode::Files || peer.tls.client_ca_file.is_none() {
            return Err("replica.peer.tls: needs files mode and a clientCaFile".into());
        }
        if peer.port == 0 {
            return Err("replica.peer.port: 0 is no address the others can reach".into());
        }
        let marker = self
            .marker
            .clone()
            .or(env_marker)
            .ok_or("replica.marker: required for a set of more than one replica")?;
        let statefulset = (name.rsplit_once('-'))
            .filter(|(set, n)| !set.is_empty() && n.parse() == Ok(ordinal))
            .map(|(set, _)| set.to_owned());
        Ok(Some(Set {
            voters,
            ordinal,
            peers: self.peers.clone(),
            peer,
            marker,
            statefulset,
            revision,
            max_apply_lead: std::time::Duration::from_millis(self.max_apply_lead_ms),
        }))
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/faderz")
}

/// The role value `auth.oidc.operatorRole` takes when the file, the flags
/// and the environment leave it out.
pub const DEFAULT_OPERATOR_ROLE: &str = "faderz-operator";

/// How a [`Setting`]'s value is written at its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A string.
    Text,
    /// A list of strings, separated by commas or spaces.
    List,
}

/// A setting of the configuration file that a command-line flag or an
/// environment variable sets as well. The flag wins over the variable, the
/// variable over the file; an empty variable counts as unset.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub flag: &'static str,
    pub env: &'static str,
    /// Where in the configuration file it goes.
    pub path: &'static [&'static str],
    pub kind: Kind,
    pub help: &'static str,
}

const fn text(
    flag: &'static str,
    env: &'static str,
    path: &'static [&'static str],
    help: &'static str,
) -> Setting {
    Setting {
        flag,
        env,
        path,
        kind: Kind::Text,
        help,
    }
}

pub const SETTINGS: &[Setting] = &[
    text(
        "--auth-mode",
        "FADERZ_AUTH_MODE",
        &["auth", "mode"],
        "none, token, proxy or oidc",
    ),
    text(
        "--oidc-issuer-url",
        "FADERZ_OIDC_ISSUER_URL",
        &["auth", "oidc", "issuerUrl"],
        "the provider's issuer; required in oidc mode",
    ),
    text(
        "--oidc-client-id",
        "FADERZ_OIDC_CLIENT_ID",
        &["auth", "oidc", "clientId"],
        "required in oidc mode",
    ),
    text(
        "--oidc-client-secret-file",
        "FADERZ_OIDC_CLIENT_SECRET_FILE",
        &["auth", "oidc", "clientSecret", "file"],
        "the client secret's file; absent for a public client",
    ),
    text(
        "--oidc-public-url",
        "FADERZ_OIDC_PUBLIC_URL",
        &["auth", "oidc", "publicUrl"],
        "scheme://host[:port] browsers reach faderz under; required in oidc mode",
    ),
    text(
        "--oidc-role-claim",
        "FADERZ_OIDC_ROLE_CLAIM",
        &["auth", "oidc", "roleClaim"],
        "the claim carrying roles or groups; default groups",
    ),
    Setting {
        flag: "--oidc-scopes",
        env: "FADERZ_OIDC_SCOPES",
        path: &["auth", "oidc", "scopes"],
        kind: Kind::List,
        help: "scopes requested besides openid, comma- or space-separated",
    },
    text(
        "--oidc-operator-role",
        "FADERZ_OIDC_OPERATOR_ROLE",
        &["auth", "oidc", "operatorRole"],
        "the claim value granting operator; default faderz-operator",
    ),
    text(
        "--oidc-viewer-role",
        "FADERZ_OIDC_VIEWER_ROLE",
        &["auth", "oidc", "viewerRole"],
        "when set, required to sign in as viewer",
    ),
    Setting {
        flag: "--oidc-allowed-clients",
        env: "FADERZ_OIDC_ALLOWED_CLIENTS",
        path: &["auth", "oidc", "allowedClients"],
        kind: Kind::List,
        help: "clients a bearer access token's azp may name; default the client id",
    },
    text(
        "--oidc-audience",
        "FADERZ_OIDC_AUDIENCE",
        &["auth", "oidc", "audience"],
        "the audience a bearer access token must name; default none, not checked",
    ),
    text(
        "--oidc-session-ttl",
        "FADERZ_OIDC_SESSION_TTL",
        &["auth", "oidc", "sessionTtl"],
        "how long a session lasts: seconds, or with s, m, h or d; default 8h",
    ),
    text(
        "--oidc-session-key-file",
        "FADERZ_OIDC_SESSION_KEY_FILE",
        &["auth", "oidc", "sessionKey", "file"],
        "the session cookie key's file; replicas behind one host share it",
    ),
];

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run {
        config: PathBuf,
        /// `(flag, value)` of each [`SETTINGS`] flag given.
        flags: Vec<(String, String)>,
    },
    Version,
    Help,
}

/// Why the command line or the configuration was refused. Each variant
/// reads as the text the operator sees at startup.
#[derive(Debug)]
pub enum Error {
    /// An argument [`command`] does not know.
    UnknownArgument(String),
    /// A flag given last, with no value after it.
    MissingValue(String),
    /// No `--config <file>` among the arguments.
    NoConfig,
    /// The configuration file could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Not JSON, or JSON that is not a [`Config`].
    Json(serde_json::Error),
    /// A [`SETTINGS`] path that crosses a value which is not an object.
    NotAnObject(String),
    /// A value the configuration's own rules refuse.
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownArgument(arg) => write!(f, "unknown argument {arg:?}"),
            Error::MissingValue(flag) => write!(f, "{flag} needs a value"),
            Error::NoConfig => f.write_str("--config <file> is required"),
            Error::Read { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Json(e) => write!(f, "{e}"),
            Error::NotAnObject(path) => write!(f, "{path}: not an object"),
            Error::Invalid(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Read { source, .. } => Some(source),
            Error::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<String> for Error {
    fn from(why: String) -> Self {
        Error::Invalid(why)
    }
}

/// Parses the arguments after the program name: `--config <file>` and
/// any [`SETTINGS`] flag, each as `--flag value` or `--flag=value`, or
/// `--version` or `--help` alone.
pub fn command(args: &[String]) -> Result<Command, Error> {
    match args {
        [only] if only == "--version" => return Ok(Command::Version),
        [only] if only == "--help" || only == "-h" => return Ok(Command::Help),
        _ => {}
    }
    let mut config = None;
    let mut flags = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let known = flag == "--config" || SETTINGS.iter().any(|s| s.flag == flag);
        if !known {
            return Err(Error::UnknownArgument(arg.clone()));
        }
        let value = match inline {
            Some(v) => v,
            None => (args.next())
                .ok_or_else(|| Error::MissingValue(flag.to_owned()))?
                .clone(),
        };
        if flag == "--config" {
            config = Some(PathBuf::from(value));
        } else {
            flags.push((flag.to_owned(), value));
        }
    }
    let config = config.ok_or(Error::NoConfig)?;
    Ok(Command::Run { config, flags })
}

/// The usage text with every flag, its variable and its key.
pub fn usage() -> String {
    let mut text = String::from(
        "usage: faderz --config <file> [flags]\n       faderz --version\n\n\
         Each flag sets the configuration key named after it, as does its\n\
         environment variable; the flag wins over the variable, the variable\n\
         over the file. An empty variable counts as unset.\n\n",
    );
    for s in SETTINGS {
        text.push_str(&format!(
            "  {} <value>\n      {}, {}: {}\n",
            s.flag,
            s.env,
            s.path.join("."),
            s.help
        ));
    }
    text
}

/// Where a setting's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    /// The configuration file, or the default when the file has none.
    File,
}

/// The value of `setting` in `flags`, else in `env`.
fn given(
    setting: &Setting,
    flags: &[(String, String)],
    env: &impl Fn(&str) -> Option<String>,
) -> Option<(String, Source)> {
    flags
        .iter()
        .rev()
        .find(|(f, _)| f == setting.flag)
        .map(|(_, v)| (v.clone(), Source::Flag))
        .or_else(|| {
            env(setting.env)
                .filter(|v| !v.is_empty())
                .map(|v| (v, Source::Env))
        })
}

/// Where `auth.mode` comes from, and how to name it in the log.
pub fn auth_mode_source(
    flags: &[(String, String)],
    env: impl Fn(&str) -> Option<String>,
) -> String {
    let mode = SETTINGS
        .iter()
        .find(|s| s.path == ["auth", "mode"])
        .expect("auth.mode is a setting");
    match given(mode, flags, &env) {
        Some((_, Source::Flag)) => mode.flag.to_owned(),
        Some((_, Source::Env)) => mode.env.to_owned(),
        _ => "the configuration file".to_owned(),
    }
}

/// Sets each [`SETTINGS`] entry given in `flags`, else in `env`, at its
/// path in `config`, then, in `oidc` mode, [`DEFAULT_OPERATOR_ROLE`] where
/// `auth.oidc` has no operator role.
pub fn apply(
    config: &mut Value,
    flags: &[(String, String)],
    env: impl Fn(&str) -> Option<String>,
) -> Result<(), Error> {
    for s in SETTINGS {
        let Some((given, _)) = given(s, flags, &env) else {
            continue;
        };
        let value = match s.kind {
            Kind::Text => json!(given),
            Kind::List => json!(
                given
                    .split([',', ' '])
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
            ),
        };
        let (last, parents) = s.path.split_last().expect("a path has a key");
        let mut at = &mut *config;
        for key in parents {
            if !at.is_object() {
                return Err(Error::NotAnObject(s.path.join(".")));
            }
            at = at
                .as_object_mut()
                .expect("checked above")
                .entry(*key)
                .or_insert_with(|| json!({}));
            if at.is_null() {
                *at = json!({});
            }
        }
        match at.as_object_mut() {
            Some(o) => o.insert((*last).to_owned(), value),
            None => return Err(Error::NotAnObject(s.path.join("."))),
        };
    }
    let oidc_mode = config.pointer("/auth/mode").and_then(Value::as_str) == Some("oidc");
    if let Some(oidc) = config
        .pointer_mut("/auth/oidc")
        .and_then(Value::as_object_mut)
        .filter(|_| oidc_mode)
    {
        oidc.entry("operatorRole")
            .or_insert_with(|| json!(DEFAULT_OPERATOR_ROLE));
    }
    Ok(())
}

impl Config {
    /// Reads and checks `path` with the [`SETTINGS`] given in `flags` or
    /// the process environment on top: the shape against the contract and
    /// its own rules ([`Shape::parse`]), the replica name against model
    /// `Id`.
    pub fn load(path: &Path, flags: &[(String, String)]) -> Result<(Config, Shape, String), Error> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        Config::parse_with(&text, flags, |k| std::env::var(k).ok(), hostname)
    }

    /// [`Config::parse_with`] without flags or environment.
    pub fn parse(text: &str, host: fn() -> String) -> Result<(Config, Shape, String), Error> {
        Config::parse_with(text, &[], |_| None, host)
    }

    pub fn parse_with(
        text: &str,
        flags: &[(String, String)],
        env: impl Fn(&str) -> Option<String>,
        host: fn() -> String,
    ) -> Result<(Config, Shape, String), Error> {
        let mut value: Value = serde_json::from_str(text).map_err(Error::Json)?;
        apply(&mut value, flags, &env)?;
        let config: Config = serde_json::from_value(value).map_err(Error::Json)?;
        // In proxy mode the API takes who the identity headers name, so
        // only the proxy may reach it: the proxy runs beside faderz, as a
        // sidecar in its pod, and nothing else can connect to loopback.
        if config.auth.mode == AuthMode::Proxy && !config.listeners.api.bind.is_loopback() {
            return Err(Error::Invalid(format!(
                "auth.mode proxy needs listeners.api.bind on loopback, not {}: \
                 identity headers from anything but the proxy would be taken",
                config.listeners.api.bind
            )));
        }
        let mut shape = Shape::parse(&config.shape).map_err(|e| format!("shape: {e}"))?;
        for (i, c) in config.consoles.iter().enumerate() {
            check(&contract().console_status, &c.status(None, false, 0))
                .map_err(|e| format!("consoles[{i}]: {e}"))?;
            if config.consoles[..i].iter().any(|d| d.id == c.id) {
                return Err(Error::Invalid(format!(
                    "consoles[{i}].id {:?}: used twice",
                    c.id
                )));
            }
        }
        let mut config = config;
        if !MAX_APPLY_LEAD_MS.contains(&config.replica.max_apply_lead_ms) {
            return Err(Error::Invalid(format!(
                "replica.maxApplyLeadMs {}: not in {}..={}",
                config.replica.max_apply_lead_ms,
                MAX_APPLY_LEAD_MS.start(),
                MAX_APPLY_LEAD_MS.end()
            )));
        }
        let name = config.replica.name.clone().unwrap_or_else(host);
        check(&contract().id, &json!(name)).map_err(|e| {
            format!("replica.name {name:?}: {e}; name the replica in the configuration")
        })?;
        let env_marker = env(MARKER_ENV).filter(|m| !m.is_empty());
        if config.replica.replicas == 1 && env_marker.is_some() {
            config
                .notes
                .push(format!("{MARKER_ENV} ignored: replica.replicas is 1"));
        }
        let revision = env(REVISION_ENV).filter(|r| !r.is_empty());
        config.set = config.replica.set(&name, env_marker, revision)?;
        config.delay_lines(&mut shape)?;
        config.tone_inputs(&mut shape)?;
        config.audio = config.audio(&shape)?;
        if let (Some(engine), Some(mxl)) = (&config.audio, &config.mxl) {
            config.wiring = (shape.channels.iter().zip(&engine.inputs))
                .map(|(c, input)| match input.tone {
                    // Reads no flow: names none ([`crate::mix::selected`]).
                    Some(_) => Wiring {
                        a: String::new(),
                        b: None,
                        domain: String::new(),
                    },
                    None => Wiring {
                        a: input
                            .flow_id
                            .clone()
                            .expect("every configured flow input names a flow"),
                        b: mxl.inputs[&c.id].alt_flow.clone(),
                        domain: input
                            .domain
                            .clone()
                            .expect("every configured flow input names a domain"),
                    },
                })
                .collect();
        }
        config.label_sources(&mut shape)?;
        config.check_nmos()?;
        Ok((config, shape, name))
    }
}

impl Config {
    /// The engine's `delayBudgetSamples`: the configured one, else
    /// [`DELAY_BUDGET`]. One that is not a number is refused where the
    /// engine's configuration is checked.
    pub fn delay_budget(&self) -> u64 {
        (self.engine.get("delayBudgetSamples"))
            .and_then(Value::as_u64)
            .unwrap_or(DELAY_BUDGET)
    }

    /// Every flow an output names, member and merged flows included: what
    /// no input may read.
    pub fn own_flows(&self) -> std::collections::BTreeSet<String> {
        (self.mxl.iter())
            .flat_map(|m| m.outputs.values())
            .flat_map(|o| {
                (o.flow.iter())
                    .chain(o.flows.iter().flatten())
                    .chain(o.merged_flow.iter())
            })
            .cloned()
            .collect()
    }

    /// Per ordinal, where the replicas of a set reach each other's NMOS
    /// listener ([`Nmos::peers`]); empty for a set of one.
    pub fn nmos_peers(&self) -> Vec<String> {
        let Some(set) = &self.set else {
            return Vec::new();
        };
        match &self.nmos.peers {
            Some(peers) => peers.clone(),
            None => (set.peers.iter())
                .map(|p| {
                    let host = p.rsplit_once(':').map_or(p.as_str(), |(h, _)| h);
                    format!("{host}:{}", self.listeners.nmos.port)
                })
                .collect(),
        }
    }

    /// The NMOS settings that cannot work: NMOS without an engine, without
    /// an advertised host or port, over TLS, with a registry not on
    /// plain HTTP or a heartbeat outside 1 to [`MAX_HEARTBEAT_S`] s, or
    /// a set's output without its merged flow. A loopback listener with
    /// another advertised host is noted.
    fn check_nmos(&mut self) -> Result<(), String> {
        let nmos = &self.nmos;
        let merged = (self.mxl.iter()).flat_map(|m| m.outputs.iter());
        for (id, o) in merged {
            match (&o.merged_flow, &self.set) {
                (Some(_), None) => {
                    return Err(format!(
                        "mxl.outputs.{id}.mergedFlow: a set of one writes its `flow` itself"
                    ));
                }
                (None, Some(_)) if nmos.enabled => {
                    return Err(format!(
                        "mxl.outputs.{id}.mergedFlow: required with nmos.enabled in a set of \
                         more than one replica, whose replicas write member flows"
                    ));
                }
                _ => {}
            }
        }
        if !nmos.enabled {
            return Ok(());
        }
        let listener = &self.listeners.nmos;
        if self.mxl.is_none() {
            return Err("nmos.enabled: needs the mxl section, whose flows it connects".into());
        }
        if nmos.advertise_host.is_empty() {
            return Err("nmos.advertiseHost: required with nmos.enabled".into());
        }
        if nmos.advertise_port == 0 && listener.port == 0 {
            return Err("nmos.advertisePort: required with listeners.nmos.port 0".into());
        }
        if listener.tls.mode != TlsMode::None {
            return Err(
                "listeners.nmos.tls: the NMOS APIs are advertised as plain HTTP, mode none".into(),
            );
        }
        if let Some(url) = &nmos.registry.url
            && !url.starts_with("http://")
        {
            return Err(format!(
                "nmos.registry.url {url:?}: the registration client speaks plain HTTP only"
            ));
        }
        if let (Some(peers), Some(set)) = (&nmos.peers, &self.set)
            && peers.len() != usize::from(set.voters.replicas)
        {
            return Err(format!(
                "nmos.peers: names {} replicas, the set has {}",
                peers.len(),
                set.voters.replicas
            ));
        }
        if self.set.is_some() && nmos.peers.is_none() && listener.bind.is_loopback() {
            return Err(format!(
                "listeners.nmos.bind {}: the other replicas forward staging to it; \
                 bind it where they reach it or name their addresses in nmos.peers",
                listener.bind
            ));
        }
        if self.set.is_some() && nmos.peers.is_none() && listener.port == 0 {
            return Err("nmos.peers: required with listeners.nmos.port 0 in a set".into());
        }
        if !(1..=MAX_HEARTBEAT_S).contains(&nmos.heartbeat_s) {
            return Err(format!(
                "nmos.heartbeatS {}: not in 1..={MAX_HEARTBEAT_S}",
                nmos.heartbeat_s
            ));
        }
        let loopback = |h: &str| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        };
        if listener.bind.is_loopback() && !loopback(&nmos.advertise_host) {
            let note = format!(
                "nmos.advertiseHost {} is not loopback but listeners.nmos binds {}: \
                 nothing else reaches the advertised APIs",
                nmos.advertise_host, listener.bind
            );
            self.notes.push(note);
        }
        Ok(())
    }

    /// What a set's compatibility key hashes besides the versions
    /// ([`samplez_replica::rollout::key`]): `product`, the mixer's
    /// configuration hash, the `engine` settings and each channel's flow
    /// channel selection, which change the output a replica computes from
    /// the same log. The member flows, which differ by replica, do not.
    pub fn compatibility(&self, product: u64) -> Vec<u8> {
        let channels: BTreeMap<&String, &Option<Vec<u32>>> = (self.mxl.iter())
            .flat_map(|m| m.inputs.iter().map(|(id, i)| (id, &i.channels)))
            .collect();
        json!({ "product": product, "engine": self.engine, "channels": channels })
            .to_string()
            .into_bytes()
    }

    /// Sets each channel's `maxDelay` in `shape` from
    /// `mxl.inputs.<id>.maxDelayMs`, and each bus's and aux's from
    /// `mxl.outputs.<output>.maxDelayMs` of the outputs carrying it,
    /// which must agree, in samples at the shape's rate; refuses a line
    /// on an output carrying anything else, and lines that together hold
    /// more history than `engine.delayBudgetSamples`: a line times the
    /// width of its channel or output, summed over all, as samplez
    /// counts it.
    fn delay_lines(&self, shape: &mut Shape) -> Result<(), String> {
        if let Some(c) = shape.channels.iter().find(|c| c.max_delay.is_some()) {
            return Err(format!(
                "shape: channel {} maxDelay: set by mxl.inputs.{}.maxDelayMs",
                c.id, c.id
            ));
        }
        let configured = (shape.buses.iter().map(|b| (&b.id, b.max_delay)))
            .chain(shape.auxes.iter().map(|a| (&a.id, a.max_delay)))
            .chain(shape.monitor_outputs.iter().map(|m| (&m.id, m.max_delay)));
        if let Some((id, _)) = configured.into_iter().find(|(_, l)| l.is_some()) {
            return Err(format!(
                "shape: {id} maxDelay: set by mxl.outputs.<output>.maxDelayMs"
            ));
        }
        let Some(mxl) = &self.mxl else {
            return Ok(());
        };
        let rate = f64::from(shape.sample_rate);
        let samples = |at: &str, ms: f64| -> Result<u32, String> {
            let samples = (ms * rate / 1000.0).round();
            if !(0.0..=f64::from(samplez_dsp::MAX_DELAY_SAMPLES)).contains(&samples) {
                return Err(format!(
                    "{at}.maxDelayMs {ms}: not in 0 to {} samples at {} Hz",
                    samplez_dsp::MAX_DELAY_SAMPLES,
                    shape.sample_rate
                ));
            }
            if ms > 0.0 && samples == 0.0 {
                return Err(format!(
                    "{at}.maxDelayMs {ms}: under one sample at {} Hz; 0 for no line",
                    shape.sample_rate
                ));
            }
            Ok(samples as u32)
        };
        let mut lines = Vec::new();
        for c in &mut shape.channels {
            let Some(ms) = mxl.inputs.get(&c.id).and_then(|i| i.max_delay_ms) else {
                continue;
            };
            let line = samples(&format!("mxl.inputs.{}", c.id), ms)?;
            if line > 0 {
                c.max_delay = Some(line);
                lines.push((c.id.clone(), u64::from(line) * c.width as u64));
            }
        }
        // Each output's line, 0 for none; the outputs carrying one bus or
        // aux take one line.
        let mut carried: Vec<(String, u32, String)> = Vec::new();
        for o in &shape.outputs {
            let at = format!("mxl.outputs.{}", o.id);
            let ms = mxl.outputs.get(&o.id).and_then(|m| m.max_delay_ms);
            let line = ms.map_or(Ok(0), |ms| samples(&at, ms))?;
            if !shape.has_limiter(&o.source) {
                if line > 0 {
                    return Err(format!(
                        "{at}.maxDelayMs: the output carries {}, no bus or aux",
                        o.source
                    ));
                }
                continue;
            }
            if let Some((_, other, first)) = carried.iter().find(|(s, ..)| *s == o.source)
                && *other != line
            {
                return Err(format!(
                    "{at}.maxDelayMs: {line} samples, mxl.outputs.{first} carrying {} \
                     takes {other}; every output carrying one bus or aux takes one line",
                    o.source
                ));
            }
            carried.push((o.source.clone(), line, o.id.clone()));
            if line > 0 {
                lines.push((o.id.clone(), u64::from(line) * o.width as u64));
            }
        }
        for (source, line, _) in carried.iter().filter(|(_, l, _)| *l > 0) {
            let bus = shape.buses.iter_mut().find(|b| b.id == *source);
            match bus {
                Some(b) => b.max_delay = Some(*line),
                None => {
                    let aux = shape.auxes.iter_mut().find(|a| a.id == *source);
                    aux.expect("a bus or an aux").max_delay = Some(*line);
                }
            }
        }
        let held: u64 = lines.iter().map(|(_, s)| s).sum();
        let budget = self.delay_budget();
        if held > budget {
            let list: Vec<String> = (lines.iter())
                .map(|(id, s)| format!("{id} ({s})"))
                .collect();
            return Err(format!(
                "mxl: the delay lines of {} hold {held} samples times legs, above \
                 engine.delayBudgetSamples {budget}; shorten maxDelayMs or raise the budget",
                list.join(", ")
            ));
        }
        Ok(())
    }

    /// Refuses a configuration whose matrix the monitor outputs' meter
    /// columns alone take over samplez's coefficient budget
    /// ([`samplez::config::COEFFICIENTS_PER_BLOCK_SAMPLE`], a column in
    /// `duckColumns` counting [`samplez::config::DUCKED_TERM_COST`] times),
    /// naming them: without them `rows` by `columns`, `ducked` of them
    /// ducked, would fit. Every other excess is samplez's to refuse.
    fn monitor_meters_fit(
        &self,
        shape: &Shape,
        rows: usize,
        columns: usize,
        ducked: usize,
    ) -> Result<(), String> {
        use samplez::config::{COEFFICIENTS_PER_BLOCK_SAMPLE, DUCKED_TERM_COST};
        let block = (self.engine.get("blockSize").and_then(Value::as_u64)).unwrap_or(48) as usize;
        let cost =
            |columns: usize, ducked: usize| rows * columns + (DUCKED_TERM_COST - 1) * rows * ducked;
        let budget = block * COEFFICIENTS_PER_BLOCK_SAMPLE;
        let monitor: usize = shape.monitor_outputs.iter().map(|m| m.width).sum();
        let with = cost(columns, ducked);
        let ducked_monitor = if shape.ducks.is_empty() { 0 } else { monitor };
        if with <= budget || cost(columns - monitor, ducked - ducked_monitor) > budget {
            return Ok(());
        }
        Err(format!(
            "shape: the monitor meter columns ({monitor}) push the duck groups over the \
             coefficient budget at blockSize {block}: {with} matrix coefficients, duckColumns \
             {DUCKED_TERM_COST} times, at most {budget}; raise engine.blockSize to at least {} \
             or remove a duck group",
            with.div_ceil(COEFFICIENTS_PER_BLOCK_SAMPLE)
        ))
    }

    /// Sets each tone channel's `tone` in `shape` from
    /// `mxl.inputs.<id>.tone`: the frequency and level its generator
    /// starts at, samplez's where none is given, and refuses one outside
    /// samplez's ranges.
    fn tone_inputs(&self, shape: &mut Shape) -> Result<(), String> {
        if let Some(c) = shape.channels.iter().find(|c| c.tone.is_some()) {
            return Err(format!(
                "shape: channel {} tone: set by mxl.inputs.{}.tone",
                c.id, c.id
            ));
        }
        let Some(mxl) = &self.mxl else {
            return Ok(());
        };
        let start = samplez_dsp::ToneParams::DEFAULT;
        let hz = samplez_dsp::TONE_FREQUENCY_HZ;
        let db = samplez_dsp::TONE_LEVEL_DB;
        let (low, high) = (f64::from(*db.start()), f64::from(*db.end()));
        for c in &mut shape.channels {
            let Some(tone) = mxl.inputs.get(&c.id).and_then(|i| i.tone.as_ref()) else {
                continue;
            };
            let frequency = tone.frequency.unwrap_or(start.frequency_hz);
            if !hz.contains(&frequency) {
                return Err(format!(
                    "mxl.inputs.{}.tone.frequency {frequency}: not in {}..={} Hz",
                    c.id,
                    hz.start(),
                    hz.end()
                ));
            }
            let level = tone.level.unwrap_or(f64::from(start.level_db));
            if !(low..=high).contains(&level) {
                return Err(format!(
                    "mxl.inputs.{}.tone.level {level}: not in {low}..={high} dBFS",
                    c.id
                ));
            }
            c.tone = Some(ChannelTone { frequency, level });
        }
        Ok(())
    }

    /// Appends each channel's flow channel selection to its `source`
    /// label, or makes it the label.
    fn label_sources(&self, shape: &mut Shape) -> Result<(), String> {
        let Some(mxl) = &self.mxl else {
            return Ok(());
        };
        for c in &mut shape.channels {
            let Some(legs) = mxl.inputs.get(&c.id).and_then(|i| i.channels.as_deref()) else {
                continue;
            };
            let part = source_channels(legs);
            let label = match &c.source {
                Some(s) => format!("{s} {part}"),
                None => part,
            };
            if label.chars().count() > MAX_SOURCE {
                return Err(format!(
                    "shape: channel {} source {label:?}: with mxl.inputs.{}.channels \
                     appended it is longer than {MAX_SOURCE} characters",
                    c.id, c.id
                ));
            }
            c.source = Some(label);
        }
        Ok(())
    }

    /// The flows `output` names, by ordinal: one for a set of one, a
    /// member flow per replica for a set of more.
    fn member_flows(&self, at: &str, output: &Output) -> Result<Vec<String>, String> {
        match (&self.set, &output.flow, &output.flows) {
            (None, Some(flow), None) => Ok(vec![flow.clone()]),
            (None, _, Some(_)) => Err(format!(
                "{at}.flows: a set of one replica writes one flow, `flow`"
            )),
            (None, None, None) => Err(format!("{at}.flow: missing")),
            (Some(_), Some(_), _) => Err(format!(
                "{at}.flow: every replica of a set writes its own member flow, \
                 listed by ordinal in `flows`"
            )),
            (Some(_), None, None) => Err(format!("{at}.flows: missing")),
            (Some(set), None, Some(flows)) => {
                if flows.len() != usize::from(set.voters.replicas) {
                    return Err(format!(
                        "{at}.flows: names {} flows, the set has {} replicas",
                        flows.len(),
                        set.voters.replicas
                    ));
                }
                Ok(flows.clone())
            }
        }
    }

    /// The engine configuration of the `mxl` section, with a matrix of
    /// zeros: one input per channel reading its flow of input a, or a tone
    /// generator, then the line-up tone's generators
    /// ([`crate::mix::tone_widths`]), one
    /// output per output of the shape writing this replica's flow
    /// ([`Config::member_flows`]), one meter per bus, aux,
    /// mix-minus and monitor output no full-width output carries, and the columns a
    /// duck group can lower ([`crate::duck::columns`]). Every channel's
    /// input meters its true peak, before its strip as well as after it,
    /// and every stereo output and meter the correlation of its legs; the
    /// meter before the strip is reserved here, and runs while the
    /// channel's `meterTap` is input ([`crate::mix::pre_strip_meters`]). A
    /// channel with a direct out also feeds matrix rows before its strip
    /// ([`crate::mix::pre_rows`]), so the direct out's input tap is a state
    /// change alone.
    fn audio(&self, shape: &Shape) -> Result<Option<samplez::Config>, String> {
        let Some(mxl) = &self.mxl else {
            return Ok(None);
        };
        if let Some(k) = self
            .engine
            .keys()
            .find(|k| !ENGINE_KEYS.contains(&k.as_str()))
        {
            return Err(format!("engine.{k}: not a key faderz passes to samplez"));
        }
        let only = (mxl.domains.len() == 1).then(|| mxl.domains[0].name.clone());
        let domain = |at: String, given: &Option<String>| {
            given
                .clone()
                .or_else(|| only.clone())
                .ok_or(format!("{at}.domain: required with more than one domain"))
        };
        let mut inputs = Vec::new();
        for c in &shape.channels {
            let at = format!("mxl.inputs.{}", c.id);
            let input = mxl.inputs.get(&c.id).ok_or(format!("{at}: missing"))?;
            if input.tone.is_some() {
                let given = [
                    ("flow", input.flow.is_some()),
                    ("altFlow", input.alt_flow.is_some()),
                    ("domain", input.domain.is_some()),
                    ("channels", input.channels.is_some()),
                ];
                if let Some((key, _)) = given.iter().find(|(_, given)| *given) {
                    return Err(format!("{at}.{key}: a tone input reads no flow"));
                }
                if c.alt_input {
                    return Err(format!(
                        "{at}.tone: the channel has altInput, a tone input has no input b"
                    ));
                }
                inputs.push(json!({
                    "name": c.id,
                    "tone": {},
                    "channels": c.width,
                    "loudnessChannels": loudness_channels(&c.layout),
                    "maxDelaySamples": c.max_delay.unwrap_or(0),
                    "truePeak": true,
                    "preStripMeter": true,
                    "preStripRows": shape.has_direct_out(&c.id),
                }));
                continue;
            }
            let flow = (input.flow.as_ref()).ok_or(format!("{at}.flow: required without tone"))?;
            match (&input.alt_flow, c.alt_input) {
                (Some(_), false) => {
                    return Err(format!("{at}.altFlow: the channel has no altInput"));
                }
                // The state takes input b for it, which would read nothing.
                (None, true) => {
                    return Err(format!("{at}.altFlow: required, the channel has altInput"));
                }
                _ => {}
            }
            let mut engine_input = json!({
                "name": c.id,
                "domain": domain(at.clone(), &input.domain)?,
                "flowId": flow,
                "channels": c.width,
                "loudnessChannels": loudness_channels(&c.layout),
                "maxDelaySamples": c.max_delay.unwrap_or(0),
                "truePeak": true,
                "preStripMeter": true,
                "preStripRows": shape.has_direct_out(&c.id),
            });
            if let Some(legs) = &input.channels {
                if legs.len() != c.width {
                    return Err(format!(
                        "{at}.channels: names {} flow channels, the channel is {} wide",
                        legs.len(),
                        c.width
                    ));
                }
                if let Some(n) = legs.iter().find(|&&n| !(1..=MAX_FLOW_CHANNEL).contains(&n)) {
                    return Err(format!(
                        "{at}.channels: flow channel {n} is not in 1..={MAX_FLOW_CHANNEL}"
                    ));
                }
                engine_input["flowChannels"] = legs.iter().map(|n| n - 1).collect();
            }
            inputs.push(engine_input);
        }
        for width in crate::mix::tone_widths(shape) {
            inputs.push(json!({
                "name": crate::mix::tone_input(width),
                "tone": {},
                "channels": width,
            }));
        }
        let mut outputs = Vec::new();
        // Per output, the flow of each ordinal.
        let mut members: Vec<Vec<String>> = Vec::new();
        for o in &shape.outputs {
            let at = format!("mxl.outputs.{}", o.id);
            let output = mxl.outputs.get(&o.id).ok_or(format!("{at}: missing"))?;
            members.push(self.member_flows(&at, output)?);
            outputs.push(json!({
                "name": o.id,
                "domain": domain(at, &output.domain)?,
                "label": o.name,
                "group": o.id,
                "role": "Audio",
                "channels": o.width,
                "loudnessChannels": loudness_channels(&o.layout),
                "strip": shape.has_strip(&o.source),
                "maxDelaySamples": shape.delay_line(&o.source).filter(|_| shape.has_limiter(&o.source)).unwrap_or(0),
                "limiter": shape.has_limiter(&o.source),
                "correlation": o.layout == "stereo",
            }));
        }
        // Inputs may share a flow; an output's flow is written by this
        // engine alone.
        let read: std::collections::BTreeSet<&str> = (mxl.inputs.values())
            .flat_map(|i| i.flow.iter().chain(&i.alt_flow))
            .map(String::as_str)
            .collect();
        let mut written = std::collections::BTreeSet::new();
        let merged = mxl.outputs.values().filter_map(|o| o.merged_flow.as_ref());
        for flow in members.iter().flatten().chain(merged) {
            if read.contains(flow.as_str()) || !written.insert(flow.as_str()) {
                return Err(format!(
                    "mxl.outputs: flow {flow} is named by another output, replica or input"
                ));
            }
        }
        let columns = shape.meter_columns();
        if columns.len() > samplez::config::MAX_METERS {
            return Err(format!(
                "shape: {} engine meters, one per monitor output and per bus, aux and \
                 mix-minus output without a full-width output, and the engine takes at most \
                 {}; add full-width outputs for buses, auxes or mix-minus outputs",
                columns.len(),
                samplez::config::MAX_METERS
            ));
        }
        let meters: Vec<Value> = (columns.into_iter())
            .map(|(id, width, layout)| {
                json!({
                    "name": id,
                    "channels": width,
                    "loudnessChannels": loudness_channels(layout),
                    "correlation": layout == "stereo",
                })
            })
            .collect();
        if let Some(id) = mxl.inputs.keys().find(|id| !shape.is_channel(id)) {
            return Err(format!("mxl.inputs.{id}: not a channel of the shape"));
        }
        if let Some(id) =
            (mxl.outputs.keys()).find(|id| !shape.outputs.iter().any(|o| &o.id == *id))
        {
            return Err(format!("mxl.outputs.{id}: not an output of the shape"));
        }
        let rows: usize = (shape.channels.iter().map(|c| c.width))
            .chain(crate::mix::tone_widths(shape))
            .sum::<usize>()
            + crate::mix::pre_strip_rows(shape);
        let columns: usize = (shape.outputs.iter().map(|o| o.width))
            .chain(shape.meter_columns().iter().map(|m| m.1))
            .sum();
        let ducked = crate::duck::columns(shape).len();
        self.monitor_meters_fit(shape, rows, columns, ducked)?;
        let mut engine = self.engine.clone();
        engine
            .entry("readerMarginSamples")
            .or_insert(json!(READER_MARGIN));
        engine
            .entry("delayBudgetSamples")
            .or_insert(json!(DELAY_BUDGET));
        let mut v = Value::Object(engine);
        v["libmxl"] = json!(mxl.library_path);
        v["domains"] = mxl
            .domains
            .iter()
            .map(|d| json!({ "name": d.name, "path": d.path }))
            .collect();
        v["inputs"] = json!(inputs);
        v["meters"] = json!(meters);
        v["matrix"] = json!(vec![vec![0.0f32; columns]; rows]);
        v["duckColumns"] = json!(crate::duck::columns(shape));
        v["rt"] = Value::Object(self.rt.clone());
        // samplez checks the flows of each ordinal; this replica runs its
        // own.
        let ordinal = self.set.as_ref().map_or(0, |s| usize::from(s.ordinal));
        let mut engine = None;
        for n in 0..members.first().map_or(1, Vec::len) {
            for ((output, flows), o) in outputs.iter_mut().zip(&members).zip(&shape.outputs) {
                output["flowId"] = json!(flows[n]);
                // A member flow's label names its replica; the output's
                // NMOS sender keeps the output's own name.
                if flows.len() > 1 {
                    output["label"] = json!(format!("{} (replica {n})", o.name));
                }
            }
            v["outputs"] = json!(outputs);
            let parsed = samplez::Config::parse(&v.to_string()).map_err(|e| match &self.set {
                Some(_) => format!("engine of replica ordinal {n}: {}", e.0),
                None => format!("engine: {}", e.0),
            })?;
            if n == ordinal {
                engine = Some(parsed);
            }
        }
        let engine = engine.expect("the ordinal is below the set's replicas");
        if engine.sample_rate != shape.sample_rate {
            return Err(format!(
                "engine.sampleRate {} differs from the shape's {}",
                engine.sample_rate, shape.sample_rate
            ));
        }
        for (slot, c) in shape.channels.iter().enumerate() {
            if let Some(b) = &mxl.inputs[&c.id].alt_flow {
                engine
                    .check_input_flow(slot, b)
                    .map_err(|e| format!("mxl.inputs.{}.altFlow: {}", c.id, e.0))?;
            }
        }
        Ok(Some(engine))
    }
}

/// The host name, lowercased.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    match std::str::from_utf8(&buf[..end]) {
        Ok(name) if rc == 0 => name.to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests;
