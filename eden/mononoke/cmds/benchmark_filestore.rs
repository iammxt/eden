/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]

use anyhow::{format_err, Error};
use blobstore::{Blobstore, PutBehaviour, DEFAULT_PUT_BEHAVIOUR};
use bytes::{Bytes, BytesMut};
use cacheblob::new_memcache_blobstore_no_lease;
use cached_config::ConfigStore;
use clap::{Arg, SubCommand};
use cmdlib::args::{self, MononokeMatches};
use context::CoreContext;
use fbinit::FacebookInit;
use filestore::{self, FetchKey, FilestoreConfig, StoreRequest};
use futures::{
    compat::Future01CompatExt,
    stream::{self, StreamExt, TryStreamExt},
};
use futures_stats::{FutureStats, TimedFutureExt};
use mononoke_types::{ContentMetadata, MononokeId};
use rand::Rng;
use sql_ext::facebook::{MysqlConnectionType, ReadConnectionType};
use sqlblob::Sqlblob;
use std::fmt::Debug;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use throttledblob::{ThrottleOptions, ThrottledBlob};
use tokio::{fs::File, io::BufReader};
use tokio_util::codec::{BytesCodec, FramedRead};

const NAME: &str = "benchmark_filestore";

const CMD_MANIFOLD: &str = "manifold";
const CMD_MEMORY: &str = "memory";
const CMD_XDB: &str = "xdb";

const ARG_MANIFOLD_BUCKET: &str = "manifold-bucket";
const ARG_SHARDMAP: &str = "shardmap";
const ARG_SHARD_COUNT: &str = "shard-count";
const ARG_MYROUTER_PORT: &str = "myrouter-port";
const ARG_USE_MYSQL_CLIENT: &str = "use-mysql-client";
const ARG_INPUT_CAPACITY: &str = "input-capacity";
const ARG_CHUNK_SIZE: &str = "chunk-size";
const ARG_CONCURRENCY: &str = "concurrency";
const ARG_MEMCACHE: &str = "memcache";
const ARG_CACHELIB_SIZE: &str = "cachelib-size";
const ARG_INPUT: &str = "input";
const ARG_DELAY: &str = "delay";
const ARG_RANDOMIZE: &str = "randomize";
const ARG_READ_QPS: &str = "read-qps";
const ARG_WRITE_QPS: &str = "write-qps";
const ARG_READ_COUNT: &str = "read-count";

fn log_perf<I, E: Debug>(stats: FutureStats, res: &Result<I, E>, len: u64) {
    match res {
        Ok(_) => {
            let bytes_per_ns = (len as f64) / (stats.completion_time.as_nanos() as f64);
            let mbytes_per_s = bytes_per_ns * (10_u128.pow(9) as f64) / (2_u128.pow(20) as f64);
            let gb_per_s = mbytes_per_s * 8_f64 / 1024_f64;
            eprintln!(
                "Success: {:.2} MB/s ({:.2} Gb/s) ({:?})",
                mbytes_per_s, gb_per_s, stats
            );
        }
        Err(e) => {
            eprintln!("Failure: {:?}", e);
        }
    };
}

async fn read<B: Blobstore>(
    blob: &B,
    ctx: &CoreContext,
    content_metadata: &ContentMetadata,
) -> Result<(), Error> {
    let key = FetchKey::Canonical(content_metadata.content_id);
    eprintln!(
        "Fetch start: {:?} ({:?} B)",
        key, content_metadata.total_size
    );

    let stream = filestore::fetch(blob, ctx.clone(), &key)
        .await?
        .ok_or(format_err!("Fetch failed: no stream"))?;

    let (stats, res) = stream.try_for_each(|_| async { Ok(()) }).timed().await;
    log_perf(stats, &res, content_metadata.total_size);

    // ignore errors - all we do is log them in `log_perf`
    match res {
        Ok(_) => Ok(()),
        Err(_) => Ok(()),
    }
}

async fn run_benchmark_filestore<'a>(
    ctx: &'a CoreContext,
    matches: &'a MononokeMatches<'a>,
    blob: Arc<dyn Blobstore>,
) -> Result<(), Error> {
    let input = matches.value_of(ARG_INPUT).unwrap().to_string();

    let input_capacity: usize = matches.value_of(ARG_INPUT_CAPACITY).unwrap().parse()?;

    let chunk_size: u64 = matches.value_of(ARG_CHUNK_SIZE).unwrap().parse()?;

    let concurrency: usize = matches.value_of(ARG_CONCURRENCY).unwrap().parse()?;

    let read_count: usize = matches.value_of(ARG_READ_COUNT).unwrap().parse()?;

    let delay: Option<Duration> = matches
        .value_of(ARG_DELAY)
        .map(|seconds| -> Result<Duration, Error> {
            let seconds = seconds.parse().map_err(Error::from)?;
            Ok(Duration::new(seconds, 0))
        })
        .transpose()?;

    let randomize = matches.is_present(ARG_RANDOMIZE);

    let config = FilestoreConfig {
        chunk_size: Some(chunk_size),
        concurrency,
    };

    eprintln!("Test with {:?}, writing into {:?}", config, blob);

    let file = File::open(input).await?;
    let metadata = file.metadata().await?;

    let data = BufReader::with_capacity(input_capacity, file);
    let data = FramedRead::new(data, BytesCodec::new()).map_ok(BytesMut::freeze);
    let len = metadata.len();

    let (len, data) = if randomize {
        let bytes = rand::thread_rng().gen::<[u8; 32]>();
        let bytes = Bytes::copy_from_slice(&bytes[..]);
        (
            len + (bytes.len() as u64),
            stream::iter(vec![Ok(bytes)]).chain(data).left_stream(),
        )
    } else {
        (len, data.right_stream())
    };

    eprintln!("Write start: {:?} B", len);

    let req = StoreRequest::new(len);

    let (stats, res) = filestore::store(&blob, config, ctx, &req, data.map_err(Error::from))
        .timed()
        .await;
    log_perf(stats, &res, len);

    let metadata = res?;

    match delay {
        Some(delay) => {
            tokio_shim::time::sleep(delay).await;
        }
        None => {}
    }

    eprintln!("Write committed: {:?}", metadata.content_id.blobstore_key());

    for _c in 0..read_count {
        read(&blob, ctx, &metadata).await?;
    }

    Ok(())
}

async fn get_blob<'a>(
    fb: FacebookInit,
    matches: &'a MononokeMatches<'a>,
    config_store: &ConfigStore,
    put_behaviour: PutBehaviour,
) -> Result<Arc<dyn Blobstore>, Error> {
    let blob: Arc<dyn Blobstore> = match matches.subcommand() {
        (CMD_MANIFOLD, Some(sub)) => {
            #[cfg(fbcode_build)]
            {
                use manifoldblob::{ManifoldBlob, ManifoldClientType};
                use prefixblob::PrefixBlobstore;

                let manifold_client_type =
                    if args::parse_blobstore_options(matches)?.manifold_use_cpp_client {
                        ManifoldClientType::CppThriftHybrid
                    } else {
                        ManifoldClientType::ThriftOnly
                    };
                let bucket = sub.value_of(ARG_MANIFOLD_BUCKET).unwrap();
                let manifold =
                    ManifoldBlob::new(fb, bucket, None, put_behaviour, manifold_client_type)
                        .map_err(|e| -> Error { e })?;
                let blobstore = PrefixBlobstore::new(manifold, format!("flat/{}.", NAME));
                Arc::new(blobstore)
            }
            #[cfg(not(fbcode_build))]
            {
                let _ = sub;
                unimplemented!("Accessing Manifold is not implemented in non fbcode builds");
            }
        }
        (CMD_MEMORY, Some(_)) => Arc::new(memblob::Memblob::default()),
        (CMD_XDB, Some(sub)) => {
            let shardmap = sub.value_of(ARG_SHARDMAP).unwrap().to_string();
            let shard_count = sub.value_of(ARG_SHARD_COUNT).unwrap().parse()?;
            let mysql_options = args::parse_mysql_options(&matches);
            let blobstore = match mysql_options.connection_type {
                MysqlConnectionType::Myrouter(port) => {
                    Sqlblob::with_myrouter(
                        fb,
                        shardmap,
                        port,
                        ReadConnectionType::Replica,
                        shard_count,
                        false,
                        put_behaviour,
                        config_store,
                    )
                    .compat()
                    .await?
                }
                MysqlConnectionType::Mysql(pool, config) => {
                    Sqlblob::with_mysql(
                        fb,
                        shardmap,
                        shard_count,
                        pool,
                        config,
                        ReadConnectionType::Replica,
                        false,
                        put_behaviour,
                        config_store,
                    )
                    .compat()
                    .await?
                }
                MysqlConnectionType::RawXDB => {
                    Sqlblob::with_raw_xdb_shardmap(
                        fb,
                        shardmap,
                        ReadConnectionType::Replica,
                        shard_count,
                        false,
                        put_behaviour,
                        config_store,
                    )
                    .compat()
                    .await?
                }
            };
            Arc::new(blobstore)
        }
        _ => unreachable!(),
    };

    let blob: Arc<dyn Blobstore> = if matches.is_present(ARG_MEMCACHE) {
        Arc::new(new_memcache_blobstore_no_lease(fb, blob, NAME, "")?)
    } else {
        blob
    };

    let blob: Arc<dyn Blobstore> = match matches.value_of(ARG_CACHELIB_SIZE) {
        Some(size) => {
            #[cfg(fbcode_build)]
            {
                use cacheblob::{new_cachelib_blobstore_no_lease, CachelibBlobstoreOptions};

                let cache_size_bytes = size.parse()?;
                cachelib::init_cache_once(fb, cachelib::LruCacheConfig::new(cache_size_bytes))?;

                let presence_pool = cachelib::get_or_create_pool(
                    "presence",
                    cachelib::get_available_space()? / 20,
                )?;
                let blob_pool =
                    cachelib::get_or_create_pool("blobs", cachelib::get_available_space()?)?;

                Arc::new(new_cachelib_blobstore_no_lease(
                    blob,
                    Arc::new(blob_pool),
                    Arc::new(presence_pool),
                    CachelibBlobstoreOptions::default(),
                ))
            }
            #[cfg(not(fbcode_build))]
            {
                let _ = size;
                unimplemented!("Using cachelib is not implemented for non fbcode build");
            }
        }
        None => blob,
    };

    let read_qps: Option<NonZeroU32> = matches
        .value_of(ARG_READ_QPS)
        .map(|v| v.parse())
        .transpose()?;

    let write_qps: Option<NonZeroU32> = matches
        .value_of(ARG_WRITE_QPS)
        .map(|v| v.parse())
        .transpose()?;

    let blob = ThrottledBlob::new(
        blob,
        ThrottleOptions {
            read_qps,
            write_qps,
            ..ThrottleOptions::default()
        },
    )
    .await;

    Ok(Arc::new(blob))
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let manifold_subcommand = SubCommand::with_name(CMD_MANIFOLD).arg(
        Arg::with_name(ARG_MANIFOLD_BUCKET)
            .takes_value(true)
            .required(false),
    );

    let memory_subcommand = SubCommand::with_name(CMD_MEMORY);
    let xdb_subcommand = SubCommand::with_name(CMD_XDB)
        .arg(
            Arg::with_name(ARG_SHARDMAP)
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name(ARG_SHARD_COUNT)
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name(ARG_MYROUTER_PORT)
                .long(ARG_MYROUTER_PORT)
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::with_name(ARG_USE_MYSQL_CLIENT)
                .long(ARG_USE_MYSQL_CLIENT)
                .required(false)
                .conflicts_with(ARG_MYROUTER_PORT),
        );

    let app = args::MononokeAppBuilder::new(NAME)
        .with_all_repos()
        .build()
        .arg(
            Arg::with_name(ARG_INPUT_CAPACITY)
                .long(ARG_INPUT_CAPACITY)
                .takes_value(true)
                .required(false)
                .default_value("8192"),
        )
        .arg(
            Arg::with_name(ARG_CHUNK_SIZE)
                .long(ARG_CHUNK_SIZE)
                .takes_value(true)
                .required(false)
                .default_value("1048576"),
        )
        .arg(
            Arg::with_name(ARG_CONCURRENCY)
                .long(ARG_CONCURRENCY)
                .takes_value(true)
                .required(false)
                .default_value("1"),
        )
        .arg(
            Arg::with_name(ARG_MEMCACHE)
                .long(ARG_MEMCACHE)
                .required(false),
        )
        .arg(
            Arg::with_name(ARG_CACHELIB_SIZE)
                .long(ARG_CACHELIB_SIZE)
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::with_name(ARG_DELAY)
                .long(ARG_DELAY)
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::with_name(ARG_RANDOMIZE)
                .long(ARG_RANDOMIZE)
                .required(false),
        )
        .arg(
            Arg::with_name(ARG_READ_QPS)
                .long(ARG_READ_QPS)
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::with_name(ARG_WRITE_QPS)
                .long(ARG_WRITE_QPS)
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::with_name(ARG_READ_COUNT)
                .long(ARG_READ_COUNT)
                .takes_value(true)
                .default_value("2")
                .required(true),
        )
        .arg(Arg::with_name(ARG_INPUT).takes_value(true).required(true))
        .subcommand(manifold_subcommand)
        .subcommand(memory_subcommand)
        .subcommand(xdb_subcommand);

    let matches = app.get_matches();

    let logger = args::init_logging(fb, &matches)?;
    let config_store = args::init_config_store(fb, &logger, &matches)?;
    let ctx = CoreContext::new_with_logger(fb, logger.clone());

    let mut runtime = tokio::runtime::Runtime::new().map_err(Error::from)?;

    let blob = runtime.block_on(get_blob(fb, &matches, config_store, DEFAULT_PUT_BEHAVIOUR))?;

    runtime.block_on(run_benchmark_filestore(&ctx, &matches, blob))?;

    Ok(())
}
