# Packed carrier budgets: measurements and how to reproduce them

The Luau 0.747 migration (#25, budgets in #32) replaced the integer that carried packed scalars
with a tagged light userdata (x86-64, AArch64) or an interned userdata (everywhere else). This
file records what that cost, measured against the paired Luau 0.746 integer build, and how to
measure it again. It is a record, not a CI gate.

## Budgets and verdicts

| Budget (#32) | Verdict |
|---|---|
| Binder instructions within 2 percent of the integer carrier | Met on x86-64 and AArch64: l3i's share of every bound call row within 1 instruction; returning a packed value 13 to 15 fewer |
| Non-copying native within 3 percent | Met for `Q:rotate`, `Q:mul` into a local and `Q:fromXYZW`. **`C:rgba8` is an accepted exception**: +2 instructions on x86-64, +4 on AArch64 (about 10 percent) |
| Interpreter within 3 percent | Met: the binder rows are the interpreter path |
| Reassignment stall measured and documented | About 18 cycles (12 percent) on `acc = Q:mul(acc, r)`; 57 to 65 percent more cycles on a `C:rgba8` then `C:red` chain; 2.7 to 5.3 ns on reassigned raster operations |
| Boxed carrier on wasm32 recorded | Recorded in wall time (below) |

The `C:rgba8` exception: a lowered result on the light carrier takes three stores (value,
`extra`, tag) where the integer took one value-and-tag store that Luau's optimizer could forward.
Luau's IR has no store that writes `extra` with the tag and does not track `extra`, so l3i cannot
remove the difference; it goes when Luau tracks `extra` (#34, first request). On an operation of
about 20 instructions those stores are 10 percent.

Luau 0.747 itself adds about 9 instructions to every C function call on AArch64, with or without
the binder (a raw `lua_CFunction`: 243 against 234 on 0.746), and none on x86-64. That is
upstream and is not charged to the carrier.

## Instruction counts under Cachegrind

Per call, the loop subtracted, counted by Cachegrind (software counting, so any Linux machine or
CI runner can repeat it). "l3i's share" is the row minus the raw `lua_CFunction`, which is Luau's
own call. Measured on GitHub's `ubuntu-24.04` and `ubuntu-24.04-arm` runners at commit `d5f8862`
(binder) and `524ffad` (native).

| Row | x86-64 0.747 light | x86-64 0.746 integer | AArch64 0.747 light | AArch64 0.746 integer |
|---|---:|---:|---:|---:|
| raw `lua_CFunction` | 260 | 259 | 243 | 234 |
| bound `(f64) -> f64`, l3i's share | 103 | 103 | 100 | 100 |
| bound `(Vector3) -> f64`, l3i's share | 241 | 239 | 225 | 224 |
| bound `(Packed<Color>) -> f64`, l3i's share | 244 | 243 | 228 | 227 |
| bound `(Bits64) -> f64`, l3i's share | 203 | 202 | 193 | 193 |
| returns a packed `Color`, l3i's share | 105 | 118 | 91 | 106 |
| native `Q:fromXYZW` | 96 | 93 | 189 | 185 |
| native `Q:rotate` | 104 | 103 | 153 | 153 |
| native `Q:mul` into a local | 207 | 205 | 371 | 367 |
| native `C:rgba8` | 22 | 20 | 32 | 28 to 29 |

The light carrier on Luau 0.746 (commit `0f56fec`) counts within 1 or 2 instructions of the
0.747 column on every row, so the native differences are the carrier's and the AArch64 call
difference is the pin's.

## Cycles, from CPU counters (Linux x86-64 only)

`cargo bench --bench main -- instructions` (`benches/instructions/linux.rs`) on one x86-64 Linux
machine, Luau 0.746, light carrier against integer carrier in the same build, per call:

| Row | integer instr | light instr | integer cycles | light cycles |
|---|---:|---:|---:|---:|
| bound `(Packed<Color>) -> f64` | 308 | 308 | 70.2 | 70.3 |
| bound `(Bits64) -> f64` | 293 | 293 | 65.9 | 65.7 |
| lowered `C:rgba8` | 20 | 23 | 10.7 | 17.7 |
| lowered `C:rgba8` into a local, then `C:red` | 28 | 33 | 13.8 | 21.7 |
| lowered `Q:fromXYZW` | 93 | 96 | 69.8 | 74.1 |
| lowered `Q:rotate` | 104 | 104 | 76.4 | 76.4 |
| lowered `Q:mul` into a local | 205 | 208 | 148.0 | 150.2 |
| lowered `Q:mul` reassigned | 208 | 211 | 160.1 | 178.6 |

Moving from 0.746 to 0.747 on the light carrier changed no row by more than 1.5 percent.

## Wall time, wasm32 included

`tests/carrier_cost.rs`, `record_the_bound_call_cost_per_argument_kind`: nanoseconds per call,
loop subtracted, minimum of five rounds of 200,000, release. Desktop rows from one x86-64 Linux
machine; wasm32 from the CI runner under Node over two runs.

| Per call | light, x86-64 | boxed, x86-64 | boxed, wasm32 |
|---|---:|---:|---:|
| bound `(f64) -> f64` | 19.5 | 19.0 | 36.9 to 40.9 |
| bound `(Vector3) -> f64` | 28.5 | 27.6 | 37.5 to 41.3 |
| bound `(Packed<Color>) -> f64` | 28.0 | 28.4 | 40.2 to 48.7 |
| bound `(Bits64) -> f64` | 23.5 | 26.8 | 36.7 to 39.8 |
| returns a packed `Color` (same value each call) | 19.6 | 56.2 | 67.1 to 85.2 |

## Reproducing

The harness is `tests/carrier_cost.rs`. It compiles unchanged on the two baseline commits, so
the same scenarios run on all three builds.

| Build | Commit |
|---|---|
| Luau 0.746, integer carrier | `2c6eb5498012727866c31cca12f4180e32a3e94f` |
| Luau 0.746, light carrier | `0f56feccc527d4ed1b9d936326a94beafd2eaa6f` |
| Luau 0.747, light carrier | this branch |

Instruction counts, on Linux with `valgrind`, `jq` and the toolchain l3i requires:

```sh
# Baselines in worktrees, each with the harness copied in.
for tree in integer:2c6eb5498012727866c31cca12f4180e32a3e94f light746:0f56feccc527d4ed1b9d936326a94beafd2eaa6f; do
  git worktree add "../${tree%%:*}" "${tree#*:}"
  git -C "../${tree%%:*}" submodule update --init luau
  cp tests/carrier_cost.rs "../${tree%%:*}/tests/carrier_cost.rs"
  echo 'mod carrier_cost;' >> "../${tree%%:*}/tests/main.rs"
done

binary() {
  cargo test --release --features jit --test integration --no-run --message-format=json \
    | jq -r 'select(.executable != null and .target.name == "integration") | .executable' | tail -1
}
count() { # binary scenario calls
  local test=carrier_cost::budget_scenario
  case "$2" in native_*) test=carrier_cost::native_budget_scenario ;; esac
  L3I_BUDGET_SCENARIO="$2" L3I_BUDGET_CALLS="$3" valgrind --tool=cachegrind --cache-sim=no \
    --smc-check=all-non-file --cachegrind-out-file=/dev/null "$1" --ignored --exact "$test" \
    --test-threads=1 2>&1 >/dev/null | awk '/I *refs:/ { gsub(",", "", $NF); print $NF }'
}
per_call() { # binary scenario: instructions per call, setup cancelled by two call counts
  local once twice
  once=$(count "$1" "$2" 20000); twice=$(count "$1" "$2" 40000)
  echo $(( (twice - once) / 20000 ))
}

for build in "$(binary)" "$(cd ../light746 && binary)" "$(cd ../integer && binary)"; do
  echo "== $build"
  for s in loop raw number vector color bits make_color native_loop native_fromxyzw native_rotate native_mul_local native_rgba8; do
    echo "$s $(per_call "$build" "$s")"
  done
done
```

Subtract `loop` from the binder rows and `native_loop` from the native rows; subtract `raw` (after
its own loop) for l3i's share. Remove the worktrees with `git worktree remove` afterwards.

Cycles need CPU counters (Linux, `perf_event_paranoid` at 2 or lower):
`cargo bench --features jit --bench main -- instructions`, with `L3I_CARRIER=light` on this
branch and on `2c6eb54` (where `L3I_CARRIER=integer` builds the oracle).

Wall time, on any target: `cargo test --release --test integration --
--ignored --nocapture --exact carrier_cost::record_the_bound_call_cost_per_argument_kind`
(`L3I_CARRIER=boxed` for the interned carrier on a desktop; on wasm32 the CI job's emscripten
setup and `node --stack-size=7800`).
