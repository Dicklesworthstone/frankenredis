# Redis 7.2.4 qualification for v0.1.1

The complete default stock suite was invoked on 2026-10-02 against the candidate's
production changes and the published v0.1.0 Linux x86_64 executable, in isolated
Linux scratch copies. Redis revision:
`d2c8a4b91e8c0e6aefd1f5bc0bf582cddbe046b7`. No tests, tags, skips, or watchdog
settings were changed in either complete run.

The reports are exactly JSON-equal. Both runs returned exit 1 with 330 passed
assertions, two errors, 24 upstream ignores, and only 10/89 units completed.
The server did not exit early. The stock progress watchdog stopped during
`BLPOP when new key is moved into place`; 79 units remain unqualified.
`anti_vacuity_ok` and `suite_passed` are false. These are **red complete-suite
receipts**, not evidence of full Redis compatibility.

The failing assertions are:

- `SWAPDB wants to wake blocked client, but the key already expired`.
- `MULTI + LPUSH + EXPIRE + DEBUG SLEEP on blocked client, key already expired`.

The first assertion detects redundant SELECT records in the replication stream
and throws before restoring the persistent client's DB9 selection and active
expiration. Stock test execution continues with that state. Subsequent writers
use DB1 while new deferred clients use DB9, causing the second failure and
eventual watchdog. Fresh-server checks of the unchanged individual assertions
confirm: SWAPDB fails, MULTI/expiry passes, and BLPOP/RENAME passes. These focused
checks diagnose the cascade; they do not replace the complete run.

The existing curated fidelity lane separately passes all 72 selected assertions
exactly once. Its report and terminal log are retained here. The complete-run
passed-assertion names also match v0.1.0 in the same order.

The release owner directed this mitigation release to proceed with documented
failures that the previous release never passed. The identical published-binary
baseline supplies that comparison. Full-suite green is not claimed. Follow-up:
[replication-stream fidelity and cleanup cascade](https://github.com/Dicklesworthstone/frankenredis/issues/3).

The candidate executable was built from the production changes committed as
`37fc9ccc88269baad16cc7cd0ec13c578661e619`, before the metadata-only 0.1.1
version bump. Its embedded Cargo version is therefore 0.1.0. The final DSR
release builds use the peeled v0.1.1 tag and its updated crate versions.
`provenance.json` records executable, harness, archive, and receipt hashes.

Historical v0.1.0 records also show partial runs and no complete green receipt:
two earlier 10/89-unit runs had 294 passes and five errors before timeout;
later complete dump/string units had 94 passes, and a list-unit run completed
with 270 passes and three encoding errors. No full-suite artifact was committed
to v0.1.0 or published with that release. Those older observations are context;
the current published-binary rerun is the actual release baseline comparison.
