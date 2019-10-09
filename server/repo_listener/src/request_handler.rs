// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::mem;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cloned::cloned;
use configerator::ConfigeratorAPI;
use failure_ext::{prelude::*, SlogKVError};
use fbinit::FacebookInit;
use fbwhoami::FbWhoAmI;
use futures::{Future, Sink, Stream};
use futures_stats::Timed;
use lazy_static::lazy_static;
use limits::types::{MononokeThrottleLimit, MononokeThrottleLimits};
use maplit::{hashmap, hashset};
use slog::{self, error, o, Drain, Level, Logger};
use slog_ext::SimpleFormatWithError;
use slog_kvfilter::KVFilter;
use stats::{define_stats, Histogram};
use time_ext::DurationExt;
use tracing::{trace_args, TraceContext, Traced};
use uuid::Uuid;

use hgproto::{sshproto, HgProtoHandler};
use repo_client::RepoClient;
use scuba_ext::ScubaSampleBuilderExt;
use sshrelay::{SenderBytesWrite, SshEnvVars, Stdio};

use crate::repo_handlers::RepoHandler;

use context::{is_quicksand, CoreContext, Metric};
use hooks::HookManager;

lazy_static! {
    static ref DATACENTER_REGION_PREFIX: String = {
        FbWhoAmI::new()
            .expect("failed to init fbwhoami")
            .get_region_data_center_prefix()
            .expect("failed to get region from fbwhoami")
            .to_string()
    };
}

// It's made public so that the code that creates ConfigeratorAPI can subscribe to this category
pub const CONFIGERATOR_LIMITS_CONFIG: &str = "scm/mononoke/loadshedding/limits";
const CONFIGERATOR_TIMEOUT: Duration = Duration::from_millis(25);
const DEFAULT_PERCENTAGE: f64 = 100.0;

define_stats! {
    prefix = "mononoke.request_handler";
    wireproto_ms:
        histogram(500, 0, 100_000, AVG, SUM, COUNT; P 5; P 25; P 50; P 75; P 95; P 97; P 99),
}

pub fn request_handler(
    fb: FacebookInit,
    RepoHandler {
        logger,
        scuba,
        wireproto_logging,
        repo,
        hash_validation_percentage,
        lca_hint,
        phases_hint,
        preserve_raw_bundle2,
        pure_push_allowed,
        support_bundle2_listkeys,
    }: RepoHandler,
    stdio: Stdio,
    hook_manager: Arc<HookManager>,
    load_limiting_config: Option<(Arc<ConfigeratorAPI>, String)>,
) -> impl Future<Item = (), Error = ()> {
    let mut scuba_logger = scuba;
    let Stdio {
        stdin,
        stdout,
        stderr,
        mut preamble,
    } = stdio;

    let session_uuid = match preamble
        .misc
        .get("session_uuid")
        .and_then(|v| Uuid::parse_str(v).ok())
    {
        Some(session_uuid) => session_uuid,
        None => {
            let session_uuid = Uuid::new_v4();
            preamble
                .misc
                .insert("session_uuid".to_owned(), format!("{}", session_uuid));
            session_uuid
        }
    };

    // Info per wireproto command within this session
    let wireproto_calls = Arc::new(Mutex::new(Vec::new()));
    let trace = TraceContext::new(session_uuid, Instant::now());

    // Per-connection logging drain that forks output to normal log and back to client stderr
    let conn_log = {
        let stderr_write = SenderBytesWrite {
            chan: stderr.wait(),
        };
        let client_drain = slog_term::PlainSyncDecorator::new(stderr_write);
        let client_drain = SimpleFormatWithError::new(client_drain);
        let client_drain = KVFilter::new(client_drain, Level::Critical).only_pass_any_on_all_keys(
            (hashmap! {
                "remote".into() => hashset!["true".into(), "remote_only".into()],
            })
            .into(),
        );

        let server_drain = KVFilter::new(logger, Level::Critical).always_suppress_any(
            (hashmap! {
                "remote".into() => hashset!["remote_only".into()],
            })
            .into(),
        );

        // Don't fail logging if the client goes away
        let drain = slog::Duplicate::new(client_drain, server_drain).ignore_res();
        Logger::root(drain, o!("session_uuid" => format!("{}", session_uuid)))
    };

    scuba_logger.log_with_msg("Connection established", None);
    let client_hostname = preamble
        .misc
        .get("source_hostname")
        .cloned()
        .unwrap_or("".to_string());

    let ssh_env_vars = SshEnvVars::from_map(&preamble.misc);
    let load_limiting_config = match load_limiting_config {
        Some((configerator_api, category)) => {
            loadlimiting_configs(configerator_api, client_hostname, &ssh_env_vars)
                .map(|limits| (limits, category))
        }
        None => None,
    };

    let ctx = CoreContext::new(
        fb,
        session_uuid,
        conn_log,
        scuba_logger.clone(),
        trace.clone(),
        preamble.misc.get("unix_username").cloned(),
        ssh_env_vars,
        load_limiting_config,
    );

    // Construct a hg protocol handler
    let proto_handler = HgProtoHandler::new(
        ctx.clone(),
        stdin,
        RepoClient::new(
            repo.clone(),
            ctx.clone(),
            hash_validation_percentage,
            lca_hint,
            phases_hint,
            preserve_raw_bundle2,
            pure_push_allowed,
            hook_manager,
            support_bundle2_listkeys,
            wireproto_logging,
        ),
        sshproto::HgSshCommandDecode,
        sshproto::HgSshCommandEncode,
        wireproto_calls.clone(),
    );

    // send responses back
    let endres = proto_handler
        .inspect({
            cloned!(ctx);
            move |bytes| ctx.bump_load(Metric::EgressBytes, bytes.len() as f64)
        })
        .map_err(Error::from)
        .forward(stdout)
        .map(|_| ());

    // If we got an error at this point, then catch it and print a message
    endres
        .traced(&trace, "wireproto request", trace_args!())
        .timed(move |stats, result| {
            let mut wireproto_calls = wireproto_calls.lock().expect("lock poisoned");
            let wireproto_calls = mem::replace(&mut *wireproto_calls, Vec::new());

            STATS::wireproto_ms.add_value(stats.completion_time.as_millis_unchecked() as i64);
            scuba_logger
                .add_future_stats(&stats)
                .add("wireproto_commands", wireproto_calls);

            match result {
                Ok(_) => scuba_logger.log_with_msg("Request finished - Success", None),
                Err(err) => {
                    scuba_logger.log_with_msg("Request finished - Failure", format!("{:#?}", err));
                }
            }
            scuba_logger.log_with_trace(&trace)
        })
        .map_err(move |err| {
            error!(ctx.logger(), "Command failed";
                SlogKVError(err),
                "remote" => "true"
            );
        })
}

fn loadlimiting_configs(
    configerator_api: Arc<ConfigeratorAPI>,
    client_hostname: String,
    ssh_env_vars: &SshEnvVars,
) -> Option<MononokeThrottleLimit> {
    let is_quicksand = is_quicksand(&ssh_env_vars);

    let data = configerator_api
        .get_entity(CONFIGERATOR_LIMITS_CONFIG, CONFIGERATOR_TIMEOUT)
        .ok();
    data.and_then(|data| {
        let config: Option<MononokeThrottleLimits> = serde_json::from_str(&data.contents).ok();
        config
    })
    .and_then(|config| {
        let region_percentage = config
            .datacenter_prefix_capacity
            .get(&*DATACENTER_REGION_PREFIX)
            .copied()
            .unwrap_or(DEFAULT_PERCENTAGE);
        let host_scheme = hostname_scheme(client_hostname);
        let limit = config
            .hostprefixes
            .get(&host_scheme)
            .or(Some(&config.defaults))
            .copied();

        let multiplier = if is_quicksand {
            region_percentage / 100.0 * config.quicksand_multiplier
        } else {
            region_percentage / 100.0
        };

        match limit {
            Some(limit) => Some(MononokeThrottleLimit {
                egress_bytes: limit.egress_bytes * multiplier,
                ingress_blobstore_bytes: limit.ingress_blobstore_bytes * multiplier,
                total_manifests: limit.total_manifests * multiplier,
                quicksand_manifests: limit.quicksand_manifests * multiplier,
                getfiles_files: limit.getfiles_files * multiplier,
                getpack_files: limit.getpack_files * multiplier,
                commits: limit.commits * multiplier,
            }),
            _ => None,
        }
    })
}

/// Translates a hostname in to a host scheme:
///   devvm001.lla1.facebook.com -> devvm
///   hg001.lla1.facebook.com -> hg
fn hostname_scheme(hostname: String) -> String {
    let mut hostprefix = hostname.clone();
    let index = hostprefix.find(|c: char| !c.is_ascii_alphabetic());
    match index {
        Some(index) => hostprefix.truncate(index),
        None => {}
    }
    hostprefix
}
