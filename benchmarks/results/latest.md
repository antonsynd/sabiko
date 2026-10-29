# Sabiko vs C# benchmarks (2026-10-29)

**Machine:** shared 4-core cloud development container (not a CI runner)  
**CPU:** Intel(R) Xeon(R) Processor @ 2.30GHz (4 logical) | **OS:** Linux 6.18.44-fc-v185 | **.NET:** 10.0.14 | **rustc:** rustc 1.99.1 (58cf8f9c3 2026-10-13)  
**Samples:** process mode 5 warmup + 100 timed launches per program (round-robin across languages); in-process 20 warmup + 100 measured iterations  
**Percentiles:** linear interpolation between closest ranks (Hyndman-Fan type 7); stdev is the sample standard deviation

> Local run on a shared, noisy machine: another agent was running the full cargo test suite on the same 4 cores, so absolute times and tail percentiles are inflated and noisier than on a quiet runner. Ratios are more meaningful than absolute numbers; the nightly CI report is the reference.

## Configurations

- `release`: Sabiko --release (wrapping arithmetic) vs C# -optimize+ (unchecked, C#'s default) vs rustc -C opt-level=3 -C overflow-checks=off
- `checked`: Sabiko debug (overflow-checked arithmetic) vs C# -optimize+ -checked+ vs rustc -C opt-level=3 -C overflow-checks=on

## Summary (p50)

Wall = process wall time per launch (start-up included); RSS = peak resident set size; Iter = in-process time per iteration (steady state); Alloc = managed bytes allocated per in-process iteration. S/C# below 1.00x means Sabiko is faster or smaller.

| Benchmark | Config | Wall Sabiko (ms) | Wall C# (ms) | Wall S/C# | Wall Rust (ms) | RSS Sabiko (MiB) | RSS C# (MiB) | RSS S/C# | Iter Sabiko (ms) | Iter C# (ms) | Iter S/C# | Alloc Sabiko (B) | Alloc C# (B) | Alloc S/C# |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | 243 | 239 | 1.02x | 40.3 | 30.1 | 28.3 | 1.07x | 57.3 | 54.1 | 1.06x | 0 | 72 | 0.00x |
| collatz | checked | 251 | 248 | 1.01x | 64.5 | 30.2 | 28.3 | 1.07x | 63.9 | 59.6 | 1.07x | 0 | 72 | 0.00x |
| fibonacci | release | 157 | 47.1 | 3.34x | 11.2 | 30.4 | 28.4 | 1.07x | 15.0 | 11.2 | 1.33x | 0 | 80 | 0.00x |
| fibonacci | checked | 181 | 114 | 1.59x | 18.2 | 30.4 | 28.4 | 1.07x | 18.7 | 13.8 | 1.35x | 0 | 80 | 0.00x |
| lcg_hash | release | 134 | 128 | 1.04x | 103 | 30.1 | 28.1 | 1.07x | 110 | 108 | 1.02x | 0 | 88 | 0.00x |
| lcg_hash | checked | 135 | 130 | 1.04x | 103 | 30.1 | 28.1 | 1.07x | 104 | 106 | 0.99x | 0 | 88 | 0.00x |
| mandelbrot | release | 74.0 | 67.6 | 1.09x | 48.0 | 30.8 | 28.3 | 1.09x | 46.1 | 45.1 | 1.02x | 0 | 72 | 0.00x |
| mandelbrot | checked | 70.9 | 66.4 | 1.07x | 46.5 | 30.8 | 28.3 | 1.09x | 46.5 | 45.6 | 1.02x | 0 | 72 | 0.00x |
| nbody | release | 164 | 107 | 1.54x | 13.9 | 30.1 | 29.6 | 1.02x | 14.0 | 13.2 | 1.06x | 1,312 | 128 | 10.25x |
| nbody | checked | 147 | 93.2 | 1.57x | 13.4 | 30.1 | 29.5 | 1.02x | 14.0 | 13.9 | 1.01x | 1,312 | 128 | 10.25x |
| primes | release | 145 | 62.3 | 2.33x | 31.9 | 30.1 | 28.3 | 1.06x | 27.3 | 27.3 | 1.00x | 0 | 72 | 0.00x |
| primes | checked | 117 | 48.6 | 2.42x | 28.0 | 30.0 | 28.1 | 1.07x | 26.8 | 25.7 | 1.05x | 0 | 72 | 0.00x |

## Observations (written by hand for this run)

- **Steady state is close to parity.** In-process p50 is within ±7% of C# for every
  benchmark except `fibonacci` (1.33x): at Tier1, the JIT inlines C#'s `FibRecursive` into
  itself but not Sabiko's, whose IL is about three times larger (89 bytes against 27)
  because MIR building copies every operand into a temporary (decision 36).
- **Start-up costs Sabiko the most.** Process wall time, which runs mostly in Tier0 and OSR
  code, is 3.3x C# for `fibonacci`, 2.3x for `primes` and 1.5x for `nbody`. Tier0 code
  keeps every IL local in a stack slot, so temporary-heavy IL is slow there. Also, the
  shared return block is emitted before the blocks that jump to it, so each function with
  an early `return` has a backward branch, which the JIT treats as a loop. Such functions
  start in Instrumented Tier0 instead of plain Tier0 (`DOTNET_JitDisasmSummary=1` shows
  `FibRecursive [Instrumented Tier0, IL size=89]` against C#'s `[Tier0, IL size=27]`).
  Moving that block last, by hand, cut the first in-process call of `fibonacci` from about
  122 ms to 79 ms (C#: 15 ms).
- **Memory.** Peak RSS is about 2 MiB (7%) higher for Sabiko: one more assembly
  (`Sabiko.Runtime.dll`) and its console setup. Sabiko allocates nothing per iteration except
  in `nbody`: 1,312 B for two `PrintLine(f64)` calls through the Rust-compatible float
  formatter, where C# allocates 128 B. C#'s 72–88 B elsewhere are `Console.WriteLine(long)`
  formatting strings. No benchmark triggered a GC in either language.
- **Warmup is enough.** For every series, the median of the first 10 measured in-process
  iterations is within 8% of the median of the last 50.

## Sabiko/C# ratios

| Benchmark | Config | Metric | mean | p50 | p90 | p99 |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| collatz | release | Process wall time | 1.02x | 1.02x | 1.02x | 1.03x |
| collatz | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| collatz | release | In-process time per iteration | 1.10x | 1.06x | 1.12x | 1.25x |
| collatz | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| collatz | checked | Process wall time | 1.02x | 1.01x | 1.03x | 1.07x |
| collatz | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| collatz | checked | In-process time per iteration | 1.06x | 1.07x | 1.05x | 1.06x |
| collatz | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| fibonacci | release | Process wall time | 3.28x | 3.34x | 3.15x | 2.59x |
| fibonacci | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| fibonacci | release | In-process time per iteration | 1.34x | 1.33x | 1.30x | 1.26x |
| fibonacci | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| fibonacci | checked | Process wall time | 1.59x | 1.59x | 1.47x | 1.64x |
| fibonacci | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| fibonacci | checked | In-process time per iteration | 1.33x | 1.35x | 1.29x | 1.30x |
| fibonacci | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| lcg_hash | release | Process wall time | 1.03x | 1.04x | 1.02x | 0.98x |
| lcg_hash | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.08x |
| lcg_hash | release | In-process time per iteration | 1.03x | 1.02x | 1.04x | 1.16x |
| lcg_hash | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| lcg_hash | checked | Process wall time | 1.04x | 1.04x | 1.04x | 1.06x |
| lcg_hash | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| lcg_hash | checked | In-process time per iteration | 0.99x | 0.99x | 0.98x | 0.97x |
| lcg_hash | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| mandelbrot | release | Process wall time | 1.09x | 1.09x | 1.05x | 1.06x |
| mandelbrot | release | Process peak RSS | 1.09x | 1.09x | 1.09x | 1.09x |
| mandelbrot | release | In-process time per iteration | 1.04x | 1.02x | 1.09x | 1.22x |
| mandelbrot | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| mandelbrot | checked | Process wall time | 1.07x | 1.07x | 1.07x | 1.09x |
| mandelbrot | checked | Process peak RSS | 1.09x | 1.09x | 1.09x | 1.09x |
| mandelbrot | checked | In-process time per iteration | 1.02x | 1.02x | 1.01x | 1.06x |
| mandelbrot | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| nbody | release | Process wall time | 1.52x | 1.54x | 1.51x | 1.34x |
| nbody | release | Process peak RSS | 1.02x | 1.02x | 1.02x | 1.02x |
| nbody | release | In-process time per iteration | 1.05x | 1.06x | 1.04x | 1.04x |
| nbody | release | In-process allocated bytes per iteration | 10.25x | 10.25x | 10.25x | 10.25x |
| nbody | checked | Process wall time | 1.57x | 1.57x | 1.55x | 1.46x |
| nbody | checked | Process peak RSS | 1.02x | 1.02x | 1.02x | 1.02x |
| nbody | checked | In-process time per iteration | 1.03x | 1.01x | 1.09x | 1.28x |
| nbody | checked | In-process allocated bytes per iteration | 10.25x | 10.25x | 10.25x | 10.25x |
| primes | release | Process wall time | 2.17x | 2.33x | 1.89x | 1.68x |
| primes | release | Process peak RSS | 1.07x | 1.06x | 1.07x | 1.07x |
| primes | release | In-process time per iteration | 1.01x | 1.00x | 1.02x | 0.95x |
| primes | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| primes | checked | Process wall time | 2.48x | 2.42x | 2.45x | 3.09x |
| primes | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| primes | checked | In-process time per iteration | 1.07x | 1.05x | 1.07x | 1.57x |
| primes | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |

## Process wall time (ms)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 243 | 10.7 | 226 | 243 | 257 | 271 | 277 |
| collatz | release | C# | 100 | 239 | 9.78 | 223 | 239 | 251 | 262 | 269 |
| collatz | release | Rust | 100 | 41.3 | 2.87 | 38.2 | 40.3 | 45.2 | 50.9 | 52.2 |
| collatz | checked | Sabiko | 100 | 253 | 10.4 | 234 | 251 | 265 | 286 | 290 |
| collatz | checked | C# | 100 | 247 | 8.72 | 230 | 248 | 257 | 268 | 275 |
| collatz | checked | Rust | 100 | 64.5 | 4.49 | 57.5 | 64.5 | 70.5 | 75.5 | 75.8 |
| fibonacci | release | Sabiko | 100 | 162 | 19.6 | 133 | 157 | 195 | 217 | 221 |
| fibonacci | release | C# | 100 | 49.4 | 9.48 | 37.7 | 47.1 | 61.8 | 83.6 | 88.8 |
| fibonacci | release | Rust | 100 | 12.2 | 2.12 | 10.1 | 11.2 | 14.6 | 19.8 | 20.0 |
| fibonacci | checked | Sabiko | 100 | 184 | 24.1 | 143 | 181 | 211 | 258 | 262 |
| fibonacci | checked | C# | 100 | 116 | 19.4 | 84.9 | 114 | 144 | 157 | 162 |
| fibonacci | checked | Rust | 100 | 18.7 | 2.68 | 15.6 | 18.2 | 21.9 | 26.9 | 29.0 |
| lcg_hash | release | Sabiko | 100 | 134 | 5.52 | 127 | 134 | 140 | 151 | 163 |
| lcg_hash | release | C# | 100 | 130 | 6.77 | 122 | 128 | 137 | 155 | 160 |
| lcg_hash | release | Rust | 100 | 103 | 3.81 | 97.1 | 103 | 106 | 116 | 120 |
| lcg_hash | checked | Sabiko | 100 | 143 | 18.4 | 127 | 135 | 168 | 210 | 214 |
| lcg_hash | checked | C# | 100 | 138 | 17.7 | 123 | 130 | 161 | 199 | 201 |
| lcg_hash | checked | Rust | 100 | 104 | 5.69 | 97.4 | 103 | 113 | 120 | 127 |
| mandelbrot | release | Sabiko | 100 | 75.6 | 5.90 | 69.1 | 74.0 | 81.1 | 98.8 | 103 |
| mandelbrot | release | C# | 100 | 69.5 | 6.54 | 63.6 | 67.6 | 77.0 | 92.9 | 103 |
| mandelbrot | release | Rust | 100 | 48.5 | 2.20 | 45.9 | 48.0 | 50.8 | 58.1 | 58.9 |
| mandelbrot | checked | Sabiko | 100 | 72.2 | 3.86 | 68.8 | 70.9 | 75.8 | 85.4 | 87.2 |
| mandelbrot | checked | C# | 100 | 67.4 | 3.30 | 64.0 | 66.4 | 70.6 | 78.6 | 82.1 |
| mandelbrot | checked | Rust | 100 | 47.6 | 5.85 | 45.5 | 46.5 | 49.4 | 54.6 | 103 |
| nbody | release | Sabiko | 100 | 169 | 19.3 | 144 | 164 | 198 | 220 | 222 |
| nbody | release | C# | 100 | 111 | 19.6 | 89.8 | 107 | 132 | 164 | 224 |
| nbody | release | Rust | 100 | 14.6 | 1.64 | 12.9 | 13.9 | 17.0 | 19.9 | 21.0 |
| nbody | checked | Sabiko | 100 | 148 | 6.73 | 141 | 147 | 154 | 166 | 193 |
| nbody | checked | C# | 100 | 94.5 | 4.34 | 89.6 | 93.2 | 99.4 | 114 | 117 |
| nbody | checked | Rust | 100 | 13.9 | 1.17 | 12.9 | 13.4 | 15.8 | 17.2 | 18.2 |
| primes | release | Sabiko | 100 | 148 | 22.7 | 116 | 145 | 174 | 213 | 215 |
| primes | release | C# | 100 | 68.3 | 18.4 | 50.0 | 62.3 | 92.0 | 127 | 155 |
| primes | release | Rust | 100 | 32.4 | 3.99 | 27.0 | 31.9 | 37.1 | 46.3 | 47.8 |
| primes | checked | Sabiko | 100 | 120 | 11.2 | 110 | 117 | 123 | 163 | 181 |
| primes | checked | C# | 100 | 48.5 | 1.72 | 44.9 | 48.6 | 50.1 | 52.7 | 56.0 |
| primes | checked | Rust | 100 | 28.0 | 0.91 | 26.5 | 28.0 | 28.9 | 30.8 | 30.9 |

## Process peak RSS (MiB)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 30.2 | 0.1 | 29.8 | 30.1 | 30.3 | 30.4 | 30.4 |
| collatz | release | C# | 100 | 28.3 | 0.1 | 27.9 | 28.3 | 28.4 | 28.4 | 28.4 |
| collatz | release | Rust | 100 | 17.8 | 0.0 | 17.8 | 17.8 | 17.8 | 17.8 | 17.8 |
| collatz | checked | Sabiko | 100 | 30.2 | 0.1 | 29.9 | 30.2 | 30.3 | 30.4 | 30.4 |
| collatz | checked | C# | 100 | 28.2 | 0.1 | 27.9 | 28.3 | 28.3 | 28.4 | 28.4 |
| collatz | checked | Rust | 100 | 18.1 | 0.0 | 17.9 | 18.1 | 18.1 | 18.1 | 18.1 |
| fibonacci | release | Sabiko | 100 | 30.4 | 0.1 | 30.0 | 30.4 | 30.6 | 30.6 | 30.6 |
| fibonacci | release | C# | 100 | 28.3 | 0.1 | 28.0 | 28.4 | 28.5 | 28.5 | 28.6 |
| fibonacci | release | Rust | 100 | 17.8 | 0.0 | 17.8 | 17.8 | 17.8 | 17.8 | 17.8 |
| fibonacci | checked | Sabiko | 100 | 30.4 | 0.1 | 30.1 | 30.4 | 30.5 | 30.6 | 30.6 |
| fibonacci | checked | C# | 100 | 28.4 | 0.1 | 28.0 | 28.4 | 28.4 | 28.5 | 28.5 |
| fibonacci | checked | Rust | 100 | 18.1 | 0.0 | 18.1 | 18.1 | 18.1 | 18.1 | 18.1 |
| lcg_hash | release | Sabiko | 100 | 30.1 | 0.1 | 29.8 | 30.1 | 30.2 | 30.5 | 30.5 |
| lcg_hash | release | C# | 100 | 28.1 | 0.1 | 27.9 | 28.1 | 28.2 | 28.3 | 28.4 |
| lcg_hash | release | Rust | 100 | 17.9 | 0.1 | 17.8 | 17.8 | 17.9 | 17.9 | 17.9 |
| lcg_hash | checked | Sabiko | 100 | 30.1 | 0.1 | 29.7 | 30.1 | 30.3 | 30.4 | 30.5 |
| lcg_hash | checked | C# | 100 | 28.2 | 0.1 | 27.9 | 28.1 | 28.3 | 28.4 | 28.4 |
| lcg_hash | checked | Rust | 100 | 18.2 | 0.0 | 18.2 | 18.2 | 18.2 | 18.2 | 18.2 |
| mandelbrot | release | Sabiko | 100 | 30.7 | 0.1 | 30.4 | 30.8 | 30.8 | 30.8 | 30.8 |
| mandelbrot | release | C# | 100 | 28.3 | 0.1 | 28.0 | 28.3 | 28.3 | 28.4 | 28.4 |
| mandelbrot | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| mandelbrot | checked | Sabiko | 100 | 30.7 | 0.1 | 30.6 | 30.8 | 30.8 | 30.8 | 30.8 |
| mandelbrot | checked | C# | 100 | 28.3 | 0.0 | 28.2 | 28.3 | 28.3 | 28.4 | 28.4 |
| mandelbrot | checked | Rust | 100 | 18.2 | 0.0 | 18.2 | 18.2 | 18.2 | 18.2 | 18.2 |
| nbody | release | Sabiko | 100 | 30.1 | 0.1 | 30.0 | 30.1 | 30.3 | 30.3 | 30.3 |
| nbody | release | C# | 100 | 29.5 | 0.1 | 29.3 | 29.6 | 29.7 | 29.8 | 29.8 |
| nbody | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| nbody | checked | Sabiko | 100 | 30.1 | 0.0 | 30.0 | 30.1 | 30.1 | 30.2 | 30.2 |
| nbody | checked | C# | 100 | 29.5 | 0.1 | 29.4 | 29.5 | 29.6 | 29.7 | 29.7 |
| nbody | checked | Rust | 100 | 18.2 | 0.0 | 18.2 | 18.2 | 18.2 | 18.2 | 18.2 |
| primes | release | Sabiko | 100 | 30.1 | 0.1 | 29.8 | 30.1 | 30.2 | 30.4 | 30.4 |
| primes | release | C# | 100 | 28.2 | 0.1 | 28.0 | 28.3 | 28.3 | 28.4 | 28.4 |
| primes | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| primes | checked | Sabiko | 100 | 30.0 | 0.0 | 30.0 | 30.0 | 30.0 | 30.0 | 30.1 |
| primes | checked | C# | 100 | 28.1 | 0.0 | 28.0 | 28.1 | 28.2 | 28.2 | 28.3 |
| primes | checked | Rust | 100 | 18.2 | 0.1 | 18.2 | 18.2 | 18.3 | 18.3 | 18.3 |

## In-process time per iteration (ms)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 58.3 | 3.50 | 51.9 | 57.3 | 61.9 | 70.8 | 73.4 |
| collatz | release | C# | 100 | 53.1 | 2.06 | 50.0 | 54.1 | 55.1 | 56.5 | 58.2 |
| collatz | checked | Sabiko | 100 | 63.1 | 2.78 | 58.3 | 63.9 | 66.0 | 69.5 | 76.1 |
| collatz | checked | C# | 100 | 59.8 | 2.81 | 54.8 | 59.6 | 63.1 | 65.5 | 70.4 |
| fibonacci | release | Sabiko | 100 | 15.1 | 0.61 | 14.5 | 15.0 | 15.8 | 17.4 | 19.2 |
| fibonacci | release | C# | 100 | 11.3 | 0.77 | 10.4 | 11.2 | 12.1 | 13.8 | 15.4 |
| fibonacci | checked | Sabiko | 100 | 19.4 | 1.65 | 17.5 | 18.7 | 21.9 | 23.4 | 23.8 |
| fibonacci | checked | C# | 100 | 14.5 | 1.42 | 12.9 | 13.8 | 16.9 | 18.0 | 18.2 |
| lcg_hash | release | Sabiko | 100 | 111 | 3.39 | 106 | 110 | 114 | 129 | 130 |
| lcg_hash | release | C# | 100 | 107 | 2.54 | 102 | 108 | 110 | 111 | 113 |
| lcg_hash | checked | Sabiko | 100 | 105 | 1.69 | 104 | 104 | 108 | 111 | 112 |
| lcg_hash | checked | C# | 100 | 106 | 2.87 | 102 | 106 | 110 | 115 | 116 |
| mandelbrot | release | Sabiko | 100 | 46.9 | 2.99 | 44.7 | 46.1 | 50.1 | 56.0 | 66.4 |
| mandelbrot | release | C# | 100 | 45.0 | 0.86 | 43.7 | 45.1 | 46.0 | 46.0 | 46.1 |
| mandelbrot | checked | Sabiko | 100 | 46.8 | 1.15 | 45.4 | 46.5 | 47.4 | 52.0 | 53.5 |
| mandelbrot | checked | C# | 100 | 45.9 | 1.14 | 44.9 | 45.6 | 47.1 | 49.2 | 53.1 |
| nbody | release | Sabiko | 100 | 14.5 | 1.47 | 13.3 | 14.0 | 17.6 | 18.3 | 19.2 |
| nbody | release | C# | 100 | 13.8 | 1.47 | 12.9 | 13.2 | 16.9 | 17.5 | 20.2 |
| nbody | checked | Sabiko | 100 | 14.3 | 1.13 | 13.1 | 14.0 | 15.6 | 18.9 | 19.0 |
| nbody | checked | C# | 100 | 13.9 | 0.39 | 13.2 | 13.9 | 14.3 | 14.8 | 15.5 |
| primes | release | Sabiko | 100 | 28.0 | 1.76 | 26.0 | 27.3 | 30.8 | 31.7 | 32.2 |
| primes | release | C# | 100 | 27.8 | 1.97 | 25.4 | 27.3 | 30.3 | 33.4 | 33.8 |
| primes | checked | Sabiko | 100 | 27.7 | 3.55 | 26.2 | 26.8 | 28.1 | 42.0 | 48.7 |
| primes | checked | C# | 100 | 25.8 | 0.36 | 25.4 | 25.7 | 26.4 | 26.7 | 26.8 |

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
| collatz | release | Sabiko | 0 | 0 | 0 | 32.6 |
| collatz | release | C# | 0 | 0 | 0 | 31.5 |
| collatz | checked | Sabiko | 0 | 0 | 0 | 32.6 |
| collatz | checked | C# | 0 | 0 | 0 | 31.5 |
| fibonacci | release | Sabiko | 0 | 0 | 0 | 32.7 |
| fibonacci | release | C# | 0 | 0 | 0 | 31.6 |
| fibonacci | checked | Sabiko | 0 | 0 | 0 | 32.7 |
| fibonacci | checked | C# | 0 | 0 | 0 | 31.6 |
| lcg_hash | release | Sabiko | 0 | 0 | 0 | 32.7 |
| lcg_hash | release | C# | 0 | 0 | 0 | 31.6 |
| lcg_hash | checked | Sabiko | 0 | 0 | 0 | 32.7 |
| lcg_hash | checked | C# | 0 | 0 | 0 | 31.7 |
| mandelbrot | release | Sabiko | 0 | 0 | 0 | 33.1 |
| mandelbrot | release | C# | 0 | 0 | 0 | 31.6 |
| mandelbrot | checked | Sabiko | 0 | 0 | 0 | 33.0 |
| mandelbrot | checked | C# | 0 | 0 | 0 | 31.6 |
| nbody | release | Sabiko | 0 | 0 | 0 | 34.8 |
| nbody | release | C# | 0 | 0 | 0 | 33.8 |
| nbody | checked | Sabiko | 0 | 0 | 0 | 34.8 |
| nbody | checked | C# | 0 | 0 | 0 | 33.8 |
| primes | release | Sabiko | 0 | 0 | 0 | 32.6 |
| primes | release | C# | 0 | 0 | 0 | 31.5 |
| primes | checked | Sabiko | 0 | 0 | 0 | 32.6 |
| primes | checked | C# | 0 | 0 | 0 | 31.5 |

*Generated by `benchmarks/run_benchmarks.py` on 2026-10-29T01:12:25Z.*
