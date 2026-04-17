use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fr_conformance::{
    CaseOutcome, HarnessConfig, LiveInfoContractCase, LiveInfoFieldComparison,
    LiveInfoFieldContract, LiveOptionalReplyCase, LiveOracleConfig, run_fixture,
    run_live_redis_diff, run_live_redis_diff_for_cases, run_live_redis_info_contract_diff,
    run_live_redis_multi_client_diff, run_live_redis_optional_reply_sequence_diff,
    run_protocol_fixture, run_replay_fixture, run_replication_handshake_fixture, run_smoke,
};
use fr_protocol::{RespFrame, parse_frame};
use fr_runtime::Runtime;

const CORE_SCAN_LIVE_STABLE_CASES: &[&str] = &[
    "scan_empty_store",
    "scan_wrong_arity",
    "scan_count_noncanonical_plus",
    "scan_count_noncanonical_leading_zero",
    "hscan_missing_key",
    "sscan_missing_key",
    "zscan_missing_key",
    "hscan_wrongtype",
    "sscan_wrongtype",
    "zscan_wrongtype",
    "hscan_wrong_arity",
    "sscan_wrong_arity",
    "zscan_wrong_arity",
    "hscan_wrong_arity_no_cursor",
    "sscan_wrong_arity_no_cursor",
    "zscan_wrong_arity_no_cursor",
    "scan_invalid_option",
];

const CORE_OBJECT_LIVE_STABLE_CASES: &[&str] = &[
    "object_encoding_string_int",
    "object_encoding_returns_int",
    "object_encoding_string_embstr",
    "object_encoding_returns_embstr",
    "object_encoding_string_raw_setup",
    "object_encoding_returns_raw",
    "object_encoding_hash_small_setup",
    "object_encoding_hash_listpack",
    "object_encoding_list_small_setup",
    "object_encoding_list_listpack",
    "object_encoding_set_intset_setup",
    "object_encoding_set_intset",
    "object_encoding_set_listpack_setup",
    "object_encoding_set_listpack",
    "object_encoding_zset_small_setup",
    "object_encoding_zset_listpack",
    "object_encoding_stream_setup",
    "object_encoding_stream",
    "object_encoding_missing_key",
    "object_refcount_string",
    "object_no_subcommand",
    "object_encoding_lowercase_subcommand",
    "object_encoding_mixedcase_subcommand",
    "object_encoding_hll_setup",
    "object_encoding_hll_raw",
    "object_encoding_geo_setup",
    "object_encoding_geo_skiplist",
];

const CORE_STREAM_LIVE_STABLE_CASES: &[&str] = &[
    "xlen_missing_key",
    "xrange_invalid_bound_error",
    "xrevrange_invalid_bound_error",
    "xdel_missing_key",
    "xdel_missing_key_invalid_id_zero",
    "xadd_explicit_setup",
    "xadd_lower_id_error",
    "xadd_equal_id_error",
    "xrange_count_zero",
    "xrange_missing_key",
    "xrevrange_missing_key",
    "xadd_read1_first",
    "xadd_read1_second",
    "xadd_read2_first",
    "xadd_partial_auto_first_entry",
    "xadd_partial_auto_same_ms_increments_seq",
    "xadd_partial_auto_same_ms_increments_again",
    "xadd_partial_auto_new_ms_resets_seq",
    "xadd_partial_auto_verify_length",
    "xadd_partial_auto_lower_ms_rejected",
];

const CORE_SCRIPTING_LIVE_STABLE_CASES: &[&str] = &[
    "eval_return_integer",
    "eval_return_string",
    "eval_client_setname_rejected_from_script",
    "eval_client_getname_rejected_from_script",
    "eval_client_id_rejected_from_script",
    "eval_return_nil",
    "eval_return_true_as_integer_1",
    "eval_return_false_as_nil",
    "eval_return_table_as_array",
    "eval_arithmetic",
    "eval_string_concat",
    "eval_local_variable",
    "eval_if_true_branch",
    "eval_if_false_branch",
    "eval_numeric_for_loop",
    "eval_keys_and_argv",
    "eval_argv_access",
    "eval_redis_call_set",
    "eval_redis_call_get",
    "eval_redis_call_incr",
    "eval_tonumber",
    "eval_tostring",
    "eval_type_function",
    "eval_string_len_operator",
    "eval_table_len_operator",
    "eval_math_floor",
    "eval_string_sub",
    "eval_string_upper",
    "eval_pcall_success",
    "eval_wrong_arity",
    "eval_invalid_numkeys",
    "eval_while_loop",
    "eval_repeat_until",
    "eval_table_insert_and_return",
    "eval_status_reply",
    "eval_error_reply",
    "eval_pcall_catches_error",
    "eval_closure_captures_upvalue",
    "eval_closure_shared_upvalue_counter",
    "eval_closure_returns_function_result",
    "eval_local_recursive_function",
    "eval_local_recursive_fibonacci",
    "eval_generic_for_ipairs",
    "eval_generic_for_pairs",
    "eval_nested_for_loops",
    "eval_for_loop_with_step",
    "eval_for_loop_negative_step",
    "eval_while_with_break",
    "eval_for_with_break",
    "eval_string_gmatch_basic",
    "eval_table_sort_basic",
    "eval_table_sort_custom_comparator",
    "eval_multiple_return_values_first",
    "eval_redis_call_with_keys_argv",
    "eval_string_format_basic",
    "eval_math_functions",
];

fn live_info_case(
    case_name: &str,
    required_sections: &[&str],
    field_contracts: &[LiveInfoFieldContract],
) -> LiveInfoContractCase {
    LiveInfoContractCase {
        case_name: case_name.to_string(),
        required_sections: required_sections
            .iter()
            .map(|section| (*section).to_string())
            .collect(),
        field_contracts: field_contracts.to_vec(),
    }
}

fn live_info_setup_case(case_name: &str) -> LiveInfoContractCase {
    live_info_case(case_name, &[], &[])
}

fn live_info_field(
    section: &str,
    field: &str,
    comparison: LiveInfoFieldComparison,
) -> LiveInfoFieldContract {
    LiveInfoFieldContract {
        section: section.to_string(),
        field: field.to_string(),
        comparison,
    }
}

const CORE_STRINGS_LIVE_STABLE_CASES: &[&str] = &[
    // Basic SET/GET
    "ping",
    "set_foo",
    "get_foo",
    // INCR/DECR
    "incr_counter_1",
    "incr_counter_2",
    // SET with NX/XX/GET options
    "set_nx_new_key",
    "set_nx_existing_key",
    "set_nx_verify_unchanged",
    "set_xx_existing",
    "set_xx_missing",
    "set_get_returns_old",
    "set_get_verify",
    "set_get_missing_key",
    // MSET/MGET
    "mset_multi",
    "mget_multi",
    "msetnx_all_new",
    "msetnx_one_exists",
    "msetnx_verify_unchanged",
    // INCRBYFLOAT/DECRBY/INCRBY
    "incrbyfloat_new_key",
    "incrbyfloat_add",
    "incrbyfloat_negative",
    "decrby_new_key",
    "decrby_existing",
    // STRLEN
    "strlen_existing",
    "strlen_missing",
    // SETRANGE/GETRANGE basics
    "setrange_extend",
    "getrange_result",
    "getrange_zero_prefix",
    // SETNX
    "setnx_new_key",
    "setnx_existing",
    "setnx_verify",
    // GETSET
    "getset_existing",
    "getset_verify",
    // Error cases
    "incr_on_non_integer",
    "wrong_arity",
    // APPEND
    "append_to_new_key",
    "append_to_existing",
    "append_verify",
    "append_empty_string",
    // STRLEN extended
    "strlen_nonexistent",
    "strlen_empty_value_setup",
    "strlen_empty_value",
    "strlen_wrong_arity",
    "strlen_wrongtype_setup",
    "strlen_wrongtype",
    // SETRANGE extended
    "setrange_setup",
    "setrange_middle",
    "setrange_verify",
    "setrange_extend_verify",
    "setrange_new_key",
    "setrange_new_key_verify",
    "setrange_zero_offset",
    "setrange_zero_verify",
    "setrange_wrong_arity",
    // GETRANGE extended
    "getrange_setup",
    "getrange_full",
    "getrange_substring",
    "getrange_negative_end",
    "getrange_negative_start",
    "getrange_out_of_bounds",
    "getrange_reversed_indices",
    "getrange_nonexistent_key",
    "getrange_wrong_arity",
    // GETDEL
    "getdel_setup",
    "getdel_returns_value",
    "getdel_key_gone",
    "getdel_nonexistent",
    "getdel_wrong_arity",
    // INCR/DECR new key
    "incr_new_key",
    "decr_new_key",
    "incrby_new_key",
    "decrby_new_key",
    "incr_not_integer_setup",
    "incr_not_integer",
    // GETSET extended
    "getset_setup",
    "getset_returns_old",
    "getset_verify_new",
    "getset_nonexistent_returns_nil",
    "getset_nonexistent_creates_key",
    // SUBSTR
    "substr_basic",
];

const CORE_LIST_LIVE_STABLE_CASES: &[&str] = &[
    // Basic LPUSH/RPUSH/LRANGE
    "lpush_single",
    "lpush_multiple",
    "lrange_all",
    "rpush_single",
    // LLEN
    "llen",
    "llen_missing_key",
    // LINDEX
    "lindex_zero",
    "lindex_negative",
    "lindex_out_of_range",
    // LPOP/RPOP
    "lpop_head",
    "rpop_tail",
    "lrange_after_pops",
    "lpop_empty_key",
    "rpop_empty_key",
    "lpop_with_count",
    "lpop_count_2",
    "rpop_count_2",
    // LSET
    "lset_element",
    "lindex_after_set",
    // LINSERT
    "linsert_setup",
    "linsert_before",
    "linsert_after",
    "linsert_missing_pivot",
    "linsert_missing_key",
    // LREM
    "lrem_setup",
    "lrem_positive_count",
    "lrange_after_lrem",
    // LTRIM
    "ltrim_setup",
    "ltrim_middle",
    "lrange_after_ltrim",
    // LPUSHX/RPUSHX
    "lpushx_existing",
    "lpushx_missing",
    "rpushx_existing",
    "rpushx_missing",
    // LMOVE
    "lmove_setup",
    "lmove_left_right",
    "lmove_verify_src",
    "lmove_verify_dst",
    // LPOS
    "lpos_setup",
    "lpos_basic",
    "lpos_missing_element",
    "lpos_rank_2",
    "lpos_rank_negative",
    "lpos_count_0",
    "lpos_count_2",
    "lpos_missing_key",
    // LMPOP (non-blocking)
    "lmpop_setup",
    "lmpop_left_single",
    "lmpop_right_single",
    "lmpop_left_count_2",
    "lmpop_verify_remaining",
    "lmpop_all_empty",
    // RPOPLPUSH
    "rpoplpush_setup",
    "rpoplpush_basic",
    "rpoplpush_verify_src",
    "rpoplpush_verify_dst",
    "rpoplpush_empty_src",
    "rpoplpush_same_key_setup",
    "rpoplpush_same_key",
    "rpoplpush_same_key_verify",
    "rpoplpush_wrong_arity",
    // LMOVE variants
    "lmove_rl_setup",
    "lmove_right_left",
    "lmove_right_right",
    "lmove_left_left",
    "lmove_empty_src",
    // Wrongtype errors
    "lpush_wrongtype_on_string",
    "lpush_wrongtype_error",
];

const CORE_HASH_LIVE_STABLE_CASES: &[&str] = &[
    // Basic HSET/HGET
    "hset_single_field",
    "hget_existing_field",
    "hget_missing_field",
    "hget_missing_key",
    "hset_multiple_fields",
    "hset_update_existing_field",
    "hget_updated_field",
    // HDEL
    "hdel_existing_field",
    "hdel_missing_field",
    "hdel_multiple_setup",
    "hdel_multiple_fields",
    "hdel_missing_key",
    // HEXISTS
    "hexists_present",
    "hexists_absent",
    "hexists_missing_key",
    // HLEN
    "hlen_hash",
    "hlen_missing_key",
    "hlen_after_hdel_multiple",
    // HMSET/HMGET
    "hmset_ok",
    "hmget_fields",
    "hmget_missing_key",
    // HINCRBY/HINCRBYFLOAT
    "hincrby_new_field",
    "hincrby_existing_field",
    "hincrby_negative",
    "hincrby_missing_key",
    "hincrby_on_non_integer",
    "hincrbyfloat_new_field",
    "hincrbyfloat_existing",
    "hincrbyfloat_negative",
    "hincrbyfloat_missing_key",
    // HSETNX
    "hsetnx_new_field",
    "hsetnx_existing_field",
    // HSTRLEN
    "hstrlen_existing",
    "hstrlen_missing",
    "hstrlen_missing_key",
    // HGETALL/HKEYS/HVALS
    "hgetall_setup",
    "hgetall_result",
    "hgetall_missing_key",
    "hkeys_setup",
    "hkeys_result",
    "hkeys_missing_key",
    "hvals_result",
    "hvals_missing_key",
    // HRANDFIELD
    "hrandfield_setup_single",
    "hrandfield_single_element",
    "hrandfield_missing_key",
    "hrandfield_count_zero",
    "hrandfield_missing_key_with_count",
    // Error cases
    "hset_wrongtype_on_string",
    "hset_wrongtype_error",
    "hset_wrong_arity",
    "hget_wrong_arity",
    "hdel_wrong_arity",
    "hmget_wrong_arity",
    "hexists_wrong_arity",
    "hlen_wrong_arity",
    "hmset_wrong_arity_odd",
    "hset_wrong_arity_odd",
    // Overflow/special cases
    "hincrby_overflow_setup",
    "hincrby_near_max",
    "hincrby_overflow_error",
    "hincrbyfloat_nan_error",
    // Empty hash behavior
    "hgetall_empty_after_delete",
    "hgetall_empty_delete_field",
    "hgetall_empty_hash_returns_empty",
];

const CORE_SET_LIVE_STABLE_CASES: &[&str] = &[
    // Basic SADD/SCARD/SISMEMBER
    "sadd_new_members",
    "sadd_duplicate_member",
    "scard",
    "scard_missing_key",
    "sismember_present",
    "sismember_absent",
    "sismember_missing_key",
    // SREM
    "srem_existing",
    "srem_absent",
    "srem_multiple",
    "srem_multiple_existing_and_missing",
    "srem_verify_remaining",
    // SMEMBERS
    "smembers_result",
    "smembers_missing_key",
    // SMISMEMBER
    "smismember_check",
    "smismember_missing_key",
    // Set operations setup
    "sadd_set2",
    // SINTER/SUNION/SDIFF
    "sinter_sets",
    "sunion_sets",
    "sdiff_sets",
    "sinter_with_nonexistent_key",
    "sunion_with_nonexistent_key",
    "sdiff_with_nonexistent_key",
    // SMOVE
    "smove_existing",
    "sismember_after_smove",
    "sismember_removed_by_smove",
    "smove_missing_member",
    "smove_nonexistent_source",
    // SINTERSTORE/SUNIONSTORE/SDIFFSTORE
    "sinterstore_dest",
    "sinterstore_src2",
    "sinterstore_result",
    "sunionstore_result",
    "sdiffstore_result",
    // SINTERCARD
    "sintercard_basic",
    "sintercard_with_limit",
    "sintercard_limit_zero",
    "sintercard_nokey",
    "sintercard_single_key",
    // SRANDMEMBER
    "srandmember_seed_single",
    "srandmember_single_element",
    "srandmember_count_zero",
    "srandmember_missing_key",
    "srandmember_missing_key_with_count",
    // SPOP
    "spop_seed_single",
    "spop_single_element",
    "spop_empty_after_pop",
    "spop_from_empty",
    "spop_missing_key",
    "spop_seed_count",
    "spop_with_count",
    "spop_missing_key_with_count",
    // Three-way operations
    "three_way_setup_c",
    "sdiff_three_sets",
    "sunion_three_sets",
    "sinter_three_sets",
    // Store operations verification
    "store_verify_setup_a",
    "store_verify_setup_b",
    "sinterstore_verify",
    "sinterstore_verify_members",
    "sunionstore_verify",
    "sunionstore_verify_members",
    "sdiffstore_verify",
    "sdiffstore_verify_members",
    "sinterstore_empty_input",
    // Error cases
    "sadd_wrongtype_on_string",
    "sadd_wrongtype_error",
    "sadd_wrong_arity",
    "srem_wrong_arity",
    "srandmember_wrong_arity",
    "spop_wrong_arity",
    "sinterstore_wrong_arity",
    // SMOVE wrongtype
    "smove_wrongtype_setup",
    "smove_wrongtype_dst",
];

const CORE_ZSET_LIVE_STABLE_CASES: &[&str] = &[
    // Basic ZADD/ZCARD/ZSCORE
    "zadd_new_members",
    "zadd_update_score",
    "zscore_existing",
    "zscore_missing_member",
    "zcard",
    "zcard_missing_key",
    // ZRANK/ZREVRANK
    "zrank_first",
    "zrank_last",
    "zrank_missing",
    "zrevrank_last",
    "zrevrank_first",
    "zrevrank_middle",
    "zrevrank_nonexistent_member",
    "zrevrank_nonexistent_key",
    // ZRANGE/ZREVRANGE
    "zrange_all",
    "zrevrange_all",
    "zrev_setup",
    "zrevrange_subset",
    "zrevrange_withscores",
    "zrevrange_nonexistent",
    "zrevrange_out_of_bounds",
    "zrange_withscores",
    "zrange_rev",
    "zrange_byscore",
    "zrange_byscore_limit",
    // ZRANGEBYSCORE/ZREVRANGEBYSCORE
    "zrangebyscore_range",
    "zrangebyscore_withscores",
    "zrangebyscore_limit",
    "zrangebyscore_exclusive_both",
    "zrangebyscore_exclusive_min",
    "zrangebyscore_exclusive_max",
    "zrangebyscore_inf_bounds",
    "zrevrangebyscore_all",
    "zrevrangebyscore_range",
    "zrevrangebyscore_exclusive",
    "zrevrangebyscore_limit",
    "zrevrangebyscore_withscores",
    "zrevrangebyscore_nonexistent",
    // ZCOUNT
    "zcount_range",
    "zcount_exclusive_both",
    "zcount_inclusive_all",
    "zcount_setup",
    "zcount_all",
    "zcount_exclusive",
    "zcount_exclusive_left",
    "zcount_nonexistent",
    "zcount_empty_range",
    // ZINCRBY
    "zincrby_existing",
    "zincrby_new_field",
    "zincrby_negative",
    "zincrby_new_key",
    // ZREM
    "zrem_member",
    "zrem_missing",
    // ZADD with options
    "zadd_nx_new_member",
    "zadd_nx_existing_member",
    "zscore_b_unchanged_by_nx",
    "zadd_xx_existing_member",
    "zscore_d_updated_by_xx",
    "zadd_xx_new_member_ignored",
    "zscore_e_not_added_by_xx",
    "zadd_gt_higher_score",
    "zscore_d_updated_by_gt",
    "zadd_gt_lower_score_rejected",
    "zscore_d_unchanged_by_gt",
    "zadd_lt_lower_score",
    "zscore_d_updated_by_lt",
    "zadd_lt_higher_score_rejected",
    "zadd_ch_returns_changed_count",
    "zadd_gt_setup",
    "zadd_gt_higher_updates",
    "zadd_gt_verify_higher",
    "zadd_gt_lower_noop",
    "zadd_gt_verify_unchanged",
    "zadd_lt_lower_updates",
    "zadd_lt_verify_lower",
    "zadd_lt_higher_noop",
    "zadd_lt_verify_unchanged",
    "zadd_xx_gt_setup",
    "zadd_xx_gt_higher_updates",
    "zadd_xx_gt_verify_updated",
    "zadd_xx_gt_lower_noop",
    "zadd_xx_gt_verify_unchanged",
    "zadd_xx_gt_new_member_ignored",
    "zadd_xx_gt_verify_no_new_member",
    "zadd_xx_lt_setup",
    "zadd_xx_lt_lower_updates",
    "zadd_xx_lt_verify_updated",
    "zadd_xx_lt_higher_noop",
    "zadd_xx_lt_verify_unchanged",
    "zadd_ch_nx_setup",
    "zadd_ch_nx_new_returns_1",
    "zadd_ch_nx_existing_returns_0",
    "zadd_ch_xx_setup",
    "zadd_ch_xx_update_returns_1",
    "zadd_ch_xx_same_score_returns_0",
    "zadd_ch_xx_nonexistent_returns_0",
    "zadd_incr_basic",
    "zadd_incr_existing",
    "zadd_incr_negative",
    "zadd_incr_nx_new",
    "zadd_incr_nx_existing_returns_nil",
    "zadd_incr_xx_existing",
    "zadd_incr_xx_nonexistent_returns_nil",
    "zadd_incr_gt_higher",
    "zadd_incr_gt_lower_returns_nil",
    "zadd_options_case_insensitive",
    // ZPOPMIN/ZPOPMAX
    "zpopmin_single",
    "zpopmin_result",
    "zpopmax_result",
    "zpopmin_empty",
    "zpopmin_with_count_setup",
    "zpopmin_with_count",
    "zpopmax_with_count",
    "zpopmin_count_exceeds_setup",
    "zpopmin_count_exceeds",
    "zpopmin_empty_after",
    "zpopmax_empty",
    "zpopmin_count_setup",
    "zpopmin_count_2",
    "zpopmax_count_2",
    "zpopmin_empty_key",
    "zpopmax_empty_key",
    "zpopmin_count_larger_setup",
    "zpopmin_count_100",
    "zpopmin_count_zero_setup",
    "zpopmin_count_0_returns_empty",
    // ZMSCORE
    "zmscore_setup",
    "zmscore_result",
    "zmscore_mixed",
    "zmscore_missing_key",
    // Set operations
    "zunionstore_setup1",
    "zunionstore_setup2",
    "zunionstore_result",
    "zinterstore_result",
    "zinterstore_verify_score",
    "zunionstore_weights_setup1",
    "zunionstore_weights_setup2",
    "zunionstore_with_weights",
    "zunionstore_weights_verify_a",
    "zunionstore_weights_verify_b",
    "zunionstore_weights_verify_c",
    "zinterstore_with_weights",
    "zinterstore_weights_verify_b",
    "zunionstore_aggregate_min",
    "zunionstore_aggregate_min_verify_b",
    "zunionstore_aggregate_max",
    "zunionstore_aggregate_max_verify_b",
    "zunionstore_nonexistent_source",
    "zinterstore_nonexistent_source",
    "zinterstore_basic",
    "zinterstore_verify_b",
    "zinterstore_verify_c",
    "zinterstore_weights",
    "zinterstore_weights_verify",
    "zinterstore_aggregate_min",
    "zinterstore_aggregate_min_verify",
    // ZDIFF/ZDIFFSTORE
    "zdiff_basic",
    "zdiff_withscores",
    "zdiff_nonexistent",
    "zdiffstore_setup_a",
    "zdiffstore_setup_b",
    "zdiffstore_result",
    "zdiffstore_verify",
    "zdiffstore_basic",
    // ZUNION/ZINTER
    "zunion_basic",
    "zinter_basic",
    "zinter_withscores",
    "zunion_withscores",
    "zinter_withscores_sum",
    // ZINTERCARD
    "zintercard_setup_a",
    "zintercard_setup_b",
    "zintercard_full",
    "zintercard_limit_one",
    "zintercard_limit_zero",
    "zintercard_nonexistent",
    // ZRANGESTORE
    "zrangestore_setup",
    "zrangestore_by_rank",
    "zrangestore_verify",
    "zrangestore_byscore_setup",
    "zrangestore_byscore",
    "zrangestore_byscore_verify",
    "zrangestore_basic",
    "zrangestore_nonexistent",
    // LEX operations
    "zlex_setup",
    "zrangebylex_all",
    "zrangebylex_inclusive_range",
    "zrangebylex_exclusive_range",
    "zrangebylex_mixed_range",
    "zrangebylex_with_limit",
    "zrangebylex_empty_range",
    "zrangebylex_nonexistent_key",
    "zrevrangebylex_all",
    "zrevrangebylex_inclusive_range",
    "zrevrangebylex_exclusive_range",
    "zrevrangebylex_with_limit",
    "zrevrangebylex_nonexistent_key",
    "zlexcount_all",
    "zlexcount_inclusive_range",
    "zlexcount_exclusive_range",
    "zlexcount_mixed_range",
    "zlexcount_nonexistent_key",
    "zrangebylex_limit_setup",
    "zrangebylex_limit_offset_1_count_2",
    "zrangebylex_limit_count_unlimited",
    // ZREMRANGEBYRANK
    "zremrangebyrank_setup",
    "zremrangebyrank_middle",
    "zremrangebyrank_verify",
    "zremrangebyrank_negative_indices_setup",
    "zremrangebyrank_negative_indices",
    "zremrangebyrank_negative_verify",
    "zremrangebyrank_nonexistent_key",
    "zremrangebyrank_all_setup",
    "zremrangebyrank_all",
    "zremrangebyrank_all_verify_empty",
    // ZREMRANGEBYLEX
    "zremrangebylex_setup",
    "zremrangebylex_inclusive",
    "zremrangebylex_verify",
    "zremrangebylex_exclusive_setup",
    "zremrangebylex_exclusive",
    "zremrangebylex_exclusive_verify",
    "zremrangebylex_nonexistent_key",
    "zremrangebylex_all_setup",
    "zremrangebylex_all",
    // ZREMRANGEBYSCORE
    "zremrangebyscore_setup",
    "zremrangebyscore_populate",
    "zremrangebyscore_range",
    "zremrangebyscore_remaining",
    "zremrangebyscore_exclusive",
    "zremrangebyscore_exclusive_range",
    "zremrangebyscore_exclusive_remaining",
    "zremrangebyscore_inf",
    "zremrangebyscore_neg_inf_to_plus_inf",
    "zremrangebyscore_nonexistent_key",
    "zcard_after_remrangebyscore",
    "zrange_after_remrangebyscore",
    // ZSCAN
    "zscan_setup",
    "zscan_full",
    "zscan_missing_key",
    // ZMPOP - setup and verification only (result format varies)
    "zmpop_setup_a",
    "zmpop_verify_remaining",
    "zmpop_setup_b",
    "zmpop_verify_empty",
    "zmpop_nonexistent_key",
    "zmpop_multi_key_setup",
    // ZRANGEBYSCORE LIMIT
    "zrangebyscore_limit_setup",
    "zrangebyscore_limit_offset_zero_count_all",
    "zrangebyscore_limit_offset_2_count_2",
    "zrangebyscore_limit_offset_beyond_range",
    "zrangebyscore_limit_count_zero",
    "zrangebyscore_limit_offset_1_count_unlimited",
    "zrevrangebyscore_limit_offset_1_count_2",
    "zrangebyscore_limit_withscores",
    "zrangebyscore_limit_cleanup",
    // Error cases (avoiding ones with error message variations)
    "zadd_wrongtype_on_string",
    "zadd_wrongtype_error",
    "zadd_exclusive_setup",
    "zadd_nx_xx_conflict",
    "zadd_incr_multiple_members_error",
];

struct VendoredRedisOracle {
    child: Child,
    port: u16,
}

impl VendoredRedisOracle {
    fn start(cfg: &HarnessConfig) -> Self {
        let server_path = cfg.oracle_root.join("src/redis-server");
        assert!(
            server_path.exists(),
            "vendored redis-server missing at {}",
            server_path.display()
        );

        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port for vendored redis");
        let port = listener
            .local_addr()
            .expect("ephemeral port address")
            .port();
        drop(listener);

        let child = Command::new(&server_path)
            .args([
                "--save",
                "",
                "--appendonly",
                "no",
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vendored redis-server");

        assert!(
            wait_for_redis_ready(port),
            "vendored redis-server did not become ready on 127.0.0.1:{port}"
        );

        Self { child, port }
    }

    fn start_with_config_file(cfg: &HarnessConfig) -> Self {
        let server_path = cfg.oracle_root.join("src/redis-server");
        assert!(
            server_path.exists(),
            "vendored redis-server missing at {}",
            server_path.display()
        );

        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port for vendored redis");
        let port = listener
            .local_addr()
            .expect("ephemeral port address")
            .port();
        drop(listener);

        let config_path = write_vendored_redis_config(port);
        let child = Command::new(&server_path)
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vendored redis-server from config file");

        assert!(
            wait_for_redis_ready(port),
            "vendored redis-server did not become ready on 127.0.0.1:{port}"
        );

        Self { child, port }
    }
}

impl Drop for VendoredRedisOracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_redis_ready(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            if stream.write_all(b"*1\r\n$4\r\nPING\r\n").is_ok() {
                let mut response = [0_u8; 16];
                if let Ok(bytes_read) = stream.read(&mut response)
                    && &response[..bytes_read] == b"+PONG\r\n"
                {
                    let _ = stream.shutdown(Shutdown::Both);
                    return true;
                }
            }
            let _ = stream.shutdown(Shutdown::Both);
        }
        sleep(Duration::from_millis(25));
    }
    false
}

fn write_vendored_redis_config(port: u16) -> PathBuf {
    let timestamp_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let temp_root = std::env::temp_dir();
    let config_path = temp_root.join(format!(
        "fr_conformance_vendored_redis_{}_{}_{}.conf",
        std::process::id(),
        port,
        timestamp_nanos
    ));
    let config = format!(
        "bind 127.0.0.1\nport {port}\nsave \"\"\nappendonly no\ndir {}\n",
        temp_root.display()
    );
    fs::write(&config_path, config).expect("write vendored redis config");
    config_path
}

fn command_frame(argv: &[&str]) -> RespFrame {
    RespFrame::Array(Some(
        argv.iter()
            .map(|arg| RespFrame::BulkString(Some(arg.as_bytes().to_vec())))
            .collect(),
    ))
}

fn send_frame_and_read(stream: &mut TcpStream, frame: &RespFrame) -> RespFrame {
    stream
        .write_all(&frame.to_bytes())
        .expect("write RESP command to vendored redis");
    stream
        .flush()
        .expect("flush RESP command to vendored redis");
    read_frame_from_stream(stream)
}

fn read_frame_from_stream(stream: &mut TcpStream) -> RespFrame {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        let n = stream.read(&mut chunk).expect("read RESP reply");
        assert!(n > 0, "vendored redis closed connection before replying");
        buf.extend_from_slice(&chunk[..n]);
        match parse_frame(&buf) {
            Ok(parsed) => return parsed.frame,
            Err(fr_protocol::RespParseError::Incomplete) => {}
            Err(err) => panic!("vendored redis emitted invalid RESP: {err}"),
        }
    }
}

fn acl_log_field<'a>(entry: &'a [RespFrame], key: &str) -> &'a RespFrame {
    entry
        .chunks_exact(2)
        .find_map(|pair| match pair {
            [RespFrame::BulkString(Some(field_name)), value]
                if field_name.eq_ignore_ascii_case(key.as_bytes()) =>
            {
                Some(value)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("ACL LOG entry missing field '{key}'"))
}

fn bulk_text(frame: &RespFrame) -> String {
    match frame {
        RespFrame::BulkString(Some(bytes)) => String::from_utf8_lossy(bytes).to_string(),
        other => panic!("expected bulk string, got {other:?}"),
    }
}

fn int_value(frame: &RespFrame) -> i64 {
    match frame {
        RespFrame::Integer(value) => *value,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn assert_acl_log_failed_auth_shape(reply: &RespFrame) {
    let entries = match reply {
        RespFrame::Array(Some(entries)) => entries,
        other => panic!("expected ACL LOG array reply, got {other:?}"),
    };
    let expected_usernames = ["disabled_user", "nobody", "testuser"];
    assert_eq!(entries.len(), expected_usernames.len());
    let mut expected_entry_id = expected_usernames.len() as i64 - 1;
    for (entry, username) in entries.iter().zip(expected_usernames) {
        let fields = match entry {
            RespFrame::Array(Some(fields)) => fields,
            other => panic!("expected ACL LOG entry array, got {other:?}"),
        };
        assert_eq!(int_value(acl_log_field(fields, "count")), 1);
        assert_eq!(bulk_text(acl_log_field(fields, "reason")), "auth");
        assert_eq!(bulk_text(acl_log_field(fields, "context")), "toplevel");
        assert_eq!(bulk_text(acl_log_field(fields, "object")), "AUTH");
        assert_eq!(bulk_text(acl_log_field(fields, "username")), username);
        let age_seconds = bulk_text(acl_log_field(fields, "age-seconds"));
        assert!(
            age_seconds.parse::<f64>().is_ok(),
            "expected numeric age-seconds, got {age_seconds}"
        );
        let client_info = bulk_text(acl_log_field(fields, "client-info"));
        assert!(client_info.contains("cmd=auth"));
        assert!(client_info.contains("user=default"));
        assert!(client_info.contains("resp=2"));
        assert_eq!(
            int_value(acl_log_field(fields, "entry-id")),
            expected_entry_id
        );
        expected_entry_id -= 1;
        let created = int_value(acl_log_field(fields, "timestamp-created"));
        let updated = int_value(acl_log_field(fields, "timestamp-last-updated"));
        assert!(created >= 0);
        assert!(updated >= created);
    }
}

fn dynamic_replication_metadata_case(name: &str) -> bool {
    matches!(
        name,
        "psync_returns_fullresync"
            | "psync_with_replid_returns_fullresync"
            | "psync_with_full_replid"
            | "psync_with_zero_offset"
            | "psync_case_insensitive"
            | "psync_wrong_arity_too_many"
            | "role_reports_master_after_promotion"
            | "role_returns_master"
            | "role_case_insensitive"
            | "role_still_master_after_operations"
    )
}

fn parse_fullresync_reply(frame: &RespFrame) -> (String, i64) {
    let reply = match frame {
        RespFrame::SimpleString(reply) => reply,
        other => panic!("expected FULLRESYNC simple string, got {other:?}"),
    };
    let parts = reply.split_whitespace().collect::<Vec<_>>();
    assert_eq!(
        parts.len(),
        3,
        "expected FULLRESYNC <replid> <offset>, got {reply}"
    );
    assert_eq!(
        parts[0], "FULLRESYNC",
        "expected FULLRESYNC reply, got {reply}"
    );
    assert_eq!(parts[1].len(), 40, "expected 40-char replid, got {reply}");
    assert!(
        parts[1].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "expected hex replid, got {reply}"
    );
    let offset = parts[2]
        .parse::<i64>()
        .unwrap_or_else(|err| panic!("expected integer FULLRESYNC offset in {reply}: {err}"));
    assert!(
        offset >= 0,
        "expected nonnegative FULLRESYNC offset, got {reply}"
    );
    (parts[1].to_string(), offset)
}

fn parse_master_role_reply(frame: &RespFrame) -> i64 {
    let items = match frame {
        RespFrame::Array(Some(items)) => items,
        other => panic!("expected ROLE array reply, got {other:?}"),
    };
    assert_eq!(items.len(), 3, "expected ROLE master triple, got {items:?}");
    assert_eq!(bulk_text(&items[0]), "master");
    let offset = int_value(&items[1]);
    assert!(
        offset >= 0,
        "expected nonnegative ROLE offset, got {items:?}"
    );
    match &items[2] {
        RespFrame::Array(Some(replicas)) => assert!(replicas.is_empty(), "expected no replicas"),
        other => panic!("expected empty replica array in ROLE reply, got {other:?}"),
    }
    offset
}

fn assert_nondecreasing(label: &str, offsets: &[i64]) {
    for pair in offsets.windows(2) {
        assert!(
            pair[0] <= pair[1],
            "{label} offsets must be nondecreasing, got {offsets:?}"
        );
    }
}

fn assert_replication_dynamic_metadata_matches_contract(failures: &[&CaseOutcome]) {
    let mut live_replids = Vec::new();
    let mut runtime_replids = Vec::new();
    let mut live_offsets = Vec::new();
    let mut runtime_offsets = Vec::new();

    for failure in failures {
        match failure.name.as_str() {
            "psync_returns_fullresync"
            | "psync_with_replid_returns_fullresync"
            | "psync_with_full_replid"
            | "psync_with_zero_offset"
            | "psync_case_insensitive"
            | "psync_wrong_arity_too_many" => {
                let (live_replid, live_offset) = parse_fullresync_reply(&failure.expected);
                let (runtime_replid, runtime_offset) = parse_fullresync_reply(&failure.actual);
                live_replids.push(live_replid);
                runtime_replids.push(runtime_replid);
                live_offsets.push(live_offset);
                runtime_offsets.push(runtime_offset);
            }
            "role_reports_master_after_promotion"
            | "role_returns_master"
            | "role_case_insensitive"
            | "role_still_master_after_operations" => {
                live_offsets.push(parse_master_role_reply(&failure.expected));
                runtime_offsets.push(parse_master_role_reply(&failure.actual));
            }
            other => panic!("unexpected dynamic replication case {other}"),
        }
    }

    if let Some(first) = live_replids.first() {
        assert!(
            live_replids.iter().all(|replid| replid == first),
            "live Redis replids changed within one smoke run: {live_replids:?}"
        );
    }
    if let Some(first) = runtime_replids.first() {
        assert!(
            runtime_replids.iter().all(|replid| replid == first),
            "runtime replids changed within one smoke run: {runtime_replids:?}"
        );
    }

    assert_nondecreasing("live Redis replication", &live_offsets);
    assert_nondecreasing("runtime replication", &runtime_offsets);
}

#[test]
fn smoke_report_is_stable() {
    let cfg = HarnessConfig::default_paths();
    let report = run_smoke(&cfg);
    assert_eq!(report.suite, "smoke");
    assert!(report.fixture_count >= 1);
    assert!(report.oracle_present);

    let fixture_path = cfg.fixture_root.join("core_strings.json");
    assert!(Path::new(&fixture_path).exists());

    let diff = run_fixture(&cfg, "core_strings.json").expect("fixture runs");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());

    let errors = run_fixture(&cfg, "core_errors.json").expect("error fixture");
    assert_eq!(errors.total, errors.passed);
    assert!(errors.failed.is_empty());

    let dispatch =
        run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("packet-003 dispatch fixture");
    assert_eq!(dispatch.total, dispatch.passed);
    assert!(dispatch.failed.is_empty());

    let auth_acl =
        run_fixture(&cfg, "fr_p2c_004_acl_journey.json").expect("packet-004 auth/acl fixture");
    assert_eq!(auth_acl.total, auth_acl.passed);
    assert!(auth_acl.failed.is_empty());

    let repl_handshake =
        run_replication_handshake_fixture(&cfg, "fr_p2c_006_replication_handshake.json")
            .expect("replication handshake fixture");
    assert_eq!(repl_handshake.total, repl_handshake.passed);
    assert!(repl_handshake.failed.is_empty());

    let protocol = run_protocol_fixture(&cfg, "protocol_negative.json").expect("protocol fixture");
    assert_eq!(protocol.total, protocol.passed);
    assert!(protocol.failed.is_empty());

    let replay = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture");
    assert_eq!(replay.total, replay.passed);
    assert!(replay.failed.is_empty());
}

#[test]
fn fr_p2c_001_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_001_eventloop_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_002_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_protocol_fixture(&cfg, "protocol_negative.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_003_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_004_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_004_acl_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_005_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_replay_fixture(&cfg, "persist_replay.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_006_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_006_replication_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_006_replication_handshake_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_replication_handshake_fixture(&cfg, "fr_p2c_006_replication_handshake.json")
        .expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_007_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_007_cluster_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_008_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_008_expire_evict_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_009_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_009_tls_config_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_hash_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_hash.json").expect("hash fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_list_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_list.json").expect("list fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_set_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_set.json").expect("set fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_zset_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_zset.json").expect("zset fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_geo_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_geo.json").expect("geo fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_stream_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_stream.json").expect("stream fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_generic_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_generic.json").expect("generic fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_acl_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_acl.json").expect("acl fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_acl_log_failed_auth_surface_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let mut runtime = Runtime::default_strict();
    let mut live = TcpStream::connect(("127.0.0.1", oracle_server.port))
        .expect("connect to vendored redis for ACL LOG smoke");
    live.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis read timeout");
    live.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis write timeout");

    let sequence = [
        (40_u64, ["ACL", "SETUSER", "testuser", "on", ">secret123"]),
        (43_u64, ["AUTH", "testuser", "wrongpass", "", ""]),
        (44_u64, ["AUTH", "nobody", "pass", "", ""]),
        (
            50_u64,
            ["ACL", "SETUSER", "disabled_user", "off", ">pass456"],
        ),
        (51_u64, ["AUTH", "disabled_user", "pass456", "", ""]),
    ];

    for (now_ms, argv) in sequence {
        let argv = argv
            .into_iter()
            .filter(|arg| !arg.is_empty())
            .collect::<Vec<_>>();
        let frame = command_frame(&argv);
        let _runtime_reply = runtime.execute_frame(frame.clone(), now_ms);
        let _live_reply = send_frame_and_read(&mut live, &frame);
    }

    let runtime_acl_log = runtime.execute_frame(command_frame(&["ACL", "LOG"]), 100);
    let live_acl_log = send_frame_and_read(&mut live, &command_frame(&["ACL", "LOG"]));

    assert_acl_log_failed_auth_shape(&runtime_acl_log);
    assert_acl_log_failed_auth_shape(&live_acl_log);
}

#[test]
fn core_hyperloglog_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_hyperloglog.json").expect("hyperloglog fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_bitmap_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_bitmap.json").expect("bitmap fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_transaction_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_transaction.json").expect("transaction fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_connection_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_connection.json").expect("connection fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_expiry_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_expiry.json").expect("expiry fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_client_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_client.json").expect("client fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_client_reply_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let cases = vec![
        LiveOptionalReplyCase {
            name: "client_reply_off_suppresses_own_ok".to_string(),
            now_ms: 10,
            argv: ["CLIENT", "REPLY", "OFF"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        LiveOptionalReplyCase {
            name: "client_reply_off_suppresses_error_reply".to_string(),
            now_ms: 11,
            argv: ["NOPE"].into_iter().map(str::to_string).collect(),
        },
        LiveOptionalReplyCase {
            name: "client_reply_on_restores_replies".to_string(),
            now_ms: 12,
            argv: ["CLIENT", "REPLY", "ON"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        LiveOptionalReplyCase {
            name: "client_reply_on_allows_following_reply".to_string(),
            now_ms: 13,
            argv: ["PING"].into_iter().map(str::to_string).collect(),
        },
        LiveOptionalReplyCase {
            name: "client_reply_skip_suppresses_own_ok".to_string(),
            now_ms: 14,
            argv: ["CLIENT", "REPLY", "SKIP"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        LiveOptionalReplyCase {
            name: "client_reply_skip_suppresses_next_error".to_string(),
            now_ms: 15,
            argv: ["NOPE"].into_iter().map(str::to_string).collect(),
        },
        LiveOptionalReplyCase {
            name: "client_reply_skip_window_expires_after_one_command".to_string(),
            now_ms: 16,
            argv: ["PING"].into_iter().map(str::to_string).collect(),
        },
    ];
    let report =
        run_live_redis_optional_reply_sequence_diff(&cfg, "core_client_reply", &cases, &oracle)
            .expect("client reply live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_server_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_server.json").expect("server fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_scripting_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_scripting.json").expect("scripting fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_pubsub_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_pubsub.json").expect("pubsub fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_replication_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_replication.json").expect("replication fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_sort_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_sort.json").expect("sort fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_scan_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_scan.json").expect("scan fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_config_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_config.json").expect("config fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_cluster_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_cluster.json").expect("cluster fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_copy_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_copy.json").expect("copy fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_function_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_function.json").expect("function fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_wait_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_wait.json").expect("wait fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_blocking_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_blocking.json").expect("blocking fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_strings_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_strings.json").expect("strings fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_errors_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_errors.json").expect("errors fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_object_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_object.json").expect("object fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_pfdebug_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_pfdebug.json").expect("pfdebug fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_migrate_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_migrate.json").expect("migrate fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_module_sentinel_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_module_sentinel.json").expect("module/sentinel fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_module_sentinel_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff(&cfg, "core_module_sentinel.json", &oracle)
        .expect("module/sentinel live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_pubsub_multi_client_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_multi_client_diff(&cfg, "core_pubsub_multi_client.json", &oracle)
        .expect("pubsub multi-client live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_blocking_multi_client_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_multi_client_diff(&cfg, "core_blocking_multi_client.json", &oracle)
        .expect("blocking multi-client live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_replication_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff(&cfg, "core_replication.json", &oracle).expect("replication live diff");
    let dynamic_failures = report
        .failed
        .iter()
        .filter(|failure| dynamic_replication_metadata_case(&failure.name))
        .collect::<Vec<_>>();
    let unexpected_failures = report
        .failed
        .iter()
        .filter(|failure| !dynamic_replication_metadata_case(&failure.name))
        .collect::<Vec<_>>();

    assert!(
        unexpected_failures.is_empty(),
        "unexpected replication mismatches: {:?}",
        unexpected_failures
    );
    assert_replication_dynamic_metadata_matches_contract(&dynamic_failures);
}

#[test]
fn core_connection_cluster_mode_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_connection.json",
        &[
            "readonly_reports_cluster_disabled",
            "readwrite_reports_cluster_disabled",
            "readonly_wrong_arity",
            "readwrite_wrong_arity",
        ],
        &oracle,
    )
    .expect("connection cluster-mode live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_cluster_disabled_surface_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_cluster.json",
        &[
            "cluster_info",
            "cluster_myid",
            "cluster_getkeysinslot_with_key",
            "cluster_countkeysinslot_with_key",
            "cluster_help",
            "cluster_unknown_subcommand",
            "cluster_keyslot_wrong_arity",
            "cluster_reset_hard",
        ],
        &oracle,
    )
    .expect("cluster disabled-surface live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_config_rewrite_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start_with_config_file(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_config.json",
        &["config_rewrite", "config_rewrite_ok"],
        &oracle,
    )
    .expect("config rewrite live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_server_config_rewrite_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start_with_config_file(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_server.json",
        &["config_rewrite_returns_ok"],
        &oracle,
    )
    .expect("server config rewrite live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_server_info_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_info_contract_diff(
        &cfg,
        "core_server.json",
        &[
            live_info_setup_case("info_keyspace_flush_for_test"),
            live_info_case("info_keyspace_section_empty_db", &["Keyspace"], &[]),
            live_info_setup_case("info_keyspace_setup"),
            live_info_case(
                "info_keyspace_section_with_key",
                &["Keyspace"],
                &[live_info_field(
                    "Keyspace",
                    "db0",
                    LiveInfoFieldComparison::Exact,
                )],
            ),
            live_info_case("info_unknown_section_empty", &[], &[]),
            live_info_case(
                "info_server_section",
                &["Server"],
                &[
                    live_info_field("Server", "redis_mode", LiveInfoFieldComparison::Exact),
                    live_info_field("Server", "process_id", LiveInfoFieldComparison::Shape),
                    live_info_field("Server", "run_id", LiveInfoFieldComparison::Shape),
                    live_info_field("Server", "tcp_port", LiveInfoFieldComparison::Shape),
                    live_info_field(
                        "Server",
                        "uptime_in_seconds",
                        LiveInfoFieldComparison::Shape,
                    ),
                ],
            ),
            live_info_case(
                "info_case_insensitive",
                &["Server"],
                &[
                    live_info_field("Server", "redis_mode", LiveInfoFieldComparison::Exact),
                    live_info_field("Server", "process_id", LiveInfoFieldComparison::Shape),
                    live_info_field("Server", "run_id", LiveInfoFieldComparison::Shape),
                ],
            ),
            live_info_case(
                "info_clients_section",
                &["Clients"],
                &[
                    live_info_field(
                        "Clients",
                        "connected_clients",
                        LiveInfoFieldComparison::Shape,
                    ),
                    live_info_field("Clients", "maxclients", LiveInfoFieldComparison::Shape),
                    live_info_field("Clients", "blocked_clients", LiveInfoFieldComparison::Shape),
                    live_info_field(
                        "Clients",
                        "tracking_clients",
                        LiveInfoFieldComparison::Shape,
                    ),
                ],
            ),
            live_info_case(
                "info_memory_section",
                &["Memory"],
                &[
                    live_info_field("Memory", "used_memory", LiveInfoFieldComparison::Shape),
                    live_info_field(
                        "Memory",
                        "used_memory_human",
                        LiveInfoFieldComparison::Shape,
                    ),
                    live_info_field("Memory", "used_memory_rss", LiveInfoFieldComparison::Shape),
                    live_info_field(
                        "Memory",
                        "mem_fragmentation_ratio",
                        LiveInfoFieldComparison::Shape,
                    ),
                    live_info_field("Memory", "maxmemory_policy", LiveInfoFieldComparison::Exact),
                ],
            ),
            live_info_case(
                "info_persistence_section",
                &["Persistence"],
                &[
                    live_info_field("Persistence", "loading", LiveInfoFieldComparison::Exact),
                    live_info_field(
                        "Persistence",
                        "rdb_changes_since_last_save",
                        LiveInfoFieldComparison::Shape,
                    ),
                    live_info_field(
                        "Persistence",
                        "rdb_last_save_time",
                        LiveInfoFieldComparison::Shape,
                    ),
                    live_info_field("Persistence", "aof_enabled", LiveInfoFieldComparison::Exact),
                ],
            ),
            live_info_case(
                "info_replication_section",
                &["Replication"],
                &[
                    live_info_field("Replication", "role", LiveInfoFieldComparison::Exact),
                    live_info_field(
                        "Replication",
                        "connected_slaves",
                        LiveInfoFieldComparison::Exact,
                    ),
                    live_info_field(
                        "Replication",
                        "master_failover_state",
                        LiveInfoFieldComparison::Exact,
                    ),
                    live_info_field(
                        "Replication",
                        "master_replid",
                        LiveInfoFieldComparison::Shape,
                    ),
                    live_info_field(
                        "Replication",
                        "master_repl_offset",
                        LiveInfoFieldComparison::Shape,
                    ),
                    live_info_field(
                        "Replication",
                        "repl_backlog_size",
                        LiveInfoFieldComparison::Shape,
                    ),
                ],
            ),
        ],
        &oracle,
    )
    .expect("server info live contract diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn fr_p2c_007_cluster_disabled_surface_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "fr_p2c_007_cluster_journey.json",
        &[
            "cluster_wrong_arity_is_rejected",
            "cluster_unknown_subcommand_is_rejected",
            "cluster_info_is_reachable_from_runtime",
            "cluster_keyslot_is_reachable_from_runtime",
            "asking_after_readonly_is_ok",
            "readonly_wrong_arity_is_rejected",
        ],
        &oracle,
    )
    .expect("packet-007 cluster disabled-surface live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_debug_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_debug.json").expect("debug fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_transaction_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff(&cfg, "core_transaction.json", &oracle).expect("transaction live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_scripting_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_scripting.json",
        CORE_SCRIPTING_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("scripting live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_stream_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_stream.json",
        CORE_STREAM_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("stream live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_scan_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_scan.json", CORE_SCAN_LIVE_STABLE_CASES, &oracle)
            .expect("scan live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_object_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_object.json",
        CORE_OBJECT_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("object live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_sort_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff(&cfg, "core_sort.json", &oracle).expect("sort live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_strings_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_strings.json",
        CORE_STRINGS_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("strings live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_list_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_list.json", CORE_LIST_LIVE_STABLE_CASES, &oracle)
            .expect("list live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_hash_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_hash.json", CORE_HASH_LIVE_STABLE_CASES, &oracle)
            .expect("hash live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_set_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_set.json", CORE_SET_LIVE_STABLE_CASES, &oracle)
            .expect("set live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_zset_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_zset.json", CORE_ZSET_LIVE_STABLE_CASES, &oracle)
            .expect("zset live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}
