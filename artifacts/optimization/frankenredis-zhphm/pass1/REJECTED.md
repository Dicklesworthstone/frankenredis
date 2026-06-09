## Pass 1 Rejected: Direct SET OK Encoding

Target: baseline SET pipeline profile showed hot server-side work, but not enough evidence that reply-frame allocation was the dominant bottleneck.

Lever tested: route borrowed plain `SET` through a direct `+OK\r\n` writer instead of constructing `RespFrame::SimpleString("OK")`.

Behavior proof:
- Golden transcript covered `SET`, `GET`, `CLIENT REPLY SKIP`, another `SET`, `GET`, `CLIENT REPLY ON`, and `QUIT`.
- Baseline RESP sha256: `ca0696628ddfcf87289e11464b35f3c64362240a72c57ff0e87bc352f40f019c`.
- Candidate RESP sha256: `ca0696628ddfcf87289e11464b35f3c64362240a72c57ff0e87bc352f40f019c`.
- Ordering, tie-breaking, floating-point, and RNG behavior: unchanged; the trial only altered reply materialization for a fixed simple string.

Benchmark evidence:
- Baseline standalone hyperfine: `1.307548s mean +/- 0.042692s`, median `1.298676s`.
- Candidate standalone hyperfine: `1.525384s mean +/- 0.129549s`, median `1.529687s`.
- Paired baseline/candidate hyperfine: baseline `1.326984s mean`, candidate `1.286771s mean`, reported `1.03x +/- 0.07`.
- Reversed candidate-first retries failed during repeated startup, so no reversed keep evidence was available.

Score: Impact `0.3` x Confidence `0.2` / Effort `1.0` = `0.06`.

Decision: reject. Source changes were removed. Next pass targets the profile-backed `fr_store::canonical_string_value` / `Store::drop_if_expired` SET hotspot.
