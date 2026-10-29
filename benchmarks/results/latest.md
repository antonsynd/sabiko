# Sabiko vs C# benchmarks (2026-10-29)

**Machine:** Claude Code cloud container, 4 cores, otherwise idle  
**CPU:** Intel(R) Xeon(R) Processor @ 2.30GHz (4 logical) | **OS:** Linux 6.18.44-fc-v185 | **.NET:** 10.0.14 | **rustc:** rustc 1.99.1 (58cf8f9c3 2026-10-13)  
**Samples:** process mode 5 warmup + 100 timed launches per program (round-robin across languages); in-process 20 warmup + 100 measured iterations  
**Percentiles:** linear interpolation between closest ranks (Hyndman-Fan type 7); stdev is the sample standard deviation

## Configurations

- `release`: Sabiko --release (wrapping arithmetic) vs C# -optimize+ (unchecked, C#'s default) vs rustc -C opt-level=3 -C overflow-checks=off
- `checked`: Sabiko debug (overflow-checked arithmetic) vs C# -optimize+ -checked+ vs rustc -C opt-level=3 -C overflow-checks=on

## Summary (p50)

Wall = process wall time per launch (start-up included); RSS = peak resident set size; Iter = in-process time per iteration (steady state); Alloc = managed bytes allocated per in-process iteration. S/C# below 1.00x means Sabiko is faster or smaller.

| Benchmark | Config | Wall Sabiko (ms) | Wall C# (ms) | Wall S/C# | Wall Rust (ms) | RSS Sabiko (MiB) | RSS C# (MiB) | RSS S/C# | Iter Sabiko (ms) | Iter C# (ms) | Iter S/C# | Alloc Sabiko (B) | Alloc C# (B) | Alloc S/C# |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | 231 | 228 | 1.02x | 39.8 | 30.1 | 28.2 | 1.07x | 52.0 | 54.3 | 0.96x | 0 | 72 | 0.00x |
| collatz | checked | 246 | 228 | 1.08x | 61.6 | 30.3 | 28.2 | 1.07x | 58.0 | 56.1 | 1.03x | 0 | 72 | 0.00x |
| fibonacci | release | 143 | 37.7 | 3.81x | 10.8 | 30.4 | 28.4 | 1.07x | 15.3 | 11.0 | 1.39x | 0 | 80 | 0.00x |
| fibonacci | checked | 200 | 85.3 | 2.35x | 16.3 | 30.5 | 28.4 | 1.07x | 20.2 | 14.4 | 1.40x | 0 | 80 | 0.00x |
| lcg_hash | release | 134 | 130 | 1.03x | 104 | 30.1 | 28.1 | 1.07x | 108 | 110 | 0.98x | 0 | 88 | 0.00x |
| lcg_hash | checked | 229 | 131 | 1.74x | 104 | 30.3 | 28.1 | 1.08x | 110 | 104 | 1.06x | 0 | 88 | 0.00x |
| mandelbrot | release | 73.0 | 66.6 | 1.10x | 48.5 | 30.8 | 28.3 | 1.09x | 45.5 | 45.2 | 1.01x | 0 | 72 | 0.00x |
| mandelbrot | checked | 229 | 69.5 | 3.29x | 48.2 | 30.7 | 28.3 | 1.09x | 48.7 | 47.3 | 1.03x | 0 | 72 | 0.00x |
| nbody | release | 149 | 94.7 | 1.58x | 13.6 | 30.1 | 29.5 | 1.02x | 13.7 | 13.1 | 1.05x | 1,312 | 128 | 10.25x |
| nbody | checked | 149 | 93.6 | 1.60x | 13.4 | 30.1 | 29.5 | 1.02x | 13.3 | 13.8 | 0.96x | 1,312 | 128 | 10.25x |
| primes | release | 114 | 48.1 | 2.38x | 28.2 | 30.0 | 28.1 | 1.07x | 27.7 | 27.1 | 1.02x | 0 | 72 | 0.00x |
| primes | checked | 126 | 48.3 | 2.61x | 27.5 | 30.1 | 28.1 | 1.07x | 26.6 | 27.0 | 0.98x | 0 | 72 | 0.00x |

## Observations (written by hand for this run)

Measured on the merged tree after #28 (explicit, located overflow checks in debug builds),
on an otherwise idle 4-core container; earlier numbers in this file's history came from the
same container under load and before #28.

- **Steady state is close to parity.** In-process p50 is within about ±6% of C# for every
  benchmark except `fibonacci` (1.39x release): at Tier1, the JIT inlines C#'s
  `FibRecursive` into itself but not Sabiko's, whose IL is about three times larger
  because MIR building copies every operand into a temporary (decision 36).
- **Start-up costs Sabiko the most.** Process wall time, which runs mostly in Tier0 and OSR
  code, is 3.8x C# for `fibonacci`, 2.4x for `primes` and 1.6x for `nbody` (release).
  Tier0 code keeps every IL local in a stack slot, so temporary-heavy IL is slow there.
  The shared return block is emitted before the blocks that jump to it, so each function
  with an early `return` has a backward branch, which the JIT treats as a loop; such
  functions start in Instrumented Tier0 instead of plain Tier0. See #29.
- **Debug start-up regressed with #28.** The explicit overflow checks (decision 43) leave
  steady state unchanged but add IL that Tier0 compiles poorly: process wall time in the
  `checked` configuration went from about 71 to 229 ms for `mandelbrot` and from 135 to
  229 ms for `lcg_hash` (C#: 70 and 131 ms). Release builds are unaffected. Cold throw
  helpers and fewer temporaries (#29) are the planned fix.
- **Memory.** Peak RSS is about 2 MiB (7%) higher for Sabiko: one more assembly
  (`Sabiko.Runtime.dll`) and its console setup. Sabiko allocates nothing per iteration except
  in `nbody`: 1,312 B for two `PrintLine(f64)` calls through the Rust-compatible float
  formatter, where C# allocates 128 B. No benchmark triggered a GC in either language.

## Sabiko/C# ratios

| Benchmark | Config | Metric | mean | p50 | p90 | p99 |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| collatz | release | Process wall time | 1.02x | 1.02x | 1.02x | 1.04x |
| collatz | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.06x |
| collatz | release | In-process time per iteration | 0.96x | 0.96x | 0.96x | 0.95x |
| collatz | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| collatz | checked | Process wall time | 1.08x | 1.08x | 1.07x | 1.07x |
| collatz | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| collatz | checked | In-process time per iteration | 1.03x | 1.03x | 1.03x | 1.08x |
| collatz | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| fibonacci | release | Process wall time | 3.81x | 3.81x | 3.82x | 3.48x |
| fibonacci | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| fibonacci | release | In-process time per iteration | 1.40x | 1.39x | 1.40x | 1.41x |
| fibonacci | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| fibonacci | checked | Process wall time | 2.33x | 2.35x | 2.36x | 2.33x |
| fibonacci | checked | Process peak RSS | 1.07x | 1.07x | 1.08x | 1.08x |
| fibonacci | checked | In-process time per iteration | 1.41x | 1.40x | 1.41x | 1.40x |
| fibonacci | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| lcg_hash | release | Process wall time | 1.03x | 1.03x | 1.03x | 1.02x |
| lcg_hash | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| lcg_hash | release | In-process time per iteration | 0.98x | 0.98x | 0.98x | 0.98x |
| lcg_hash | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| lcg_hash | checked | Process wall time | 1.75x | 1.74x | 1.74x | 1.79x |
| lcg_hash | checked | Process peak RSS | 1.08x | 1.08x | 1.08x | 1.08x |
| lcg_hash | checked | In-process time per iteration | 1.06x | 1.06x | 1.06x | 1.04x |
| lcg_hash | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| mandelbrot | release | Process wall time | 1.09x | 1.10x | 1.10x | 1.09x |
| mandelbrot | release | Process peak RSS | 1.09x | 1.09x | 1.09x | 1.09x |
| mandelbrot | release | In-process time per iteration | 1.01x | 1.01x | 1.01x | 0.98x |
| mandelbrot | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| mandelbrot | checked | Process wall time | 3.31x | 3.29x | 3.26x | 3.21x |
| mandelbrot | checked | Process peak RSS | 1.08x | 1.09x | 1.09x | 1.09x |
| mandelbrot | checked | In-process time per iteration | 1.03x | 1.03x | 1.04x | 1.07x |
| mandelbrot | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| nbody | release | Process wall time | 1.57x | 1.58x | 1.57x | 1.54x |
| nbody | release | Process peak RSS | 1.02x | 1.02x | 1.02x | 1.02x |
| nbody | release | In-process time per iteration | 1.05x | 1.05x | 1.05x | 1.01x |
| nbody | release | In-process allocated bytes per iteration | 10.25x | 10.25x | 10.25x | 10.25x |
| nbody | checked | Process wall time | 1.58x | 1.60x | 1.58x | 1.65x |
| nbody | checked | Process peak RSS | 1.02x | 1.02x | 1.02x | 1.02x |
| nbody | checked | In-process time per iteration | 0.97x | 0.96x | 1.01x | 1.04x |
| nbody | checked | In-process allocated bytes per iteration | 10.25x | 10.25x | 10.25x | 10.25x |
| primes | release | Process wall time | 2.40x | 2.38x | 2.46x | 2.82x |
| primes | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| primes | release | In-process time per iteration | 1.02x | 1.02x | 1.02x | 1.00x |
| primes | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| primes | checked | Process wall time | 2.65x | 2.61x | 2.74x | 2.95x |
| primes | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| primes | checked | In-process time per iteration | 0.99x | 0.98x | 1.01x | 1.02x |
| primes | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |

## Process wall time (ms)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 232 | 3.82 | 227 | 231 | 236 | 248 | 249 |
| collatz | release | C# | 100 | 228 | 3.69 | 222 | 228 | 232 | 239 | 245 |
| collatz | release | Rust | 100 | 40.1 | 1.31 | 38.1 | 39.8 | 41.4 | 44.1 | 44.8 |
| collatz | checked | Sabiko | 100 | 246 | 3.73 | 239 | 246 | 251 | 256 | 256 |
| collatz | checked | C# | 100 | 228 | 4.71 | 220 | 228 | 234 | 239 | 241 |
| collatz | checked | Rust | 100 | 61.2 | 2.70 | 57.4 | 61.6 | 63.3 | 70.2 | 71.0 |
| fibonacci | release | Sabiko | 100 | 144 | 4.31 | 136 | 143 | 148 | 157 | 162 |
| fibonacci | release | C# | 100 | 37.8 | 1.70 | 35.3 | 37.7 | 38.8 | 45.1 | 49.1 |
| fibonacci | release | Rust | 100 | 10.9 | 0.93 | 9.94 | 10.8 | 11.1 | 16.7 | 17.2 |
| fibonacci | checked | Sabiko | 100 | 199 | 10.2 | 181 | 200 | 211 | 215 | 238 |
| fibonacci | checked | C# | 100 | 85.6 | 4.92 | 78.7 | 85.3 | 89.4 | 92.5 | 122 |
| fibonacci | checked | Rust | 100 | 16.3 | 1.09 | 15.2 | 16.3 | 16.9 | 20.7 | 23.7 |
| lcg_hash | release | Sabiko | 100 | 134 | 4.57 | 126 | 134 | 140 | 145 | 146 |
| lcg_hash | release | C# | 100 | 130 | 4.63 | 122 | 130 | 136 | 142 | 150 |
| lcg_hash | release | Rust | 100 | 103 | 3.07 | 97.5 | 104 | 106 | 111 | 114 |
| lcg_hash | checked | Sabiko | 100 | 228 | 4.68 | 220 | 229 | 232 | 244 | 254 |
| lcg_hash | checked | C# | 100 | 130 | 3.33 | 123 | 131 | 133 | 136 | 138 |
| lcg_hash | checked | Rust | 100 | 103 | 3.95 | 97.4 | 104 | 106 | 111 | 133 |
| mandelbrot | release | Sabiko | 100 | 72.9 | 3.63 | 67.5 | 73.0 | 75.8 | 84.9 | 89.8 |
| mandelbrot | release | C# | 100 | 66.7 | 2.87 | 62.1 | 66.6 | 68.9 | 77.7 | 78.3 |
| mandelbrot | release | Rust | 100 | 48.4 | 1.75 | 45.8 | 48.5 | 49.7 | 55.2 | 57.0 |
| mandelbrot | checked | Sabiko | 100 | 229 | 2.19 | 225 | 229 | 231 | 237 | 237 |
| mandelbrot | checked | C# | 100 | 69.2 | 1.85 | 64.6 | 69.5 | 71.0 | 73.7 | 76.7 |
| mandelbrot | checked | Rust | 100 | 48.2 | 1.21 | 45.7 | 48.2 | 49.5 | 50.6 | 52.6 |
| nbody | release | Sabiko | 100 | 149 | 5.21 | 140 | 149 | 156 | 160 | 165 |
| nbody | release | C# | 100 | 94.7 | 3.56 | 88.5 | 94.7 | 99.4 | 104 | 105 |
| nbody | release | Rust | 100 | 13.7 | 0.85 | 12.8 | 13.6 | 14.3 | 18.2 | 18.3 |
| nbody | checked | Sabiko | 100 | 149 | 4.84 | 142 | 149 | 154 | 163 | 164 |
| nbody | checked | C# | 100 | 94.0 | 2.89 | 89.0 | 93.6 | 97.6 | 99.2 | 102 |
| nbody | checked | Rust | 100 | 13.8 | 1.71 | 12.8 | 13.4 | 14.2 | 17.6 | 28.5 |
| primes | release | Sabiko | 100 | 116 | 9.88 | 105 | 114 | 125 | 152 | 177 |
| primes | release | C# | 100 | 48.4 | 2.08 | 45.1 | 48.1 | 51.0 | 54.0 | 56.1 |
| primes | release | Rust | 100 | 28.3 | 0.95 | 26.8 | 28.2 | 29.2 | 30.9 | 31.6 |
| primes | checked | Sabiko | 100 | 129 | 8.68 | 120 | 126 | 139 | 164 | 172 |
| primes | checked | C# | 100 | 48.7 | 2.92 | 45.5 | 48.3 | 50.7 | 55.6 | 70.9 |
| primes | checked | Rust | 100 | 27.9 | 1.08 | 26.7 | 27.5 | 28.9 | 31.2 | 33.1 |

## Process peak RSS (MiB)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 30.1 | 0.0 | 30.1 | 30.1 | 30.2 | 30.2 | 30.2 |
| collatz | release | C# | 100 | 28.2 | 0.0 | 28.1 | 28.2 | 28.2 | 28.3 | 28.3 |
| collatz | release | Rust | 100 | 17.8 | 0.0 | 17.7 | 17.8 | 17.8 | 17.8 | 17.8 |
| collatz | checked | Sabiko | 100 | 30.3 | 0.0 | 30.1 | 30.3 | 30.3 | 30.3 | 30.3 |
| collatz | checked | C# | 100 | 28.2 | 0.0 | 28.1 | 28.2 | 28.2 | 28.3 | 28.3 |
| collatz | checked | Rust | 100 | 18.0 | 0.1 | 17.9 | 17.9 | 18.0 | 18.0 | 18.0 |
| fibonacci | release | Sabiko | 100 | 30.4 | 0.0 | 30.3 | 30.4 | 30.4 | 30.4 | 30.5 |
| fibonacci | release | C# | 100 | 28.4 | 0.0 | 28.3 | 28.4 | 28.4 | 28.4 | 28.4 |
| fibonacci | release | Rust | 100 | 17.8 | 0.0 | 17.8 | 17.8 | 17.8 | 17.8 | 17.8 |
| fibonacci | checked | Sabiko | 100 | 30.5 | 0.0 | 30.3 | 30.5 | 30.5 | 30.6 | 30.6 |
| fibonacci | checked | C# | 100 | 28.4 | 0.0 | 28.3 | 28.4 | 28.4 | 28.4 | 28.4 |
| fibonacci | checked | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| lcg_hash | release | Sabiko | 100 | 30.1 | 0.0 | 30.1 | 30.1 | 30.2 | 30.2 | 30.2 |
| lcg_hash | release | C# | 100 | 28.1 | 0.0 | 28.0 | 28.1 | 28.2 | 28.2 | 28.2 |
| lcg_hash | release | Rust | 100 | 17.9 | 0.1 | 17.8 | 17.9 | 17.9 | 17.9 | 17.9 |
| lcg_hash | checked | Sabiko | 100 | 30.3 | 0.0 | 30.2 | 30.3 | 30.3 | 30.4 | 30.4 |
| lcg_hash | checked | C# | 100 | 28.1 | 0.0 | 28.0 | 28.1 | 28.1 | 28.2 | 28.2 |
| lcg_hash | checked | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| mandelbrot | release | Sabiko | 100 | 30.8 | 0.0 | 30.6 | 30.8 | 30.8 | 30.8 | 30.8 |
| mandelbrot | release | C# | 100 | 28.3 | 0.0 | 28.3 | 28.3 | 28.3 | 28.4 | 28.4 |
| mandelbrot | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| mandelbrot | checked | Sabiko | 100 | 30.7 | 0.1 | 30.6 | 30.7 | 30.8 | 30.8 | 30.8 |
| mandelbrot | checked | C# | 100 | 28.3 | 0.0 | 28.3 | 28.3 | 28.3 | 28.4 | 28.4 |
| mandelbrot | checked | Rust | 100 | 18.1 | 0.0 | 18.0 | 18.2 | 18.2 | 18.2 | 18.2 |
| nbody | release | Sabiko | 100 | 30.1 | 0.0 | 30.0 | 30.1 | 30.1 | 30.2 | 30.2 |
| nbody | release | C# | 100 | 29.5 | 0.1 | 29.4 | 29.5 | 29.6 | 29.7 | 29.7 |
| nbody | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| nbody | checked | Sabiko | 100 | 30.1 | 0.0 | 30.0 | 30.1 | 30.1 | 30.2 | 30.2 |
| nbody | checked | C# | 100 | 29.5 | 0.1 | 29.4 | 29.5 | 29.6 | 29.7 | 29.7 |
| nbody | checked | Rust | 100 | 18.2 | 0.0 | 18.2 | 18.2 | 18.2 | 18.2 | 18.2 |
| primes | release | Sabiko | 100 | 30.0 | 0.0 | 30.0 | 30.0 | 30.0 | 30.0 | 30.0 |
| primes | release | C# | 100 | 28.1 | 0.0 | 28.0 | 28.1 | 28.2 | 28.2 | 28.2 |
| primes | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| primes | checked | Sabiko | 100 | 30.1 | 0.0 | 30.0 | 30.1 | 30.2 | 30.2 | 30.2 |
| primes | checked | C# | 100 | 28.1 | 0.0 | 28.0 | 28.1 | 28.1 | 28.2 | 28.2 |
| primes | checked | Rust | 100 | 18.2 | 0.1 | 18.2 | 18.2 | 18.3 | 18.3 | 18.3 |

## In-process time per iteration (ms)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 52.2 | 0.47 | 51.7 | 52.0 | 52.9 | 53.3 | 54.4 |
| collatz | release | C# | 100 | 54.3 | 0.71 | 53.1 | 54.3 | 55.3 | 55.9 | 55.9 |
| collatz | checked | Sabiko | 100 | 58.9 | 2.66 | 56.6 | 58.0 | 61.8 | 69.2 | 69.3 |
| collatz | checked | C# | 100 | 57.2 | 2.63 | 54.5 | 56.1 | 59.7 | 63.8 | 69.9 |
| fibonacci | release | Sabiko | 100 | 15.4 | 0.35 | 14.9 | 15.3 | 15.7 | 15.9 | 17.6 |
| fibonacci | release | C# | 100 | 11.0 | 0.22 | 10.5 | 11.0 | 11.2 | 11.3 | 11.5 |
| fibonacci | checked | Sabiko | 100 | 20.2 | 0.48 | 19.1 | 20.2 | 20.7 | 21.4 | 22.6 |
| fibonacci | checked | C# | 100 | 14.4 | 0.34 | 13.7 | 14.4 | 14.7 | 15.3 | 15.5 |
| lcg_hash | release | Sabiko | 100 | 108 | 1.98 | 105 | 108 | 110 | 111 | 116 |
| lcg_hash | release | C# | 100 | 110 | 1.90 | 105 | 110 | 112 | 113 | 116 |
| lcg_hash | checked | Sabiko | 100 | 110 | 1.77 | 106 | 110 | 111 | 112 | 121 |
| lcg_hash | checked | C# | 100 | 104 | 1.37 | 102 | 104 | 105 | 108 | 108 |
| mandelbrot | release | Sabiko | 100 | 45.8 | 1.04 | 44.7 | 45.5 | 47.0 | 49.3 | 50.0 |
| mandelbrot | release | C# | 100 | 45.2 | 1.30 | 43.4 | 45.2 | 46.5 | 50.3 | 51.3 |
| mandelbrot | checked | Sabiko | 100 | 48.7 | 0.96 | 46.7 | 48.7 | 49.5 | 52.2 | 52.5 |
| mandelbrot | checked | C# | 100 | 47.3 | 0.53 | 45.5 | 47.3 | 47.6 | 48.6 | 48.8 |
| nbody | release | Sabiko | 100 | 13.8 | 0.40 | 13.3 | 13.7 | 14.1 | 15.1 | 16.2 |
| nbody | release | C# | 100 | 13.2 | 0.46 | 12.8 | 13.1 | 13.4 | 14.9 | 16.8 |
| nbody | checked | Sabiko | 100 | 13.6 | 0.79 | 13.0 | 13.3 | 14.5 | 16.7 | 16.8 |
| nbody | checked | C# | 100 | 13.9 | 0.67 | 13.2 | 13.8 | 14.4 | 16.0 | 18.6 |
| primes | release | Sabiko | 100 | 27.6 | 0.52 | 26.5 | 27.7 | 28.0 | 28.5 | 30.1 |
| primes | release | C# | 100 | 27.0 | 0.50 | 25.5 | 27.1 | 27.5 | 28.6 | 29.8 |
| primes | checked | Sabiko | 100 | 26.7 | 0.64 | 25.9 | 26.6 | 27.8 | 28.5 | 28.5 |
| primes | checked | C# | 100 | 27.0 | 0.42 | 26.2 | 27.0 | 27.6 | 27.9 | 28.4 |

## In-process allocated bytes per iteration (B)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| collatz | release | C# | 100 | 72 | 0 | 72 | 72 | 72 | 72 | 72 |
| collatz | checked | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| collatz | checked | C# | 100 | 72 | 0 | 72 | 72 | 72 | 72 | 72 |
| fibonacci | release | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| fibonacci | release | C# | 100 | 80 | 0 | 80 | 80 | 80 | 80 | 80 |
| fibonacci | checked | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| fibonacci | checked | C# | 100 | 80 | 0 | 80 | 80 | 80 | 80 | 80 |
| lcg_hash | release | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| lcg_hash | release | C# | 100 | 88 | 0 | 88 | 88 | 88 | 88 | 88 |
| lcg_hash | checked | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| lcg_hash | checked | C# | 100 | 88 | 0 | 88 | 88 | 88 | 88 | 88 |
| mandelbrot | release | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| mandelbrot | release | C# | 100 | 72 | 0 | 72 | 72 | 72 | 72 | 72 |
| mandelbrot | checked | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| mandelbrot | checked | C# | 100 | 72 | 0 | 72 | 72 | 72 | 72 | 72 |
| nbody | release | Sabiko | 100 | 1,312 | 0 | 1,312 | 1,312 | 1,312 | 1,312 | 1,312 |
| nbody | release | C# | 100 | 128 | 0 | 128 | 128 | 128 | 128 | 128 |
| nbody | checked | Sabiko | 100 | 1,312 | 0 | 1,312 | 1,312 | 1,312 | 1,312 | 1,312 |
| nbody | checked | C# | 100 | 128 | 0 | 128 | 128 | 128 | 128 | 128 |
| primes | release | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| primes | release | C# | 100 | 72 | 0 | 72 | 72 | 72 | 72 | 72 |
| primes | checked | Sabiko | 100 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| primes | checked | C# | 100 | 72 | 0 | 72 | 72 | 72 | 72 | 72 |

## In-process GC collections (over the measured iterations) and peak working set

| Benchmark | Config | Language | gen0 | gen1 | gen2 | peak WS (MiB) |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 0 | 0 | 0 | 32.5 |
| collatz | release | C# | 0 | 0 | 0 | 31.5 |
| collatz | checked | Sabiko | 0 | 0 | 0 | 32.7 |
| collatz | checked | C# | 0 | 0 | 0 | 31.5 |
| fibonacci | release | Sabiko | 0 | 0 | 0 | 32.7 |
| fibonacci | release | C# | 0 | 0 | 0 | 31.5 |
| fibonacci | checked | Sabiko | 0 | 0 | 0 | 32.9 |
| fibonacci | checked | C# | 0 | 0 | 0 | 31.5 |
| lcg_hash | release | Sabiko | 0 | 0 | 0 | 32.8 |
| lcg_hash | release | C# | 0 | 0 | 0 | 31.6 |
| lcg_hash | checked | Sabiko | 0 | 0 | 0 | 32.9 |
| lcg_hash | checked | C# | 0 | 0 | 0 | 31.7 |
| mandelbrot | release | Sabiko | 0 | 0 | 0 | 33.1 |
| mandelbrot | release | C# | 0 | 0 | 0 | 31.6 |
| mandelbrot | checked | Sabiko | 0 | 0 | 0 | 33.1 |
| mandelbrot | checked | C# | 0 | 0 | 0 | 31.7 |
| nbody | release | Sabiko | 0 | 0 | 0 | 35.3 |
| nbody | release | C# | 0 | 0 | 0 | 33.9 |
| nbody | checked | Sabiko | 0 | 0 | 0 | 34.8 |
| nbody | checked | C# | 0 | 0 | 0 | 33.8 |
| primes | release | Sabiko | 0 | 0 | 0 | 32.6 |
| primes | release | C# | 0 | 0 | 0 | 31.5 |
| primes | checked | Sabiko | 0 | 0 | 0 | 32.7 |
| primes | checked | C# | 0 | 0 | 0 | 31.5 |

*Generated by `benchmarks/run_benchmarks.py` on 2026-10-29T01:25:00Z.*
