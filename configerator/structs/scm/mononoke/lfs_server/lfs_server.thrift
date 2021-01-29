// @generated SignedSource<<92e40e04d36c5aa81af02d22e55d9352>>
// DO NOT EDIT THIS FILE MANUALLY!
// This file is a mechanical copy of the version in the configerator repo. To
// modify it, edit the copy in the configerator repo instead and copy it over by
// running the following in your fbcode directory:
//
// configerator-thrift-updater scm/mononoke/lfs_server/lfs_server.thrift

namespace rust mononoke.lfs_server.config

struct ObjectPopularity {
  1: string category;
  2: i32 window;
  3: i64 threshold;
}

// Generic throttle_limits based on ODS counter
struct ThrottleLimit {
  // ODS Counter to monitor
  1: string counter,
  // Limit to enforce. If the counter exceeds the limit, the client's request
  // will be rejected.
  2: i64 limit,
  // Sleep before returning a rate limiting error (in milliseconds). This is
  // useful if clients don't do their own backpressure.
  3: i64 sleep_ms,
  // A random amount of jitter is added to the above sleep time.
  // This is the upper limit on that additional sleep time, in milliseconds.
  4: i64 max_jitter_ms,
  // DEPRECATED: A list of client identities that the rate limit should be applied to
  5: list<string> client_identities,
  // Probability of this limit being applied, from 0 (not applied) to 100
  // (always applied).
  6: i64 probability_pct,
  // A list of client identitty sets that the rate limit should be applied to
  // If client identity set is a superset of one of the sets provided here,
  // they will be throttled.
  7: list<list<string>> client_identity_sets,
}

struct LfsServerConfig {
  // Whether or not to increment counters when sending bytes as opposed to when
  // accepting an upload.
  1: bool track_bytes_sent;

  // Whether or not to emit ?routing=SHA256 query strings when respond to a
  // batch request.
  2: bool enable_consistent_routing;

  // Don't use 3 and 4: these were used in the past.

  // Limits to apply
  5: list<ThrottleLimit> throttle_limits;

  // 6: deleted

  // Whether to enforce ACL checks.
  7: bool enforce_acl_check;

  // Whether to skip client hostname resolution and logging.
  8: bool disable_hostname_logging;

  // DEPRECATED: This is going away soon.
  // A SCS to use to record object popularity.
  9: string object_popularity_category

  // DEPRECATED: This is going away soon.
  // How many times an object must be requested in the last
  // OBJECT_POPULARITY_WINDOW seconds to be considered popular and therefore
  // excluded from consistent routing (see popularity.rs for
  // OBJECT_POPULARITY_WINDOW).
  10: i64 object_popularity_threshold;

  11: optional ObjectPopularity object_popularity;

  // The number of tasks to receive given content.
  12: i16 tasks_per_content;
}
