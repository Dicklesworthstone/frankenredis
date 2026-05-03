use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fr_config::{
    ConfigFileParseErrorReason, parse_redis_config, parse_redis_config_bytes,
    split_config_line_args, split_config_line_args_bytes,
};
use fr_conformance::{
    CaseOutcome, ConformanceCase, ConformanceFixture, HarnessConfig, LiveInfoContractCase,
    LiveInfoFieldComparison, LiveInfoFieldContract, LiveOptionalReplyCase, LiveOracleConfig,
    run_fixture, run_live_redis_diff, run_live_redis_diff_for_cases,
    run_live_redis_info_contract_diff, run_live_redis_multi_client_diff,
    run_live_redis_optional_reply_sequence_diff, run_protocol_fixture, run_replay_fixture,
    run_replication_handshake_fixture, run_smoke,
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
    "object_idletime_missing_key",
    "object_refcount_string",
    "object_freq_non_lfu_policy_errors",
    "object_freq_missing_key",
    "object_freq_enable_allkeys_lfu",
    "object_freq_existing_key_starts_at_zero_after_lfu_switch",
    "object_freq_get_increments_counter",
    "object_freq_reports_incremented_counter",
    "object_freq_overwrite_increments_counter",
    "object_freq_reports_counter_after_overwrite",
    "object_freq_disable_lfu_again",
    "object_freq_lru_policy_errors",
    "object_idletime_enable_allkeys_lfu",
    "object_idletime_lfu_policy_errors",
    "object_idletime_disable_lfu_again",
    "object_no_subcommand",
    "object_encoding_extra_arg",
    "object_refcount_extra_arg",
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
    "xadd_nomkstream_flush",
    "xadd_nomkstream_nonexistent_returns_nil",
    "xadd_nomkstream_verify_not_created",
    "xadd_nomkstream_existing_stream_works",
    "xadd_nomkstream_on_existing_succeeds",
    "xadd_nomkstream_verify_length",
    "xadd_maxlen_seed_1",
    "xadd_maxlen_seed_2",
    "xadd_maxlen_seed_3",
    "xadd_with_maxlen_trims_to_2",
    "xadd_maxlen_verify_length",
    "xadd_maxlen_approx_trims",
    "xadd_maxlen_approx_verify_length",
    "xgroup_createconsumer_read1_c1_created",
    "xgroup_createconsumer_read1_c1_duplicate",
    "xgroup_createconsumer_read1_c2_created",
    "xgroup_createconsumer_missing_group_error",
    "xgroup_createconsumer_missing_key_error",
    "xrange_wrongtype_seed",
    "xgroup_createconsumer_wrongtype_error",
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
    "eval_ro_may_replicate_command_rejected",
    "evalsha_ro_setup_write_reject_load",
    "evalsha_ro_write_command_rejected",
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
    "script_load_and_exists",
    "script_load_repeat_same_hash",
    "script_load_newline_variant_hash",
    "script_load_crlf_variant_hash",
    "script_load_leading_space_variant_hash",
    "script_load_trailing_space_variant_hash",
    "script_exists_loaded_variants",
    "eval_while_loop",
    "eval_repeat_until",
    "eval_table_insert_and_return",
    "eval_status_reply",
    "eval_error_reply",
    "eval_error_reply_default_err_code",
    "eval_error_reply_no_dash_uses_first_word_as_code",
    "eval_error_reply_dash_uses_first_word_as_code",
    "eval_error_reply_dash_without_message_defaults_err_code",
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

const CORE_EXPIRY_LIVE_STABLE_CASES: &[&str] = &[
    // Missing/no-expiry observability
    "pttl_missing_key_returns_minus2",
    "ttl_missing_key_returns_minus2",
    "set_key_for_expiry_tests",
    "pttl_no_expiry_returns_minus1",
    "ttl_no_expiry_returns_minus1",
    "expire_sets_ttl_returns_1",
    "persist_removes_expiry_returns_1",
    "pttl_after_persist_returns_minus1",
    "persist_no_expiry_returns_0",
    "persist_missing_key_returns_0",
    "expire_missing_key_returns_0",
    "set_key_no_expiry_for_expiretime",
    "expiretime_no_expiry_returns_minus1",
    "pexpiretime_no_expiry_returns_minus1",
    "expiretime_missing_key_returns_minus2",
    "pexpiretime_missing_key_returns_minus2",
    // Relative expiry option matrices
    "pexpire_opts_setup",
    "pexpire_nx_no_ttl_succeeds",
    "pexpire_nx_has_ttl_fails",
    "pexpire_xx_has_ttl_succeeds",
    "pexpire_gt_larger_succeeds",
    "pexpire_gt_smaller_fails",
    "pexpire_lt_smaller_succeeds",
    "pexpire_lt_larger_fails",
    "expire_nx_option_only_set_when_no_expiry",
    "expire_nx_on_no_expiry_succeeds",
    "expire_nx_on_existing_expiry_fails",
    "expire_xx_on_existing_expiry_succeeds",
    "expire_gt_sets_longer_ttl",
    "expire_gt_rejects_shorter_ttl",
    "expire_lt_sets_shorter_ttl",
    "expire_lt_rejects_longer_ttl",
    "pexpire_nx_setup",
    "pexpire_nx_no_existing_expiry",
    "pexpire_nx_already_has_expiry",
    "pexpire_xx_has_expiry",
    "pexpire_gt_longer",
    "pexpire_gt_shorter_rejected",
    "pexpire_lt_shorter",
    "pexpire_lt_longer_rejected",
    "pexpire_missing_key",
    "expire_xx_no_expiry_setup",
    "expire_xx_no_existing_expiry_fails",
    "expire_nx_missing_key",
    "expire_xx_missing_key",
    // GETEX / KEEPTTL paths that do not depend on wall-clock alignment
    "getex_no_option_setup",
    "getex_no_option",
    "getex_no_option_verify_no_expiry",
    "getex_missing_key",
    "getex_wrong_arity",
    "getex_ex_setup",
    "getex_persist",
    "getex_persist_verify_no_ttl",
    "set_keepttl_no_existing_ttl_setup",
    "set_keepttl_no_existing_ttl",
    "set_keepttl_no_existing_ttl_verify",
    // Validation and parse errors
    "expire_wrong_arity",
    "pttl_wrong_arity",
    "ttl_wrong_arity",
    "persist_wrong_arity",
    "pexpire_wrong_arity",
    "expire_invalid_combo_setup",
    "expire_nx_gt_error",
    "expire_non_integer_error",
    "pexpire_non_integer_error",
    "expiretime_wrong_arity",
    "pexpiretime_wrong_arity",
    "expire_gt_lt_error",
    "pexpire_nx_xx_error",
    "getex_wrongtype_setup",
    "getex_wrongtype",
    "getex_ex_non_integer",
    "persist_missing_key_returns_zero",
    "persist_no_ttl_setup",
    "persist_no_ttl_returns_zero",
    "persist_with_ttl_setup",
    "persist_with_ttl_returns_one",
    "persist_ttl_removed",
    "persist_nonexistent_returns_zero",
    "ttl_nonexistent_returns_minus2",
    "pttl_nonexistent_returns_minus2",
    "expire_nonexistent_returns_zero",
    "expire_rejects_leading_plus",
    "expire_rejects_leading_zero",
    "pexpire_rejects_leading_plus",
    "pexpire_rejects_leading_zero",
    // Immediate deletion semantics that remain stable against wall clock
    "expire_zero_setup",
    "expire_zero_set_key",
    "expire_zero_deletes_key",
    "expire_zero_key_gone",
    "expire_negative_setup",
    "expire_negative_deletes",
    "expire_negative_get_missing",
    "expire_negative_pttl_missing",
    "pexpire_zero_setup",
    "pexpire_zero_deletes",
    "pexpire_zero_get_missing",
    "expireat_zero_set_key",
    "expireat_zero_deletes_key",
    "expireat_zero_key_gone",
    "expireat_past_setup",
    "expireat_past_deletes",
    "expireat_past_get_missing",
    "expireat_negative_setup",
    "expireat_negative_deletes",
    "expireat_negative_get_missing",
    "pexpireat_past_setup",
    "pexpireat_past_deletes",
    "pexpireat_past_pttl_missing",
    "pexpireat_negative_setup",
    "pexpireat_negative_deletes",
    "pexpireat_negative_pttl_missing",
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
    "smove_same_key_setup",
    "smove_same_key_existing_member",
    "smove_same_key_member_still_present",
    "smove_same_key_cardinality_unchanged",
    "smove_same_key_missing_member",
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
    "sintercard_limit_negative",
    "sintercard_duplicate_limit_uses_last_value",
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
    "zpopmax_count_0_returns_empty",
    "zpopmin_negative_count_errors",
    "zpopmax_negative_count_errors",
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

const CORE_GENERIC_LIVE_STABLE_CASES: &[&str] = &[
    // TYPE command
    "set_key_for_type",
    "type_string",
    "type_missing_key",
    "setup_list_for_type",
    "type_list",
    "setup_set_for_type",
    "type_set",
    "setup_hash_for_type",
    "type_hash",
    "setup_zset_for_type",
    "type_zset",
    "type_stream_setup",
    "type_stream",
    "type_none",
    // EXISTS
    "exists_present",
    "exists_missing",
    "exists_multiple",
    "exists_duplicate_keys",
    "exists_multi_setup1",
    "exists_multi_setup2",
    "exists_multi",
    "exists_duplicate_key",
    // DEL/UNLINK
    "del_single",
    "del_single_result",
    "del_missing",
    "unlink_setup",
    "unlink_result",
    "unlink_missing",
    "del_multiple",
    "unlink_setup_multi",
    "unlink_multiple",
    "unlink_verify_deleted",
    "unlink_nonexistent",
    "del_multi_setup1",
    "del_multi_setup2",
    "del_multi",
    // RENAME/RENAMENX
    "rename_setup",
    "rename_ok",
    "rename_verify_new",
    "rename_verify_old_gone",
    "rename_missing_key",
    "renamenx_setup",
    "renamenx_no_conflict",
    "renamenx_with_conflict",
    "renamenx_conflict_result",
    "rename_missing_source",
    "rename_self_setup",
    "rename_self_same_key",
    "rename_self_verify",
    "renamenx_missing_source",
    "renamenx_dest_exists_setup",
    "renamenx_dest_exists",
    "rename_nonexistent",
    "rename_same_key_setup",
    "rename_same_key",
    "renamenx_nonexistent_src",
    // COPY
    "copy_setup",
    "copy_success",
    "copy_verify_src",
    "copy_verify_dst",
    "copy_no_replace",
    "copy_with_replace",
    "copy_replace_result",
    "copy_replace_verify",
    "copy_missing_src",
    // EXPIRE/TTL/PERSIST (non-timing-dependent)
    "expire_setup",
    "expire_set",
    "ttl_no_expiry",
    "ttl_missing_key",
    "pttl_missing_key",
    "persist_missing_key",
    "persist_no_expiry",
    "expire_missing_key",
    "expiretime_setup",
    "expiretime_no_expiry",
    "expiretime_missing",
    "pexpiretime_no_expiry",
    "pexpiretime_missing",
    "persist_nonexistent_key",
    "persist_no_ttl_key_setup",
    "persist_no_ttl_key",
    // EXPIRE options (NX/XX/GT/LT)
    "expire_nx_setup",
    "expire_nx_no_existing_ttl",
    "expire_nx_has_existing_ttl",
    "expire_xx_setup",
    "expire_xx_no_existing_ttl",
    // APPEND
    "append_new_key",
    "append_existing",
    "append_verify",
    // GETDEL
    "getdel_setup",
    "getdel_returns_value",
    "getdel_key_gone",
    "getdel_missing",
    // GETEX (non-timing aspects)
    "getex_setup",
    "getex_no_options",
    "getex_missing",
    // OBJECT ENCODING
    "object_encoding_string_embstr",
    "object_encoding_embstr",
    "object_encoding_int",
    "object_encoding_int_result",
    "object_encoding_hash",
    "object_encoding_list",
    "object_encoding_list_result",
    "object_encoding_set",
    "object_encoding_zset",
    "object_encoding_missing_key",
    "object_encoding_raw_setup",
    "object_encoding_raw",
    "object_encoding_lowercase",
    "object_encoding_stream_setup",
    "object_encoding_stream",
    "object_encoding_raw_string_setup",
    "object_encoding_raw_string",
    "object_encoding_hashtable_setup",
    "object_encoding_hashtable",
    "object_encoding_skiplist_setup",
    "object_encoding_skiplist",
    "object_encoding_intset_setup",
    "object_encoding_intset",
    "object_encoding_nonexistent",
    // OBJECT REFCOUNT
    "object_refcount",
    // TOUCH
    "touch_existing",
    "touch_setup_a",
    "touch_setup_b",
    "touch_multiple_existing",
    "touch_mix_existing_missing",
    "touch_all_missing",
    "touch_nonexistent",
    "touch_mixed_setup",
    "touch_mixed",
    // DBSIZE
    "dbsize_count",
    // KEYS (pattern matching)
    "keys_setup_a",
    "keys_setup_b",
    "keys_setup_c",
    "keys_no_match",
    "keys_single_match",
    "keys_setup_prefix_a",
    "keys_setup_prefix_b",
    "keys_setup_suffix",
    // keys_prefix_wildcard and keys_suffix_wildcard excluded: KEYS ordering is non-deterministic
    "keys_char_class_setup_a",
    "keys_char_class_setup_b",
    "keys_char_class_setup_x",
    "keys_negated_char_class",
    "keys_escaped_star_setup",
    "keys_escaped_star_match",
    "keys_escaped_question_setup",
    "keys_escaped_question_match",
    "keys_literal_brackets_setup",
    "keys_literal_brackets_match",
    // RENAME with TTL preservation
    "rename_ttl_setup",
    "rename_ttl_set_expire",
    "rename_ttl_do_rename",
    "rename_ttl_verify_value",
    // RENAME cross-type
    "rename_cross_type_list_setup",
    "rename_cross_type_string_setup",
    "rename_cross_type_do",
    "rename_cross_type_verify_type",
    "rename_cross_type_verify_contents",
    // RENAME hash
    "rename_hash_preserves_fields_setup",
    "rename_hash_do",
    "rename_hash_verify_type",
    "rename_hash_verify_field",
    "rename_hash_src_gone",
    // DUMP/RESTORE
    "dump_missing_key",
    // Wrong arity errors
    "touch_wrong_arity",
    "randomkey_wrong_arity",
    "rename_wrong_arity",
    "renamenx_wrong_arity",
    "type_wrong_arity",
    "persist_wrong_arity",
    "del_wrong_arity",
    "object_wrong_arity",
    "keys_wrong_arity_zero",
    "keys_wrong_arity_extra",
    "dump_wrong_arity_no_args",
    "dump_wrong_arity_extra",
    "restore_wrong_arity",
    "restore_invalid_payload",
    "sort_stream_wrongtype",
];

const CORE_BITMAP_LIVE_STABLE_CASES: &[&str] = &[
    // SETBIT/GETBIT basic
    "setbit_creates_key_returns_0",
    "getbit_returns_set_bit",
    "getbit_returns_0_for_unset_bit",
    "setbit_returns_old_value",
    "getbit_after_clear_returns_0",
    "getbit_missing_key_returns_0",
    "setbit_high_offset",
    "getbit_high_offset",
    "setbit_returns_previous_on_string",
    "getbit_on_string_key",
    // BITCOUNT
    "setup_bitcount_key",
    "bitcount_whole_string",
    "bitcount_byte_range_0_0",
    "bitcount_byte_range_1_1",
    "bitcount_missing_key",
    "bitcount_negative_range",
    "bitcount_bit_range_10_14",
    "bitcount_bit_range_out_of_bounds",
    "bitcount_bit_negative_reverse",
    "bitcount_syntax_error_missing_end",
    "bitcount_on_string_key",
    "bitcount_range_entire",
    "bitcount_range_out_of_bounds",
    "bitcount_nonexistent_with_range",
    // BITPOS
    "bitpos_find_first_set_bit",
    "bitpos_find_first_clear_bit",
    "bitpos_missing_key_bit_0",
    "bitpos_missing_key_bit_1",
    "bitpos_with_range",
    "bitpos_with_range_start_end",
    "bitpos_setup_allzeros",
    "bitpos_allzeros_find_one",
    "bitpos_nonexistent_find_zero",
    "bitpos_nonexistent_find_one",
    // BITOP
    "bitop_and_setup_key1",
    "bitop_and_setup_key2",
    "bitop_and_same_keys",
    "bitop_and_result_check",
    "bitop_not",
    "bitop_or",
    "bitop_xor",
    "bitop_xor_same_key_zeroes",
    "bitop_and_missing_key",
    "bitop_and_missing_key_result",
    "bitop_or_missing_key",
    "bitop_or_missing_key_result",
    "bitop_and_self",
    "bitop_or_self",
    "bitop_and_missing_sources",
    "bitop_or_missing_sources",
    // BITFIELD
    "bitfield_set_u8",
    "bitfield_get_u8",
    "bitfield_set_i8",
    "bitfield_get_i8",
    "bitfield_incrby",
    "bitfield_multiple_ops",
    "bitfield_get_missing_key",
    "bitfield_u16_set_and_get",
    "bitfield_u16_get",
    "bitfield_at_offset",
    "bitfield_get_at_offset",
    "bitfield_incrby_u8",
    "bitfield_overflow_sat",
    "bitfield_overflow_fail",
    "bitfield_overflow_wrap_u8_setup",
    "bitfield_overflow_wrap_u8_incrby",
    "bitfield_overflow_wrap_i8_setup",
    "bitfield_overflow_wrap_i8_positive_to_negative",
    "bitfield_overflow_wrap_i8_negative_setup",
    "bitfield_overflow_wrap_i8_negative_to_positive",
    "bitfield_i16_signed",
    "bitfield_i16_get",
    "bitfield_triple_ops",
    "bitfield_incrby_wrap_underflow",
    "bitfield_incrby_sat_overflow",
    "bitfield_incrby_fail_overflow",
    // BITFIELD_RO
    "bitfield_ro_setup_field",
    "bitfield_ro_get_u8",
    "bitfield_ro_rejects_set",
    "bitfield_ro_rejects_incrby",
    "bitfield_ro_multiple_gets",
    "bitfield_ro_read_two_fields",
    "bitfield_ro_nonexistent_key",
    "bitfield_ro_signed_get",
    "bitfield_ro_read_signed",
    // Error cases
    "setbit_wrong_arity_no_args",
    "getbit_wrong_arity",
    "bitop_wrong_arity",
    "setbit_invalid_bit_value",
    "setbit_wrongtype_on_list",
    "setbit_on_list_wrongtype",
    "getbit_on_list_wrongtype",
    "bitcount_on_list_wrongtype",
    "bitfield_wrong_arity",
    "bitpos_wrong_arity",
    "bitpos_on_list_wrongtype",
    "bitpos_wrong_arity_no_key",
    "bitop_wrongtype_list",
    "bitop_not_multiple_sources_error",
    "setbit_negative_offset_error",
    "getbit_negative_offset_error",
    "setbit_non_integer_offset",
    "bitop_invalid_operation",
    "bitfield_wrongtype",
    "setbit_wrong_arity",
    "setbit_bad_offset",
    "setbit_bad_value",
];

const CORE_HYPERLOGLOG_LIVE_STABLE_CASES: &[&str] = &[
    // PFADD basic
    "pfadd_creates_key_returns_1",
    "pfadd_duplicate_elements_returns_0",
    "pfadd_new_element_returns_1",
    "pfadd_no_elements_creates_key",
    "pfadd_no_elements_existing_returns_0",
    "pfadd_single_element",
    "pfadd_empty_string_element",
    "pfadd_all_duplicates_returns_0",
    "pfadd_many_elements",
    "pfadd_repeat_same_elements",
    "pfadd_numeric_string_elements",
    // PFCOUNT basic
    "pfcount_missing_key_returns_0",
    "pfcount_empty_hll_returns_0",
    "pfcount_hll1_has_four",
    "pfcount_single_element",
    "pfcount_empty_string_element",
    "pfcount_many_elements",
    "pfcount_numeric_string_elements",
    "pfcount_all_nonexistent_returns_0",
    // PFCOUNT multi-key
    "pfadd_second_set",
    "pfcount_hll2_has_three",
    "pfcount_multiple_keys_union_count",
    "pfcount_multiple_keys_empty_union",
    "pfcount_mix_existing_nonexisting",
    "pfadd_hll3",
    "pfcount_three_key_union",
    "pfcount_union_without_merge",
    "hll_pfcount_multiple_nonexistent",
    // PFMERGE basic
    "pfmerge_basic",
    "pfmerge_dest_only",
    "pfcount_dest_only_returns_0",
    "pfcount_merged_has_seven",
    "pfmerge_nonexistent_sources",
    "pfcount_merged_none_returns_0",
    "pfmerge_into_existing",
    "pfmerge_overwrites_dest",
    "pfcount_merged_into_existing",
    "pfmerge_single_source",
    "pfcount_copied",
    "pfmerge_three_sources",
    "pfcount_merged_three",
    "pfmerge_overlapping_sets",
    "pfmerge_overlapping_sets_2",
    "pfmerge_overlapping",
    "pfcount_overlapping_merged",
    "pfmerge_dest_is_also_source",
    "pfmerge_self_merge",
    "pfcount_after_self_merge",
    "pfmerge_multiple_overlapping",
    "pfcount_multi_merge",
    // Delete and recreate
    "setup_hll_for_del",
    "del_hll_for_recreate",
    "pfadd_after_del_recreate",
    "pfcount_after_recreate",
    "del_hll_then_pfcount",
    "pfcount_after_del",
    // PFMERGE with empty
    "pfadd_nonempty_for_merge",
    "pfmerge_with_empty_source",
    "pfcount_merged_with_empty",
    "pfmerge_nonexistent_dest_from_nonexistent_sources",
    "pfcount_merged_nonexistent_returns_0",
    // PFADD/PFMERGE after operations
    "pfadd_after_pfmerge",
    "pfcount_after_merge_and_add",
    // Object/type introspection
    "object_encoding_hll_is_raw",
    "type_of_hll_is_string",
    // Large HLL batches
    "hll_large_flush",
    "hll_large_pfadd_batch_0",
    "hll_large_pfadd_batch_1",
    "hll_large_pfadd_batch_2",
    "hll_large_pfadd_batch_3",
    "hll_large_pfadd_batch_4",
    "hll_large_pfcount_50",
    "hll_large_pfadd_batch_5",
    "hll_large_pfadd_batch_6",
    "hll_large_pfadd_batch_7",
    "hll_large_pfadd_batch_8",
    "hll_large_pfadd_batch_9",
    "hll_large_pfcount_100",
    // Duplicates and empty strings
    "hll_dup_in_single_call",
    "hll_dup_count_is_3",
    "hll_empty_string_element",
    "hll_empty_string_count",
    // Error cases - wrong type
    "setup_list_key_for_wrongtype",
    "pfadd_on_list_key_wrongtype",
    "pfcount_on_list_key_wrongtype",
    "setup_zset_for_hll_wrongtype",
    "pfadd_on_zset_wrongtype",
    "pfcount_on_zset_wrongtype",
    "pfmerge_src_zset_wrongtype",
    "pfmerge_on_hash_key_wrongtype",
    "pfadd_on_hash_key_wrongtype",
    "pfcount_on_hash_key_wrongtype",
    "pfmerge_dest_hash_wrongtype",
    // Error cases - invalid HLL
    "pfadd_on_string_key_invalid_hll",
    "pfcount_on_string_key_invalid_hll",
    "pfmerge_src_invalid_hll",
    "hll_merge_non_hll_dest_setup",
    "hll_merge_non_hll_dest_error",
    "pfadd_dest_wrongtype_setup",
    "pfmerge_dest_is_non_hll_string",
    // Wrong arity
    "pfadd_wrong_arity_no_key",
    "pfcount_wrong_arity",
    "pfmerge_wrong_arity",
];

const CORE_GEO_LIVE_STABLE_CASES: &[&str] = &[
    // GEOADD basic
    "geoadd_seed",
    "geoadd_ch_same_coordinate_is_zero",
    "geoadd_ch_changed_coordinate_counts_update",
    "geoadd_nx_skips_existing",
    "geoadd_xx_skips_missing",
    "geoadd_negative_longitude",
    "geoadd_multiple_members_at_once",
    "geoadd_multiple_at_once",
    "geoadd_ch_flag_update",
    "geoadd_ch_flag_change",
    // GEOHASH (avoiding precision-dependent cases)
    "geohash_missing_member",
    "geohash_on_missing_key",
    // GEODIST
    "geodist_kilometers",
    "geodist_missing_member",
    "geodist_meters_default",
    "geodist_miles",
    "geodist_feet",
    "geodist_nonexistent_key",
    "geodist_same_member",
    "geodist_sf_la",
    "geodist_feet_setup_a",
    "geodist_feet_setup_b",
    // GEOPOS (avoiding coordinate precision cases)
    "geopos_missing_member",
    "geopos_missing_key",
    "geopos_on_empty_key",
    // GEORADIUS (deprecated but still supported)
    "geoadd_seed_geo2",
    "georadius_basic",
    "georadius_count_1",
    "georadius_desc",
    "georadius_no_match",
    "georadius_nonexistent_key",
    "georadius_withdist",
    "georadius_ro_basic",
    // GEORADIUSBYMEMBER
    "georadiusbymember_basic",
    "georadiusbymember_missing_member",
    "georadiusbymember_ro_basic",
    // GEOSEARCH
    "geosearch_fromlonlat_byradius",
    "geosearch_frommember_byradius",
    "geosearch_fromlonlat_byradius_count",
    "geosearch_fromlonlat_bybox",
    "geosearch_nonexistent_key",
    "geosearch_bybox_narrow",
    "geosearch_frommember_missing",
    "geosearch_desc_order",
    "geosearch_large_radius_all",
    "geosearch_small_radius_none",
    "geosearch_asc_setup",
    "geosearch_asc",
    "geosearch_desc",
    "geosearch_count_1_asc",
    "geosearch_bybox",
    // GEOSEARCHSTORE
    "geosearchstore_basic",
    "geosearchstore_verify_dest_type",
    "geosearchstore_verify_dest_card",
    "geosearchstore_storedist",
    // GEORADIUS STORE
    "georadius_store_setup",
    "georadius_store_basic",
    "georadius_store_verify_type",
    "georadius_store_verify_card",
    "georadius_storedist_basic",
    "georadius_storedist_verify_type",
    "georadius_storedist_verify_scores_are_distances",
    "georadius_store_no_match_deletes_dest",
    "georadius_store_nonexistent_key",
    "georadius_store_with_count",
    "georadius_store_count1_verify",
    "georadius_store_all_with_large_radius",
    "georadius_store_all_verify",
    // GEORADIUSBYMEMBER STORE
    "georadiusbymember_store_basic",
    "georadiusbymember_store_verify_card",
    "georadiusbymember_storedist_basic",
    "georadiusbymember_storedist_verify_type",
    // Error cases (avoiding those with differing error messages)
    "geoadd_invalid_long_lat_pair",
    "geodist_invalid_unit",
    "geoadd_nx_xx_is_syntax_error",
    "geoadd_latitude_out_of_range",
    "georadius_ro_negative_radius_error",
    "geoadd_incomplete_triple",
    // Wrong type errors
    "wrongtype_geoadd_on_string",
    "wrongtype_geoadd_on_string_error",
    "wrongtype_geopos_on_string_error",
    "wrongtype_geodist_on_string_error",
    "wrongtype_geohash_on_string_error",
    // Wrong arity
    "geoadd_wrong_arity_no_members",
    "geohash_wrong_arity",
    "geopos_wrong_arity",
    "geodist_wrong_arity",
    "geosearch_wrong_arity",
    "geosearchstore_wrong_arity",
    "georadius_wrong_arity",
    "georadiusbymember_wrong_arity",
];

const CORE_COPY_LIVE_STABLE_CASES: &[&str] = &[
    // Setup
    "setup_string",
    "setup_list",
    "setup_set",
    "setup_hash",
    "setup_zset",
    // Basic COPY operations
    "copy_string",
    "verify_copy_string",
    "verify_source_unchanged",
    "copy_list",
    "verify_copy_list",
    "copy_set",
    "verify_copy_set_card",
    "copy_hash",
    "verify_copy_hash",
    "copy_zset",
    "verify_copy_zset",
    // Edge cases
    "copy_nonexistent_source",
    "copy_destination_exists_no_replace",
    "verify_destination_unchanged",
    // REPLACE option
    "setup_overwrite_target",
    "copy_with_replace",
    "verify_replace_worked",
    "copy_with_replace_case_insensitive",
    "copy_replace_lowercase",
    // DB option
    "copy_with_db_option",
    "copy_with_replace_and_db",
    // Arity and syntax errors
    "copy_wrong_arity_no_args",
    "copy_wrong_arity_one_arg",
    "copy_invalid_option",
    // DUMP command
    "dump_nonexistent_key",
    "dump_wrong_arity",
    "dump_wrong_arity_extra",
    // RESTORE command errors
    "restore_wrong_arity",
    "restore_wrong_arity_two",
    "restore_wrong_arity_three",
    "restore_bad_payload",
    // Self-copy (verify only - copy_src_eq_dst behavior differs between versions)
    "verify_src_str_after_self_copy",
    // Cross-type replace
    "copy_replace_cross_type_setup",
    "copy_string_over_list_with_replace",
    "verify_cross_list_is_now_string",
    "verify_cross_list_type",
    // Type preservation
    "copy_list_preserves_type",
    "verify_list_copy_type",
    "verify_list_copy_contents",
    "copy_hash_preserves_type",
    "verify_hash_copy_type",
    "verify_hash_copy_field",
    "copy_set_preserves_type",
    "verify_set_copy_type",
    "verify_set_copy_card",
    "copy_zset_preserves_type",
    "verify_zset_copy_type",
    "verify_zset_copy_score",
    // DB operations
    "copy_with_db_nonzero",
    "verify_db_copy",
    "verify_db_copy_value",
    "copy_with_db_and_replace",
    "copy_with_db_and_replace_value",
    // Independence verification
    "copy_independent_modification",
    "copy_for_independence",
    "modify_source_after_copy",
    "verify_copy_unaffected",
    // Additional type verification
    "copy_string_type_setup",
    "copy_string_type_copy",
    "copy_string_type_verify",
    "copy_string_value_verify",
    "copy_nonexistent_returns_zero",
    "copy_bad_option",
    // Replace diff type
    "copy_replace_diff_type_setup",
    "copy_replace_string_over_list",
    "copy_replace_type_verify",
    "copy_replace_value_verify",
    // Stream COPY
    "copy_stream_setup_xadd",
    "copy_stream_setup_xadd2",
    "copy_stream_setup_group",
    "copy_stream",
    "copy_stream_type_verify",
    "copy_stream_len_verify",
    "copy_stream_encoding_verify",
    "copy_stream_data_verify",
    "copy_stream_group_preserved_setid",
    // DB parsing errors
    "copy_db_leading_plus",
    "copy_db_leading_zeros",
    "copy_db_non_numeric",
    "copy_db_negative",
    "copy_db_float",
    "copy_db_empty",
    "copy_db_missing_value",
    // RESTORE TTL parsing errors
    "restore_ttl_leading_plus",
    "restore_ttl_leading_zeros",
    "restore_ttl_non_numeric",
    "restore_ttl_float",
    "restore_ttl_empty",
];

const CORE_CONNECTION_LIVE_STABLE_CASES: &[&str] = &[
    // PING/ECHO basics
    "ping_no_args_returns_pong",
    "ping_with_message",
    "ping_wrong_arity",
    "echo_returns_argument",
    "echo_wrong_arity_no_args",
    "echo_wrong_arity_too_many",
    "echo_binary_safe",
    "ping_binary_message",
    "echo_empty_string",
    "ping_with_custom_message",
    "echo_empty_string_returns_empty",
    // Session-scoped SELECT cases
    "select_db0_ok",
    "select_db1_ok",
    "select_wrong_arity",
    "select_invalid_db",
    "select_noncanonical_plus",
    "select_noncanonical_leading_zero",
    "select_db15_ok",
    "select_db0_after_db15",
    "select_negative_db",
    "select_large_db",
    "select_too_many_args",
    "select_db_negative",
    "select_db_very_large",
    "select_db_non_integer",
    "move_same_db_errors_even_when_key_is_missing",
    // Dedicated connection/session commands
    "quit_returns_ok",
    "readonly_reports_cluster_disabled",
    "readwrite_reports_cluster_disabled",
    "readonly_wrong_arity",
    "readwrite_wrong_arity",
    "reset_returns_reset",
    "auth_no_password_configured",
];

const CORE_CONNECTION_HELLO_LIVE_CASES: &[&str] = &[
    "hello_resp2",
    "hello_resp3",
    "hello_unsupported_version",
    "hello_version_1_unsupported",
    "hello_reset_to_2",
    "hello_no_version_returns_info",
    "hello_wrong_type_version",
    "hello_version_0_unsupported",
];

const CORE_CLIENT_LIVE_STABLE_CASES: &[&str] = &[
    "client_getredir_returns_minus_one",
    "client_getredir_wrong_arity",
    "client_trackinginfo_returns_off",
    "client_tracking_on_optin",
    "client_caching_yes_optin_ok",
    "client_caching_no_optin_rejected",
    "client_tracking_bcast_requires_disable",
    "client_getredir_returns_zero_when_tracking_enabled",
    "client_trackinginfo_retains_optin_caching_yes_after_rejected_bcast_switch",
    "client_tracking_optout_requires_disable",
    "client_caching_no_without_optout_rejected",
    "client_caching_yes_remains_valid_after_rejected_optout_switch",
    "client_tracking_off_before_optout_caching_no",
    "client_tracking_on_optout",
    "client_caching_no_optout_ok",
    "client_trackinginfo_reports_optout_caching_no",
    "client_trackinginfo_wrong_arity",
    "client_tracking_off",
    "client_getredir_returns_minus_one",
];

const CORE_CLIENT_UNBLOCK_LIVE_STABLE_CASES: &[&str] = &[
    "client_unblock_wrong_arity",
    "client_unblock_nonexistent",
    "client_unblock_with_timeout",
    "client_unblock_with_error",
    "client_unblock_invalid_mode",
];

const CORE_CLIENT_PAUSE_LIVE_STABLE_CASES: &[&str] = &[
    "client_pause_wrong_arity",
    "client_unpause_wrong_arity",
    "client_pause_invalid_timeout",
    "client_pause_invalid_mode",
];

const CORE_DEBUG_LIVE_STABLE_CASES: &[&str] = &[
    // Arity error (top-level only - subcommand arity errors differ between Redis/FR)
    "debug_wrong_arity",
    // SLEEP (fast variants only - returns OK)
    "debug_sleep_basic",
    "debug_sleep_non_numeric",
    "debug_sleep_zero_fast",
    "debug_sleep_negative_ignored",
    "debug_sleep_fractional",
    "debug_sleep_very_small_decimal",
    "debug_sleep_scientific_notation",
    "debug_sleep_negative_scientific",
    "debug_case_insensitive_sleep",
    // SET-ACTIVE-EXPIRE (0/1 only - Redis accepts more values than FR)
    "debug_set_active_expire_on",
    "debug_set_active_expire_off",
    "debug_set_active_expire_case_insensitive",
    "debug_set_active_expire_multiple_toggles_on",
    "debug_set_active_expire_toggle_off",
    "debug_set_active_expire_toggle_on_again",
    // DEBUG OBJECT missing key error
    "debug_object_missing_key",
    // Setup commands for context
    "debug_setup_string",
    "debug_setup_hash",
    "debug_setup_list",
    "debug_setup_set",
    "debug_setup_zset",
    "debug_setup_stream",
    "debug_setup_hyperloglog",
    "debug_setup_int_encoded_string",
    "debug_setup_embstr_encoded",
    "debug_setup_raw_encoded",
    "debug_setup_intset",
    "debug_setup_empty_string",
    "debug_setup_empty_list",
    "debug_setup_special_chars_key",
    "debug_setup_large_integer",
    "debug_digest_setup_keys",
    // Non-deterministic outputs excluded: DEBUG OBJECT, DEBUG DIGEST, DEBUG DIGEST-VALUE
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
                "--enable-debug-command",
                "local",
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
        "bind 127.0.0.1\nport {port}\nsave \"\"\nappendonly no\nenable-debug-command local\ndir {}\n",
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

fn command_frame_owned(argv: &[String]) -> RespFrame {
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
        match parse_live_frame(&buf) {
            Ok(Some(frame)) => return frame,
            Ok(None) => {}
            Err(err) => panic!("vendored redis emitted invalid RESP: {err}"),
        }
    }
}

fn run_runtime_live_exact(
    runtime: &mut Runtime,
    live: &mut TcpStream,
    now_ms: u64,
    argv: &[&str],
) -> RespFrame {
    let frame = command_frame(argv);
    let runtime_reply = runtime.execute_frame(frame.clone(), now_ms);
    let live_reply = send_frame_and_read(live, &frame);
    assert_eq!(
        runtime_reply, live_reply,
        "runtime/live command drift for {:?}",
        argv
    );
    runtime_reply
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveResp3ParseError {
    Incomplete,
}

fn parse_live_frame(input: &[u8]) -> Result<Option<RespFrame>, String> {
    match parse_frame(input) {
        Ok(parsed) => Ok(Some(parsed.frame)),
        Err(fr_protocol::RespParseError::Incomplete) => Ok(None),
        Err(fr_protocol::RespParseError::UnsupportedResp3Type(_)) => {
            match parse_live_resp3_frame(input, 0) {
                Ok((frame, _consumed)) => Ok(Some(frame)),
                Err(LiveResp3ParseError::Incomplete) => Ok(None),
            }
        }
        Err(err) => Err(err.to_string()),
    }
}

fn parse_live_resp3_frame(
    input: &[u8],
    start: usize,
) -> Result<(RespFrame, usize), LiveResp3ParseError> {
    let prefix = *input.get(start).ok_or(LiveResp3ParseError::Incomplete)?;
    let next = start + 1;
    match prefix {
        b'+' => {
            let (line, consumed) = read_live_resp3_line(input, next)?;
            Ok((RespFrame::BulkString(Some(line.to_vec())), consumed))
        }
        b'-' => {
            let (line, consumed) = read_live_resp3_line(input, next)?;
            Ok((
                RespFrame::Error(String::from_utf8_lossy(line).to_string()),
                consumed,
            ))
        }
        b':' => {
            let (line, consumed) = read_live_resp3_line(input, next)?;
            let value = String::from_utf8_lossy(line)
                .parse::<i64>()
                .expect("vendored redis integer reply");
            Ok((RespFrame::Integer(value), consumed))
        }
        b'$' => parse_live_resp3_bulk(input, next),
        b'*' => parse_live_resp3_array(input, next),
        b'%' => parse_live_resp3_map(input, next),
        b'_' => {
            let consumed = expect_live_resp3_crlf(input, next)?;
            Ok((RespFrame::BulkString(None), consumed))
        }
        other => panic!(
            "vendored redis emitted unsupported RESP3 frame prefix: {}",
            char::from(other)
        ),
    }
}

fn parse_live_resp3_bulk(
    input: &[u8],
    start: usize,
) -> Result<(RespFrame, usize), LiveResp3ParseError> {
    let (line, consumed) = read_live_resp3_line(input, start)?;
    let len = String::from_utf8_lossy(line)
        .parse::<i64>()
        .expect("vendored redis bulk length");
    if len == -1 {
        return Ok((RespFrame::BulkString(None), consumed));
    }
    let len = usize::try_from(len).expect("vendored redis bulk length fits usize");
    let end = consumed + len + 2;
    if input.len() < end {
        return Err(LiveResp3ParseError::Incomplete);
    }
    assert_eq!(&input[consumed + len..end], b"\r\n");
    Ok((
        RespFrame::BulkString(Some(input[consumed..consumed + len].to_vec())),
        end,
    ))
}

fn parse_live_resp3_array(
    input: &[u8],
    start: usize,
) -> Result<(RespFrame, usize), LiveResp3ParseError> {
    let (line, mut cursor) = read_live_resp3_line(input, start)?;
    let len = String::from_utf8_lossy(line)
        .parse::<i64>()
        .expect("vendored redis array length");
    if len == -1 {
        return Ok((RespFrame::Array(None), cursor));
    }
    let len = usize::try_from(len).expect("vendored redis array length fits usize");
    let mut items = Vec::with_capacity(len);
    for _ in 0..len {
        let (item, consumed) = parse_live_resp3_frame(input, cursor)?;
        items.push(item);
        cursor = consumed;
    }
    Ok((RespFrame::Array(Some(items)), cursor))
}

fn parse_live_resp3_map(
    input: &[u8],
    start: usize,
) -> Result<(RespFrame, usize), LiveResp3ParseError> {
    let (line, mut cursor) = read_live_resp3_line(input, start)?;
    let len = String::from_utf8_lossy(line)
        .parse::<usize>()
        .expect("vendored redis map length");
    let mut items = Vec::with_capacity(len * 2);
    for _ in 0..len {
        let (key, consumed_after_key) = parse_live_resp3_frame(input, cursor)?;
        cursor = consumed_after_key;
        let (value, consumed_after_value) = parse_live_resp3_frame(input, cursor)?;
        cursor = consumed_after_value;
        items.push(key);
        items.push(value);
    }
    Ok((RespFrame::Array(Some(items)), cursor))
}

fn read_live_resp3_line(input: &[u8], start: usize) -> Result<(&[u8], usize), LiveResp3ParseError> {
    let tail = input.get(start..).ok_or(LiveResp3ParseError::Incomplete)?;
    let line_end = tail
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or(LiveResp3ParseError::Incomplete)?;
    let end = start + line_end;
    Ok((&input[start..end], end + 2))
}

fn expect_live_resp3_crlf(input: &[u8], start: usize) -> Result<usize, LiveResp3ParseError> {
    let bytes = input
        .get(start..start + 2)
        .ok_or(LiveResp3ParseError::Incomplete)?;
    assert_eq!(bytes, b"\r\n");
    Ok(start + 2)
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

fn parse_client_info_fields(frame: &RespFrame) -> BTreeMap<String, String> {
    let reply = bulk_text(frame);
    let line = reply
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_else(|| panic!("expected non-empty CLIENT info/list line in {reply:?}"));
    let mut fields = BTreeMap::new();
    for part in line.split_whitespace() {
        if let Some((key, value)) = part.split_once('=') {
            fields.insert(key.to_string(), value.to_string());
        }
    }
    fields
}

fn int_value(frame: &RespFrame) -> i64 {
    match frame {
        RespFrame::Integer(value) => *value,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn error_text(frame: &RespFrame) -> &str {
    match frame {
        RespFrame::Error(text) => text,
        other => panic!("expected error frame, got {other:?}"),
    }
}

fn load_named_conformance_cases(
    cfg: &HarnessConfig,
    fixture_name: &str,
    case_names: &[&str],
) -> Vec<ConformanceCase> {
    let path = cfg.fixture_root.join(fixture_name);
    let raw = fs::read_to_string(&path).expect("read conformance fixture");
    let fixture: ConformanceFixture =
        serde_json::from_str(&raw).expect("parse conformance fixture");
    case_names
        .iter()
        .map(|case_name| {
            fixture
                .cases
                .iter()
                .find(|case| case.name == *case_name)
                .unwrap_or_else(|| panic!("missing conformance case '{case_name}'"))
                .clone()
        })
        .collect()
}

fn hello_field<'a>(reply: &'a RespFrame, key: &str) -> &'a RespFrame {
    match reply {
        RespFrame::Array(Some(fields)) => fields
            .chunks_exact(2)
            .find_map(|pair| match pair {
                [RespFrame::BulkString(Some(field_name)), value]
                    if field_name.eq_ignore_ascii_case(key.as_bytes()) =>
                {
                    Some(value)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("HELLO reply missing field '{key}'")),
        RespFrame::Map(Some(fields)) => fields
            .iter()
            .find_map(|(field_name, value)| match field_name {
                RespFrame::BulkString(Some(field_name))
                    if field_name.eq_ignore_ascii_case(key.as_bytes()) =>
                {
                    Some(value)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("HELLO reply missing field '{key}'")),
        other => panic!("expected HELLO array/map reply, got {other:?}"),
    }
}

fn empty_array(frame: &RespFrame) -> bool {
    matches!(frame, RespFrame::Array(Some(items)) if items.is_empty())
}

fn is_semver_like(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn assert_hello_success_contract(reply: &RespFrame, expected_proto: i64) {
    assert_eq!(bulk_text(hello_field(reply, "server")), "redis");
    assert!(
        is_semver_like(&bulk_text(hello_field(reply, "version"))),
        "HELLO version should look like semver: {:?}",
        hello_field(reply, "version")
    );
    assert_eq!(int_value(hello_field(reply, "proto")), expected_proto);
    assert!(int_value(hello_field(reply, "id")) >= 1);
    assert_eq!(bulk_text(hello_field(reply, "mode")), "standalone");
    assert_eq!(bulk_text(hello_field(reply, "role")), "master");
    assert!(empty_array(hello_field(reply, "modules")));
}

fn assert_hello_success_pair(
    runtime_reply: &RespFrame,
    live_reply: &RespFrame,
    expected_proto: i64,
) {
    assert_hello_success_contract(runtime_reply, expected_proto);
    assert_hello_success_contract(live_reply, expected_proto);
    for key in ["server", "mode", "role"] {
        assert_eq!(
            bulk_text(hello_field(runtime_reply, key)),
            bulk_text(hello_field(live_reply, key)),
            "HELLO stable field mismatch for {key}"
        );
    }
    assert_eq!(
        int_value(hello_field(runtime_reply, "proto")),
        int_value(hello_field(live_reply, "proto"))
    );
    assert!(empty_array(hello_field(runtime_reply, "modules")));
    assert!(empty_array(hello_field(live_reply, "modules")));
}

fn assert_hello_error_prefix_pair(runtime_reply: &RespFrame, live_reply: &RespFrame, prefix: &str) {
    assert!(
        error_text(runtime_reply).starts_with(prefix),
        "runtime HELLO error should start with '{prefix}', got {:?}",
        runtime_reply
    );
    assert!(
        error_text(live_reply).starts_with(prefix),
        "live HELLO error should start with '{prefix}', got {:?}",
        live_reply
    );
}

fn assert_hello_error_contains_pair(
    runtime_reply: &RespFrame,
    live_reply: &RespFrame,
    needles: &[&str],
) {
    let runtime_error = error_text(runtime_reply);
    let live_error = error_text(live_reply);
    for needle in needles {
        assert!(
            runtime_error.contains(needle),
            "runtime HELLO error should contain '{needle}', got {runtime_error:?}"
        );
        assert!(
            live_error.contains(needle),
            "live HELLO error should contain '{needle}', got {live_error:?}"
        );
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

const CORE_ACL_LIVE_STABLE_CASES: &[&str] = &[
    "acl_whoami_returns_default",
    "acl_users_returns_default",
    "acl_setuser_creates_new_user",
    "acl_getuser_alice_returns_details",
    "acl_deluser_alice_returns_count",
    "acl_deluser_default_is_rejected",
    "acl_deluser_nonexistent_returns_zero",
    "acl_log_returns_empty_array",
    "acl_log_reset_returns_ok",
    "acl_unknown_subcommand_is_rejected",
    "acl_no_subcommand_is_rejected",
    "acl_genpass_negative_error",
    "acl_genpass_too_large_error",
    "acl_whoami_lowercase",
    "acl_users_lowercase",
    "acl_setuser_bob_on",
    "acl_setuser_bob_off",
    "acl_deluser_bob",
    "acl_setuser_allkeys_then_resetkeys",
    "acl_getuser_after_allkeys_then_resetkeys",
    "acl_setuser_resetkeys_then_allkeys",
    "acl_getuser_after_resetkeys_then_allkeys",
    "acl_deluser_cleanup_order1",
    "acl_deluser_cleanup_order2",
];

#[test]
fn core_acl_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_acl.json", CORE_ACL_LIVE_STABLE_CASES, &oracle)
            .expect("acl live diff");
    assert_eq!(report.total, report.passed, "failed: {:?}", report.failed);
    assert!(report.failed.is_empty());
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
fn core_expiry_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        align_timing_from_fixture: false,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_expiry.json",
        CORE_EXPIRY_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("expiry live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());

    let mut runtime = Runtime::default_strict();
    let mut live =
        TcpStream::connect(("127.0.0.1", oracle_server.port)).expect("connect vendored redis");
    live.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis read timeout");
    live.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis write timeout");

    let mut now_ms = 50_000u64;
    let mut run_pair = |argv: Vec<String>| {
        let frame = command_frame_owned(&argv);
        let runtime_reply = runtime.execute_frame(frame.clone(), now_ms);
        let live_reply = send_frame_and_read(&mut live, &frame);
        assert_eq!(
            runtime_reply, live_reply,
            "expiry live contract drift for {:?}",
            argv
        );
        now_ms += 1;
        runtime_reply
    };

    let _ = run_pair(vec!["FLUSHALL".to_string()]);

    let unix_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix time");
    let unix_now_ms = u64::try_from(unix_now.as_millis()).expect("unix millis fit u64");

    let exat_deadline = unix_now.as_secs() + 300;
    let exat_deadline_i64 = i64::try_from(exat_deadline).expect("EXAT deadline fits i64");
    let exat_deadline_ms_i64 =
        i64::try_from(exat_deadline.saturating_mul(1000)).expect("EXAT ms fits i64");
    let pxat_deadline = unix_now_ms + 330_000;
    let pxat_deadline_i64 = i64::try_from(pxat_deadline).expect("PXAT deadline fits i64");
    let pxat_deadline_secs_i64 =
        i64::try_from(pxat_deadline.saturating_add(500) / 1000).expect("PXAT seconds fit i64");
    let getex_exat_deadline = exat_deadline + 360;
    let getex_exat_deadline_i64 =
        i64::try_from(getex_exat_deadline).expect("GETEX EXAT deadline fits i64");
    let getex_pxat_deadline = unix_now_ms + 390_000;
    let getex_pxat_deadline_i64 =
        i64::try_from(getex_pxat_deadline).expect("GETEX PXAT deadline fits i64");
    let set_exat_deadline = exat_deadline + 420;
    let set_exat_deadline_i64 =
        i64::try_from(set_exat_deadline).expect("SET EXAT deadline fits i64");
    let set_pxat_deadline = unix_now_ms + 450_000;
    let set_pxat_deadline_i64 =
        i64::try_from(set_pxat_deadline).expect("SET PXAT deadline fits i64");

    assert_eq!(
        run_pair(vec![
            "SET".to_string(),
            "live:expiry:exat".to_string(),
            "value".to_string(),
        ]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "EXPIREAT".to_string(),
            "live:expiry:exat".to_string(),
            exat_deadline.to_string(),
        ])),
        1
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "EXPIRETIME".to_string(),
            "live:expiry:exat".to_string(),
        ])),
        exat_deadline_i64
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "PEXPIRETIME".to_string(),
            "live:expiry:exat".to_string(),
        ])),
        exat_deadline_ms_i64
    );

    assert_eq!(
        run_pair(vec![
            "SET".to_string(),
            "live:expiry:pxat".to_string(),
            "value".to_string(),
        ]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "PEXPIREAT".to_string(),
            "live:expiry:pxat".to_string(),
            pxat_deadline.to_string(),
        ])),
        1
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "PEXPIRETIME".to_string(),
            "live:expiry:pxat".to_string(),
        ])),
        pxat_deadline_i64
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "EXPIRETIME".to_string(),
            "live:expiry:pxat".to_string(),
        ])),
        pxat_deadline_secs_i64
    );

    assert_eq!(
        run_pair(vec![
            "SET".to_string(),
            "live:expiry:getex-exat".to_string(),
            "value".to_string(),
        ]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        bulk_text(&run_pair(vec![
            "GETEX".to_string(),
            "live:expiry:getex-exat".to_string(),
            "EXAT".to_string(),
            getex_exat_deadline.to_string(),
        ])),
        "value"
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "EXPIRETIME".to_string(),
            "live:expiry:getex-exat".to_string(),
        ])),
        getex_exat_deadline_i64
    );

    assert_eq!(
        run_pair(vec![
            "SET".to_string(),
            "live:expiry:getex-pxat".to_string(),
            "value".to_string(),
        ]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        bulk_text(&run_pair(vec![
            "GETEX".to_string(),
            "live:expiry:getex-pxat".to_string(),
            "PXAT".to_string(),
            getex_pxat_deadline.to_string(),
        ])),
        "value"
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "PEXPIRETIME".to_string(),
            "live:expiry:getex-pxat".to_string(),
        ])),
        getex_pxat_deadline_i64
    );

    assert_eq!(
        run_pair(vec![
            "SET".to_string(),
            "live:expiry:set-exat".to_string(),
            "value".to_string(),
            "EXAT".to_string(),
            set_exat_deadline.to_string(),
        ]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "EXPIRETIME".to_string(),
            "live:expiry:set-exat".to_string(),
        ])),
        set_exat_deadline_i64
    );

    assert_eq!(
        run_pair(vec![
            "SET".to_string(),
            "live:expiry:set-pxat".to_string(),
            "value".to_string(),
            "PXAT".to_string(),
            set_pxat_deadline.to_string(),
        ]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        int_value(&run_pair(vec![
            "PEXPIRETIME".to_string(),
            "live:expiry:set-pxat".to_string(),
        ])),
        set_pxat_deadline_i64
    );
}

#[test]
fn core_client_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_client.json").expect("client fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

const CORE_CLIENT_ADMIN_LIVE_STABLE_CASES: &[&str] = &[
    "client_getname_initially_nil",
    "client_setname_ok",
    "client_getname_after_setname",
    "client_setname_overwrite",
    "client_getname_after_overwrite",
    "client_setname_clear_with_empty",
    "client_getname_after_clear",
    "client_setname_spaces_rejected",
    "client_setname_wrong_arity",
    "client_unknown_subcommand",
    "client_wrong_arity_no_args",
    "client_setname_with_underscore",
    "client_getname_underscore",
    "client_setname_with_dash",
    "client_getname_dash",
    "client_getname_wrong_arity",
    "client_case_insensitive_subcommand",
    "client_case_insensitive_setname",
    "client_verify_lowered",
    "client_setname_empty_string",
    "client_getname_after_empty_setname",
    "client_kill_returns_zero",
    "client_kill_without_filter_rejects_missing_target",
    "client_info_wrong_arity",
];

#[test]
fn core_client_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_client.json",
        CORE_CLIENT_ADMIN_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("client admin live diff");
    assert_eq!(report.total, report.passed, "failed: {:?}", report.failed);
    assert!(report.failed.is_empty());
}

#[test]
fn core_client_tracking_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_client.json",
        CORE_CLIENT_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("client tracking live diff");
    assert_eq!(report.total, report.passed, "failed: {:?}", report.failed);
    assert!(report.failed.is_empty());
}

#[test]
fn core_client_unblock_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_client.json",
        CORE_CLIENT_UNBLOCK_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("client unblock live diff");
    assert_eq!(report.total, report.passed, "failed: {:?}", report.failed);
    assert!(report.failed.is_empty());
}

#[test]
fn core_client_pause_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_client.json",
        CORE_CLIENT_PAUSE_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("client pause live diff");
    assert_eq!(report.total, report.passed, "failed: {:?}", report.failed);
    assert!(report.failed.is_empty());
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

const CORE_PUBSUB_LIVE_STABLE_CASES: &[&str] = &[
    "subscribe_single_channel",
    "unsubscribe_mychannel",
    "unsubscribe_no_channels",
    "psubscribe_pattern",
    "punsubscribe_pattern",
    "punsubscribe_no_patterns",
    "publish_no_subscribers",
    "pubsub_channels_empty",
    "pubsub_numpat_zero",
    "publish_wrong_arity",
    "subscribe_wrong_arity",
    "psubscribe_wrong_arity",
    "pubsub_wrong_arity",
    "pubsub_unknown_subcommand",
    "pubsub_help",
    "pubsub_help_is_array",
];

#[test]
fn core_pubsub_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_pubsub.json",
        CORE_PUBSUB_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("pubsub live diff");
    assert_eq!(report.total, report.passed, "failed: {:?}", report.failed);
    assert!(report.failed.is_empty());
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

const CORE_CONFIG_LIVE_STABLE_CASES: &[&str] = &[
    "config_get_exact_maxmemory",
    "config_get_exact_bind",
    "config_get_exact_enable_protected_configs",
    "config_get_exact_hz",
    "config_get_exact_maxmemory_policy",
    "config_get_no_match",
    "config_get_prefix_pattern_cluster",
    "config_wrong_arity_no_subcommand",
    "config_get_wrong_arity",
    "config_set_wrong_arity_odd",
    "config_resetstat_wrong_arity",
    "config_rewrite_wrong_arity",
    "config_set_wrong_arity_three_values",
    "config_resetstat",
    "config_unknown_subcommand",
    "config_set_maxmemory",
    "config_get_maxmemory_after_set",
    "config_set_maxmemory_zero",
    "config_get_maxmemory_after_zero",
    "config_set_hz_value",
    "config_get_hz_after_set",
    "config_set_hz_100",
    "config_get_hz_after_100",
    "config_set_hz_1_min",
    "config_get_hz_after_1",
    "config_set_hz_500_max",
    "config_get_hz_after_500",
    "config_set_hz_non_integer",
    "config_set_hz_zero_error",
    "config_set_hz_negative_error",
    "config_set_hz_over_max_error",
    "config_get_hz_unchanged_after_errors",
];

#[test]
fn core_config_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = fr_conformance::run_live_redis_diff_for_cases(
        &cfg,
        "core_config.json",
        CORE_CONFIG_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("config live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn redis_config_file_parser_conformance_matrix() {
    // Mirrors Redis loadServerConfigFromString() and sdssplitargs() contracts.
    let parsed = parse_redis_config(
        "  # full-line comment\r\n\
         PORT 6380\n\
         rename-command \"CONFIG\" \"\"\n\
         notify-keyspace-events \"K\\x45A\\n\\r\\t\\b\\a\\\\\"\n\
         requirepass 'pa\\'ss#literal'\n\
         appendonly yes # inline comment stays args\n\
         \x0bdir /tmp\n",
    )
    .expect("config parser conformance fixture");

    let directives = parsed.directives;
    assert_eq!(directives.len(), 6);
    assert_eq!(directives[0].line_number, 2);
    assert_eq!(directives[0].name, b"port");
    assert_eq!(directives[0].args, vec![b"6380".to_vec()]);

    assert_eq!(directives[1].line_number, 3);
    assert_eq!(directives[1].name, b"rename-command");
    assert_eq!(directives[1].args, vec![b"CONFIG".to_vec(), Vec::new()]);

    assert_eq!(directives[2].line_number, 4);
    assert_eq!(directives[2].name, b"notify-keyspace-events");
    assert_eq!(directives[2].args, vec![b"KEA\n\r\t\x08\x07\\".to_vec()]);

    assert_eq!(directives[3].line_number, 5);
    assert_eq!(directives[3].name, b"requirepass");
    assert_eq!(directives[3].args, vec![b"pa'ss#literal".to_vec()]);

    assert_eq!(directives[4].line_number, 6);
    assert_eq!(directives[4].name, b"appendonly");
    assert_eq!(
        directives[4].args,
        vec![
            b"yes".to_vec(),
            b"#".to_vec(),
            b"inline".to_vec(),
            b"comment".to_vec(),
            b"stays".to_vec(),
            b"args".to_vec(),
        ]
    );

    assert_eq!(directives[5].line_number, 7);
    assert_eq!(directives[5].name, b"dir");
    assert_eq!(directives[5].args, vec![b"/tmp".to_vec()]);

    let truncated =
        parse_redis_config("port 6381\0\nbind 0.0.0.0\n").expect("nul truncates config buffer");
    assert_eq!(truncated.directives.len(), 1);
    assert_eq!(truncated.directives[0].line_number, 1);
    assert_eq!(truncated.directives[0].name, b"port");
    assert_eq!(truncated.directives[0].args, vec![b"6381".to_vec()]);

    let raw_bytes = parse_redis_config_bytes(b"requirepass \xff\n").expect("raw byte config token");
    assert_eq!(raw_bytes.directives.len(), 1);
    assert_eq!(raw_bytes.directives[0].line_number, 1);
    assert_eq!(raw_bytes.directives[0].name, b"requirepass");
    assert_eq!(raw_bytes.directives[0].args, vec![vec![0xff]]);

    let vt_comment = parse_redis_config("\x0b# not a full-line comment\n")
        .expect("vertical tab is not trimmed before Redis comment check");
    assert_eq!(vt_comment.directives.len(), 1);
    assert_eq!(vt_comment.directives[0].line_number, 1);
    assert_eq!(vt_comment.directives[0].name, b"#");
    assert_eq!(
        vt_comment.directives[0].args,
        vec![
            b"not".to_vec(),
            b"a".to_vec(),
            b"full-line".to_vec(),
            b"comment".to_vec()
        ]
    );
}

#[test]
fn redis_config_argument_splitter_conformance_matrix() {
    assert_eq!(
        split_config_line_args("foo\x0bbar baz").expect("vertical tab inside bare token"),
        vec![b"foo\x0bbar".to_vec(), b"baz".to_vec()]
    );
    assert_eq!(
        split_config_line_args("foo\x0cbar baz").expect("form feed inside bare token"),
        vec![b"foo\x0cbar".to_vec(), b"baz".to_vec()]
    );
    assert_eq!(
        split_config_line_args("\"foo\"\x0bbar").expect("vertical tab after closed quote"),
        vec![b"foo".to_vec(), b"bar".to_vec()]
    );
    assert_eq!(
        split_config_line_args("\"foo\"\x0cbar").expect("form feed after closed quote"),
        vec![b"foo".to_vec(), b"bar".to_vec()]
    );
    assert_eq!(
        split_config_line_args(r#""\x4g\xzz""#).expect("malformed hex escapes stay literal"),
        vec![b"x4gxzz".to_vec()]
    );
    assert_eq!(
        split_config_line_args(r#""a\x00b""#).expect("hex nul escape decodes inside token"),
        vec![b"a\0b".to_vec()]
    );

    let double_quote_error =
        split_config_line_args("\"foo\"bar").expect_err("adjacent double-quoted token");
    assert_eq!(
        double_quote_error,
        ConfigFileParseErrorReason::InvalidQuotedToken
    );

    let single_quote_error =
        split_config_line_args("'foo'bar").expect_err("adjacent single-quoted token");
    assert_eq!(
        single_quote_error,
        ConfigFileParseErrorReason::InvalidQuotedToken
    );

    assert_eq!(
        split_config_line_args_bytes(b"rename-command CONFIG \xfe").expect("raw byte token"),
        vec![b"rename-command".to_vec(), b"CONFIG".to_vec(), vec![0xfe]]
    );
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

const CORE_FUNCTION_LIVE_STABLE_CASES: &[&str] = &[
    "function_list_empty",
    "function_stats_empty",
    "function_load_simple",
    "function_stats_after_load",
    "fcall_not_found",
    "fcall_myfunc_returns_arg",
    "fcall_myfunc_no_args_returns_nil",
    "function_load_numeric_lib",
    "fcall_add42_returns_integer",
    "function_delete_numlib",
    "function_load_redis_call_lib",
    "fcall_myset_sets_key",
    "fcall_verify_key_was_set",
    "function_delete_setlib",
    "function_load_replace",
    "function_delete",
    "function_delete_not_found",
    "function_list_after_delete",
    "function_load_for_flush",
    "function_flush",
    "function_list_after_flush",
    "function_load_bad_header",
    "function_wrong_arity",
    "function_stats_wrong_arity",
    "function_kill_notbusy",
    "function_kill_wrong_arity",
    "fcall_wrong_arity",
    "fcall_wrong_arity_no_numkeys",
    "script_load",
    "script_exists_yes",
    "script_exists_no",
    "script_exists_mixed",
    "evalsha_loaded_script",
    "script_flush",
    "script_exists_after_flush",
    "function_unknown_subcommand",
    "function_load_missing_name",
    "function_load_wrong_arity",
    "function_delete_wrong_arity",
    "function_restore_wrong_arity",
    "function_restore_wrong_policy",
    "function_restore_extra_args",
    "fcall_ro_basic",
    "fcall_ro_returns_arg",
    "function_delete_rolib",
    "function_flush_sync",
    "function_list_after_sync_flush",
    "fcall_not_found_after_flush",
    "fcall_invalid_numkeys",
    "fcall_negative_numkeys",
    "fcall_numkeys_exceeds_args",
    "fcall_ro_wrong_arity",
    "fcall_ro_wrong_arity_no_numkeys",
    "fcall_ro_invalid_numkeys",
    "fcall_ro_negative_numkeys",
    "fcall_ro_numkeys_exceeds_args",
    "script_wrong_arity",
    "script_exists_wrong_arity",
    "script_load_wrong_arity",
    "script_unknown_subcommand",
    "script_flush_sync",
    "script_flush_async",
];

#[test]
fn core_function_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = fr_conformance::run_live_redis_diff_for_cases(
        &cfg,
        "core_function.json",
        CORE_FUNCTION_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("function live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_function_dump_restore_roundtrip_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let mut runtime = Runtime::default_strict();
    let mut live = TcpStream::connect(("127.0.0.1", oracle_server.port))
        .expect("connect to vendored redis for FUNCTION DUMP/RESTORE smoke");
    live.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis read timeout");
    live.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis write timeout");

    let start = Instant::now();
    let now_ms = || u64::try_from(start.elapsed().as_millis()).expect("elapsed millis fit u64");
    let bulk = |bytes: &[u8]| RespFrame::BulkString(Some(bytes.to_vec()));
    let library = "#!lua name=roundtriplib\nredis.register_function('roundtrip_echo', function(keys, args) return args[1] end)";

    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["FUNCTION", "FLUSH", "SYNC"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["FUNCTION", "LOAD", "REPLACE", library],
        ),
        RespFrame::BulkString(Some(b"roundtriplib".to_vec()))
    );

    let dump_frame = command_frame(&["FUNCTION", "DUMP"]);
    let runtime_dump = runtime.execute_frame(dump_frame.clone(), now_ms());
    let live_dump = send_frame_and_read(&mut live, &dump_frame);
    let runtime_dump_bytes = match runtime_dump {
        RespFrame::BulkString(Some(bytes)) => bytes,
        other => panic!("runtime FUNCTION DUMP returned unexpected reply: {other:?}"),
    };
    let live_dump_bytes = match live_dump {
        RespFrame::BulkString(Some(bytes)) => bytes,
        other => panic!("live FUNCTION DUMP returned unexpected reply: {other:?}"),
    };
    assert!(
        !runtime_dump_bytes.is_empty(),
        "runtime FUNCTION DUMP payload was empty"
    );
    assert!(
        !live_dump_bytes.is_empty(),
        "live FUNCTION DUMP payload was empty"
    );

    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["FUNCTION", "FLUSH", "SYNC"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );

    let runtime_restore = RespFrame::Array(Some(vec![
        bulk(b"FUNCTION"),
        bulk(b"RESTORE"),
        RespFrame::BulkString(Some(runtime_dump_bytes)),
    ]));
    let live_restore = RespFrame::Array(Some(vec![
        bulk(b"FUNCTION"),
        bulk(b"RESTORE"),
        RespFrame::BulkString(Some(live_dump_bytes)),
    ]));
    assert_eq!(
        runtime.execute_frame(runtime_restore, now_ms()),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        send_frame_and_read(&mut live, &live_restore),
        RespFrame::SimpleString("OK".to_string())
    );

    let function_list_frame = command_frame(&["FUNCTION", "LIST"]);
    let runtime_function_list = runtime.execute_frame(function_list_frame.clone(), now_ms());
    let live_function_list = send_frame_and_read(&mut live, &function_list_frame);
    assert_eq!(runtime_function_list, live_function_list);

    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["FCALL", "roundtrip_echo", "0", "echoed"],
        ),
        RespFrame::BulkString(Some(b"echoed".to_vec()))
    );
}

#[test]
fn core_wait_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_wait.json").expect("wait fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

const CORE_WAIT_LIVE_STABLE_CASES: &[&str] = &[
    "wait_standalone_zero_replicas",
    "wait_case_insensitive",
    "wait_after_set_still_zero",
    "wait_wrong_arity_no_args",
    "wait_wrong_arity_one_arg",
    "wait_wrong_arity_extra_args",
    "wait_invalid_numreplicas",
    "wait_negative_numreplicas",
    "wait_invalid_timeout",
    "wait_negative_timeout",
    "wait_float_numreplicas",
    "wait_float_timeout",
];

#[test]
fn core_wait_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_wait.json", CORE_WAIT_LIVE_STABLE_CASES, &oracle)
            .expect("wait live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
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

const CORE_ERRORS_LIVE_STABLE_CASES: &[&str] = &[
    "unknown_no_args",
    "unknown_with_args_preview",
    "unknown_with_long_args_preview",
    "unknown_case_preserved",
    "wrong_arity_get",
    "del_wrong_arity",
    "append_wrong_arity",
    "strlen_wrong_arity",
    "type_wrong_arity",
    "keys_wrong_arity",
    "object_unknown_subcommand",
    "setrange_negative_offset",
    "setrange_offset_not_integer",
    "getrange_start_not_integer",
    "getrange_end_not_integer",
    "lindex_wrong_arity",
    "lindex_not_integer",
    "select_not_integer",
    "select_negative",
];

#[test]
fn core_errors_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_errors.json",
        CORE_ERRORS_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("errors live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
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

const CORE_PFDEBUG_LIVE_STABLE_CASES: &[&str] = &[
    "pfselftest_returns_ok",
    "pfselftest_wrong_arity",
    "pfdebug_missing_key_getreg_error",
    "pfdebug_missing_key_encoding_error",
    "pfdebug_missing_key_todense_error",
    "pfdebug_missing_key_decode_error",
    "pfdebug_unknown_subcommand_missing_key_error",
    "pfdebug_wrong_arity_no_args",
    "pfdebug_wrong_arity_too_many",
    "pfdebug_setup_hll",
    "pfdebug_encoding_returns_sparse",
    "pfdebug_unknown_subcommand_existing_hll_error",
    "pfdebug_getreg_returns_register_array",
    "pfdebug_encoding_after_getreg_returns_dense",
    "pfdebug_todense_already_dense_returns_0",
    "pfdebug_decode_on_dense_errors",
    "pfdebug_wrongtype_list_setup",
    "pfdebug_encoding_wrongtype_list",
    "pfdebug_getreg_wrongtype_list",
    "pfdebug_todense_wrongtype_list",
    "pfdebug_decode_wrongtype_list",
    "pfdebug_unknown_subcommand_wrongtype_list_error",
    "pfdebug_invalid_hll_setup",
    "pfdebug_encoding_invalid_hll_string",
    "pfdebug_getreg_invalid_hll_string",
    "pfdebug_case_insensitive_subcommand",
    "pfdebug_case_insensitive_getreg",
    "pfdebug_case_insensitive_todense",
    "pfdebug_case_insensitive_decode",
];

#[test]
fn core_pfdebug_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_pfdebug.json",
        CORE_PFDEBUG_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("pfdebug live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_migrate_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_migrate.json").expect("migrate fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

const CORE_MIGRATE_LIVE_STABLE_CASES: &[&str] = &[
    "migrate_wrong_arity_no_args",
    "migrate_wrong_arity_too_few",
    "migrate_wrong_arity_five_args",
    "migrate_invalid_db",
    "migrate_invalid_timeout",
    "migrate_unknown_option_syntax_error",
    "migrate_auth_missing_password_syntax_error",
    "migrate_auth2_missing_args_syntax_error",
    "migrate_keys_with_nonempty_key_error",
    "migrate_nokey_nonexistent_key",
    "migrate_nokey_empty_keys_list",
    "migrate_copy_option_nokey",
    "migrate_replace_option_nokey",
    "migrate_copy_replace_combined_nokey",
    "migrate_auth_with_password_nokey",
    "migrate_auth2_with_user_pass_nokey",
    "migrate_keys_option_with_multiple_keys_nokey",
    "migrate_all_options_combined_nokey",
    "migrate_auth2_syntax_error_only_user",
    "migrate_keys_duplicate_option",
    "migrate_auth_empty_password",
    "migrate_auth2_empty_credentials",
    "migrate_options_case_insensitive_copy",
    "migrate_options_case_insensitive_replace",
    "migrate_options_case_insensitive_auth",
    "migrate_options_case_insensitive_keys",
    "migrate_all_options_different_order",
    "migrate_auth2_then_replace_copy",
];

#[test]
fn core_migrate_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_migrate.json",
        CORE_MIGRATE_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("migrate live diff");
    assert_eq!(report.total, report.passed, "failed: {:?}", report.failed);
    assert!(report.failed.is_empty());
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
            "cluster_reset",
            "cluster_getkeysinslot_with_key",
            "cluster_countkeysinslot_with_key",
            "cluster_countkeysinslot_slot_zero",
            "cluster_countkeysinslot_non_integer",
            "cluster_countkeysinslot_negative_error",
            "cluster_countkeysinslot_invalid_16384",
            "cluster_getkeysinslot_non_integer_slot",
            "cluster_getkeysinslot_negative_slot_error",
            "cluster_getkeysinslot_slot_out_of_range",
            "cluster_help",
            "cluster_unknown_subcommand",
            "cluster_keyslot_wrong_arity",
            "cluster_reset_hard",
            "cluster_reset_soft",
            "cluster_reset_invalid_arg",
            "cluster_reset_wrong_arity_extra",
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
    let oracle_server = VendoredRedisOracle::start(&cfg);
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

const CORE_SERVER_LIVE_STABLE_CASES: &[&str] = &[
    "dbsize_empty_db",
    "dbsize_returns_zero",
    "dbsize_setup_keys",
    "dbsize_returns_three",
    "dbsize_wrong_arity",
    "touch_setup",
    "touch_single_existing",
    "touch_single_missing",
    "touch_multiple_mixed",
    "touch_wrong_arity",
    "latency_latest_empty",
    "latency_history_empty",
    "latency_reset_ok",
    "latency_help",
    "move_same_db_errors_even_when_key_exists",
    "command_info_get",
    "command_list_filterby_module_unknown_returns_empty",
    "command_list_filterby_module_missing_name_syntax_error",
    "command_list_filterby_module_extra_arg_syntax_error",
    "waitaof_without_appendonly_reports_no_local_ack",
    "waitaof_min_replicas_unmet_without_appendonly",
    "unlink_setup",
    "unlink_multiple",
    "unlink_verify_gone",
    "unlink_wrong_arity",
];

#[test]
fn core_server_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_server.json",
        CORE_SERVER_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("server live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_server_move_same_db_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_server.json",
        &["move_same_db_errors_even_when_key_exists"],
        &oracle,
    )
    .expect("server move same-db live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_server_command_info_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_server.json", &["command_info_get"], &oracle)
            .expect("server command info live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_server_command_list_filterby_module_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_server.json",
        &[
            "command_list_filterby_module_unknown_returns_empty",
            "command_list_filterby_module_missing_name_syntax_error",
            "command_list_filterby_module_extra_arg_syntax_error",
        ],
        &oracle,
    )
    .expect("server command list module live diff");
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
fn core_client_no_touch_object_idletime_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let mut runtime = Runtime::default_strict();
    let mut live = TcpStream::connect(("127.0.0.1", oracle_server.port))
        .expect("connect to vendored redis for OBJECT IDLETIME smoke");
    live.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis read timeout");
    live.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis write timeout");

    let start = Instant::now();
    let now_ms = || u64::try_from(start.elapsed().as_millis()).expect("elapsed millis fit u64");

    assert_eq!(
        run_runtime_live_exact(&mut runtime, &mut live, now_ms(), &["FLUSHALL"]),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        run_runtime_live_exact(&mut runtime, &mut live, now_ms(), &["OBJECT", "HELP"]),
        RespFrame::Array(Some(vec![
            RespFrame::SimpleString(
                "OBJECT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:".to_string(),
            ),
            RespFrame::SimpleString("ENCODING <key>".to_string()),
            RespFrame::SimpleString(
                "    Return the kind of internal representation used in order to store the value"
                    .to_string(),
            ),
            RespFrame::SimpleString("    associated with a <key>.".to_string()),
            RespFrame::SimpleString("FREQ <key>".to_string()),
            RespFrame::SimpleString(
                "    Return the access frequency index of the <key>. The returned integer is"
                    .to_string(),
            ),
            RespFrame::SimpleString(
                "    proportional to the logarithm of the recent access frequency of the key."
                    .to_string(),
            ),
            RespFrame::SimpleString("IDLETIME <key>".to_string()),
            RespFrame::SimpleString(
                "    Return the idle time of the <key>, that is the approximated number of"
                    .to_string(),
            ),
            RespFrame::SimpleString(
                "    seconds elapsed since the last access to the key.".to_string(),
            ),
            RespFrame::SimpleString("REFCOUNT <key>".to_string()),
            RespFrame::SimpleString(
                "    Return the number of references of the value associated with the specified"
                    .to_string(),
            ),
            RespFrame::SimpleString("    <key>.".to_string()),
            RespFrame::SimpleString("HELP".to_string()),
            RespFrame::SimpleString("    Print this help.".to_string()),
        ]))
    );
    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["SET", "live:client:no-touch", "value"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["CLIENT", "NO-TOUCH", "ON"]
        ),
        RespFrame::SimpleString("OK".to_string())
    );

    sleep(Duration::from_millis(2_100));
    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["GET", "live:client:no-touch"]
        ),
        RespFrame::BulkString(Some(b"value".to_vec()))
    );

    sleep(Duration::from_millis(200));
    let idletime_frame = command_frame(&["OBJECT", "IDLETIME", "live:client:no-touch"]);
    let runtime_idletime = runtime.execute_frame(idletime_frame.clone(), now_ms());
    let live_idletime = send_frame_and_read(&mut live, &idletime_frame);
    let runtime_idle = int_value(&runtime_idletime);
    let live_idle = int_value(&live_idletime);
    assert!(
        runtime_idle >= 2,
        "runtime OBJECT IDLETIME should preserve pre-GET idle time under CLIENT NO-TOUCH, got {runtime_idle}"
    );
    assert!(
        live_idle >= 2,
        "live Redis OBJECT IDLETIME should preserve pre-GET idle time under CLIENT NO-TOUCH, got {live_idle}"
    );
    assert!(
        runtime_idle.abs_diff(live_idle) <= 1,
        "runtime/live OBJECT IDLETIME drifted too far under CLIENT NO-TOUCH: runtime={runtime_idle}, live={live_idle}"
    );

    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["TOUCH", "live:client:no-touch"]
        ),
        RespFrame::Integer(1)
    );
    let idletime_after_touch = command_frame(&["OBJECT", "IDLETIME", "live:client:no-touch"]);
    assert_eq!(
        runtime.execute_frame(idletime_after_touch.clone(), now_ms()),
        RespFrame::Integer(0)
    );
    assert_eq!(
        send_frame_and_read(&mut live, &idletime_after_touch),
        RespFrame::Integer(0)
    );
    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["CLIENT", "NO-TOUCH", "OFF"]
        ),
        RespFrame::SimpleString("OK".to_string())
    );
}

#[test]
fn core_client_setinfo_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let mut runtime = Runtime::default_strict();
    let mut live = TcpStream::connect(("127.0.0.1", oracle_server.port))
        .expect("connect to vendored redis for CLIENT SETINFO smoke");
    live.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis read timeout");
    live.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis write timeout");

    let start = Instant::now();
    let now_ms = || u64::try_from(start.elapsed().as_millis()).expect("elapsed millis fit u64");

    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["CLIENT", "SETINFO", "LIB-NAME", "redis-rs"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["CLIENT", "SETINFO", "lib-ver", "1.2.3"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        run_runtime_live_exact(
            &mut runtime,
            &mut live,
            now_ms(),
            &["CLIENT", "SETINFO", "LIB-NAME", "final-client"],
        ),
        RespFrame::SimpleString("OK".to_string())
    );

    let invalid_setinfo = command_frame(&["CLIENT", "SETINFO", "UNKNOWN", "value"]);
    let runtime_invalid = runtime.execute_frame(invalid_setinfo.clone(), now_ms());
    let live_invalid = send_frame_and_read(&mut live, &invalid_setinfo);
    assert_eq!(runtime_invalid, live_invalid);
    assert_eq!(
        error_text(&runtime_invalid),
        "ERR Unrecognized option 'UNKNOWN'"
    );

    let runtime_info = runtime.execute_frame(command_frame(&["CLIENT", "INFO"]), now_ms());
    let live_info = send_frame_and_read(&mut live, &command_frame(&["CLIENT", "INFO"]));
    let runtime_info_fields = parse_client_info_fields(&runtime_info);
    let live_info_fields = parse_client_info_fields(&live_info);
    for (key, expected) in [
        ("lib-name", "final-client"),
        ("lib-ver", "1.2.3"),
        ("user", "default"),
        ("resp", "2"),
        ("cmd", "client|info"),
    ] {
        assert_eq!(
            runtime_info_fields.get(key).map(String::as_str),
            Some(expected),
            "runtime CLIENT INFO missing {key}={expected}: {runtime_info:?}"
        );
        assert_eq!(
            live_info_fields.get(key).map(String::as_str),
            Some(expected),
            "live CLIENT INFO missing {key}={expected}: {live_info:?}"
        );
    }

    let runtime_list = runtime.execute_frame(
        command_frame(&["CLIENT", "LIST", "TYPE", "normal"]),
        now_ms(),
    );
    let live_list = send_frame_and_read(
        &mut live,
        &command_frame(&["CLIENT", "LIST", "TYPE", "normal"]),
    );
    let runtime_list_fields = parse_client_info_fields(&runtime_list);
    let live_list_fields = parse_client_info_fields(&live_list);
    for (key, expected) in [
        ("lib-name", "final-client"),
        ("lib-ver", "1.2.3"),
        ("user", "default"),
        ("resp", "2"),
        ("cmd", "client|list"),
    ] {
        assert_eq!(
            runtime_list_fields.get(key).map(String::as_str),
            Some(expected),
            "runtime CLIENT LIST missing {key}={expected}: {runtime_list:?}"
        );
        assert_eq!(
            live_list_fields.get(key).map(String::as_str),
            Some(expected),
            "live CLIENT LIST missing {key}={expected}: {live_list:?}"
        );
    }

    assert_eq!(
        run_runtime_live_exact(&mut runtime, &mut live, now_ms(), &["RESET"]),
        RespFrame::SimpleString("RESET".to_string())
    );

    let runtime_info_after_reset =
        runtime.execute_frame(command_frame(&["CLIENT", "INFO"]), now_ms());
    let live_info_after_reset = send_frame_and_read(&mut live, &command_frame(&["CLIENT", "INFO"]));
    let runtime_info_after_reset_fields = parse_client_info_fields(&runtime_info_after_reset);
    let live_info_after_reset_fields = parse_client_info_fields(&live_info_after_reset);
    for (key, expected) in [
        ("lib-name", "final-client"),
        ("lib-ver", "1.2.3"),
        ("user", "default"),
        ("resp", "2"),
        ("cmd", "client|info"),
    ] {
        assert_eq!(
            runtime_info_after_reset_fields.get(key).map(String::as_str),
            Some(expected),
            "runtime CLIENT INFO after RESET missing {key}={expected}: {runtime_info_after_reset:?}"
        );
        assert_eq!(
            live_info_after_reset_fields.get(key).map(String::as_str),
            Some(expected),
            "live CLIENT INFO after RESET missing {key}={expected}: {live_info_after_reset:?}"
        );
    }

    let runtime_list_after_reset = runtime.execute_frame(
        command_frame(&["CLIENT", "LIST", "TYPE", "normal"]),
        now_ms(),
    );
    let live_list_after_reset = send_frame_and_read(
        &mut live,
        &command_frame(&["CLIENT", "LIST", "TYPE", "normal"]),
    );
    let runtime_list_after_reset_fields = parse_client_info_fields(&runtime_list_after_reset);
    let live_list_after_reset_fields = parse_client_info_fields(&live_list_after_reset);
    for (key, expected) in [
        ("lib-name", "final-client"),
        ("lib-ver", "1.2.3"),
        ("user", "default"),
        ("resp", "2"),
        ("cmd", "client|list"),
    ] {
        assert_eq!(
            runtime_list_after_reset_fields.get(key).map(String::as_str),
            Some(expected),
            "runtime CLIENT LIST after RESET missing {key}={expected}: {runtime_list_after_reset:?}"
        );
        assert_eq!(
            live_list_after_reset_fields.get(key).map(String::as_str),
            Some(expected),
            "live CLIENT LIST after RESET missing {key}={expected}: {live_list_after_reset:?}"
        );
    }
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
fn core_copy_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_copy.json", CORE_COPY_LIVE_STABLE_CASES, &oracle)
            .expect("copy live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_connection_live_redis_matches_runtime() {
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
        CORE_CONNECTION_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("connection live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_connection_hello_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let hello_cases = load_named_conformance_cases(
        &cfg,
        "core_connection.json",
        CORE_CONNECTION_HELLO_LIVE_CASES,
    );
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let mut live = TcpStream::connect(("127.0.0.1", oracle_server.port))
        .expect("connect to vendored redis for HELLO coverage");
    live.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set live read timeout");
    live.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set live write timeout");

    let mut runtime = Runtime::default_strict();
    for case in hello_cases {
        let frame = RespFrame::Array(Some(
            case.argv
                .iter()
                .map(|arg| RespFrame::BulkString(Some(arg.as_bytes().to_vec())))
                .collect(),
        ));
        let runtime_reply = runtime.execute_frame(frame.clone(), case.now_ms);
        let live_reply = send_frame_and_read(&mut live, &frame);

        match case.name.as_str() {
            "hello_resp2" | "hello_reset_to_2" | "hello_no_version_returns_info" => {
                assert_hello_success_pair(&runtime_reply, &live_reply, 2);
            }
            "hello_resp3" => {
                assert_hello_success_pair(&runtime_reply, &live_reply, 3);
            }
            "hello_unsupported_version"
            | "hello_version_1_unsupported"
            | "hello_version_0_unsupported" => {
                assert_hello_error_prefix_pair(
                    &runtime_reply,
                    &live_reply,
                    "NOPROTO unsupported protocol version",
                );
            }
            "hello_wrong_type_version" => {
                assert_hello_error_contains_pair(
                    &runtime_reply,
                    &live_reply,
                    &["integer", "out of range"],
                );
            }
            other => panic!("unexpected HELLO live case {other}"),
        }
    }
}

#[test]
fn core_debug_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_debug.json",
        CORE_DEBUG_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("debug live diff");
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

#[test]
fn core_generic_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_generic.json",
        CORE_GENERIC_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("generic live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_bitmap_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_bitmap.json",
        CORE_BITMAP_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("bitmap live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_hyperloglog_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_hyperloglog.json",
        CORE_HYPERLOGLOG_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("hyperloglog live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_geo_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff_for_cases(&cfg, "core_geo.json", CORE_GEO_LIVE_STABLE_CASES, &oracle)
            .expect("geo live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}
