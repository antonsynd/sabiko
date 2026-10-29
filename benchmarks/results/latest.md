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
| collatz | release | 231 | 231 | 1.00x | 40.9 | 30.1 | 28.1 | 1.07x | 51.4 | 53.8 | 0.95x | 0 | 72 | 0.00x |
| collatz | checked | 248 | 230 | 1.08x | 58.3 | 30.2 | 28.1 | 1.07x | 61.2 | 67.1 | 0.91x | 0 | 72 | 0.00x |
| fibonacci | release | 42.6 | 38.1 | 1.12x | 10.6 | 30.3 | 28.3 | 1.07x | 11.2 | 11.2 | 1.00x | 0 | 80 | 0.00x |
| fibonacci | checked | 156 | 89.2 | 1.75x | 16.2 | 30.5 | 28.3 | 1.08x | 19.7 | 15.0 | 1.32x | 0 | 80 | 0.00x |
| lcg_hash | release | 138 | 133 | 1.04x | 104 | 30.1 | 28.0 | 1.07x | 107 | 107 | 1.00x | 0 | 88 | 0.00x |
| lcg_hash | checked | 139 | 132 | 1.06x | 102 | 30.1 | 28.0 | 1.07x | 107 | 106 | 1.01x | 0 | 88 | 0.00x |
| mandelbrot | release | 75.5 | 69.9 | 1.08x | 48.7 | 30.6 | 28.3 | 1.08x | 47.2 | 46.1 | 1.02x | 0 | 72 | 0.00x |
| mandelbrot | checked | 80.0 | 74.1 | 1.08x | 49.6 | 30.7 | 28.3 | 1.09x | 48.0 | 47.7 | 1.00x | 0 | 72 | 0.00x |
| nbody | release | 116 | 98.0 | 1.18x | 14.0 | 30.2 | 29.5 | 1.03x | 13.1 | 13.4 | 0.98x | 1,312 | 128 | 10.25x |
| nbody | checked | 120 | 101 | 1.18x | 14.2 | 30.3 | 29.5 | 1.03x | 13.8 | 13.3 | 1.03x | 1,312 | 128 | 10.25x |
| primes | release | 52.1 | 48.8 | 1.07x | 27.4 | 30.1 | 28.0 | 1.07x | 26.0 | 26.0 | 1.00x | 0 | 72 | 0.00x |
| primes | checked | 107 | 52.1 | 2.05x | 28.8 | 30.1 | 28.0 | 1.07x | 27.3 | 26.4 | 1.04x | 0 | 72 | 0.00x |

## Observations (written by hand for this run)

Measured after #29 (JIT-friendly IL: block layout, throw helpers for failed checks,
single-use values on the evaluation stack and copy forwarding, decision 51), on an
otherwise idle 4-core container. The previous version of this file, measured on the same
container just before #29, is the baseline in the comparisons below.

- **Start-up gaps closed in release builds.** Process wall time (release, p50) went from
  3.8x C# to 1.12x for `fibonacci` (141 to 43 ms), from 2.4x to 1.07x for `primes` (112 to
  52 ms) and from 1.6x to 1.18x for `nbody` (149 to 116 ms). Methods with an early
  `return` no longer have a backward branch, so they start in plain Tier0 instead of
  instrumented Tier0, and Tier0 code is much smaller once single-use temporaries stay on
  the evaluation stack (`FibRecursive`: 89 bytes of IL and 239 bytes of Tier0 code before,
  56 and 73 after; C#: 27 and 73).
- **The debug start-up regression of #28 is gone for `mandelbrot` and `lcg_hash`.** In the
  `checked` configuration they went from 3.3x and 1.8x C# to 1.08x and 1.06x (227 to 80 ms
  and 225 to 139 ms): failed checks call throw helpers in `Sabiko.Runtime.Checks` instead
  of building the exception inline, which made `Escape` small enough for the JIT to inline
  into `Main`'s OSR code, as it does in release builds and for C#.
- **Steady state is at parity except checked `fibonacci`.** In-process p50 is within 5%
  of C# everywhere but there; release `fibonacci` went from 1.37x to 1.00x, because the JIT
  now inlines `FibRecursive` into itself at Tier1, as it does C#'s. Debug builds check
  64-bit arithmetic through `Sabiko.Runtime.Arithmetic` helpers with a scratch local
  (decision 43), which keeps checked `FibRecursive` at 128 bytes of IL (C#'s `-checked+`
  version: 27), too large to inline into itself: 1.32x in-process, 1.75x process.
  `primes` in the `checked` configuration (2.05x process) also still pays for its checked
  32-bit multiply and remainder in OSR code.
- **Memory.** Peak RSS is about 2 MiB (7%) higher for Sabiko: one more assembly
  (`Sabiko.Runtime.dll`) and its console setup. Sabiko allocates nothing per iteration except
  in `nbody`: 1,312 B for two `PrintLine(f64)` calls through the Rust-compatible float
  formatter, where C# allocates 128 B. No benchmark triggered a GC in either language.

## Sabiko/C# ratios

| Benchmark | Config | Metric | mean | p50 | p90 | p99 |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| collatz | release | Process wall time | 1.00x | 1.00x | 1.00x | 1.00x |
| collatz | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| collatz | release | In-process time per iteration | 0.98x | 0.95x | 1.00x | 0.98x |
| collatz | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| collatz | checked | Process wall time | 1.08x | 1.08x | 1.08x | 1.08x |
| collatz | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| collatz | checked | In-process time per iteration | 0.92x | 0.91x | 0.95x | 0.85x |
| collatz | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| fibonacci | release | Process wall time | 1.13x | 1.12x | 1.11x | 1.15x |
| fibonacci | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.08x |
| fibonacci | release | In-process time per iteration | 0.99x | 1.00x | 1.01x | 1.01x |
| fibonacci | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| fibonacci | checked | Process wall time | 1.74x | 1.75x | 1.81x | 1.69x |
| fibonacci | checked | Process peak RSS | 1.07x | 1.08x | 1.08x | 1.08x |
| fibonacci | checked | In-process time per iteration | 1.32x | 1.32x | 1.35x | 1.75x |
| fibonacci | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| lcg_hash | release | Process wall time | 1.04x | 1.04x | 1.05x | 1.08x |
| lcg_hash | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| lcg_hash | release | In-process time per iteration | 1.00x | 1.00x | 0.99x | 0.93x |
| lcg_hash | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| lcg_hash | checked | Process wall time | 1.06x | 1.06x | 1.08x | 1.08x |
| lcg_hash | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| lcg_hash | checked | In-process time per iteration | 1.00x | 1.01x | 1.00x | 1.00x |
| lcg_hash | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| mandelbrot | release | Process wall time | 1.07x | 1.08x | 1.06x | 0.97x |
| mandelbrot | release | Process peak RSS | 1.08x | 1.08x | 1.08x | 1.08x |
| mandelbrot | release | In-process time per iteration | 1.02x | 1.02x | 1.04x | 1.06x |
| mandelbrot | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| mandelbrot | checked | Process wall time | 1.09x | 1.08x | 1.11x | 1.15x |
| mandelbrot | checked | Process peak RSS | 1.09x | 1.09x | 1.09x | 1.09x |
| mandelbrot | checked | In-process time per iteration | 0.99x | 1.00x | 0.98x | 1.00x |
| mandelbrot | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| nbody | release | Process wall time | 1.18x | 1.18x | 1.19x | 1.16x |
| nbody | release | Process peak RSS | 1.03x | 1.03x | 1.02x | 1.02x |
| nbody | release | In-process time per iteration | 0.97x | 0.98x | 0.96x | 0.78x |
| nbody | release | In-process allocated bytes per iteration | 10.25x | 10.25x | 10.25x | 10.25x |
| nbody | checked | Process wall time | 1.18x | 1.18x | 1.17x | 1.15x |
| nbody | checked | Process peak RSS | 1.03x | 1.03x | 1.02x | 1.03x |
| nbody | checked | In-process time per iteration | 1.02x | 1.03x | 1.02x | 1.04x |
| nbody | checked | In-process allocated bytes per iteration | 10.25x | 10.25x | 10.25x | 10.25x |
| primes | release | Process wall time | 1.07x | 1.07x | 1.07x | 1.12x |
| primes | release | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| primes | release | In-process time per iteration | 1.00x | 1.00x | 1.01x | 1.04x |
| primes | release | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |
| primes | checked | Process wall time | 2.14x | 2.05x | 2.46x | 2.66x |
| primes | checked | Process peak RSS | 1.07x | 1.07x | 1.07x | 1.07x |
| primes | checked | In-process time per iteration | 1.03x | 1.04x | 1.07x | 1.02x |
| primes | checked | In-process allocated bytes per iteration | 0.00x | 0.00x | 0.00x | 0.00x |

## Process wall time (ms)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 231 | 4.92 | 222 | 231 | 237 | 243 | 249 |
| collatz | release | C# | 100 | 231 | 3.92 | 225 | 231 | 236 | 243 | 244 |
| collatz | release | Rust | 100 | 40.8 | 1.52 | 38.7 | 40.9 | 42.3 | 45.6 | 47.2 |
| collatz | checked | Sabiko | 100 | 249 | 4.34 | 242 | 248 | 255 | 261 | 262 |
| collatz | checked | C# | 100 | 230 | 4.56 | 223 | 230 | 236 | 242 | 244 |
| collatz | checked | Rust | 100 | 59.8 | 3.78 | 56.5 | 58.3 | 61.6 | 75.2 | 76.4 |
| fibonacci | release | Sabiko | 100 | 44.9 | 5.36 | 40.3 | 42.6 | 52.5 | 61.9 | 62.9 |
| fibonacci | release | C# | 100 | 39.9 | 4.70 | 35.8 | 38.1 | 47.2 | 53.8 | 55.3 |
| fibonacci | release | Rust | 100 | 11.2 | 1.53 | 9.93 | 10.6 | 13.5 | 16.4 | 16.9 |
| fibonacci | checked | Sabiko | 100 | 158 | 11.7 | 138 | 156 | 178 | 199 | 201 |
| fibonacci | checked | C# | 100 | 90.9 | 7.18 | 81.9 | 89.2 | 98.6 | 118 | 134 |
| fibonacci | checked | Rust | 100 | 16.3 | 1.05 | 14.8 | 16.2 | 17.4 | 20.4 | 20.8 |
| lcg_hash | release | Sabiko | 100 | 138 | 4.77 | 130 | 138 | 145 | 154 | 154 |
| lcg_hash | release | C# | 100 | 133 | 4.58 | 124 | 133 | 137 | 142 | 161 |
| lcg_hash | release | Rust | 100 | 104 | 3.90 | 97.2 | 104 | 107 | 122 | 122 |
| lcg_hash | checked | Sabiko | 100 | 140 | 5.30 | 133 | 139 | 148 | 155 | 157 |
| lcg_hash | checked | C# | 100 | 132 | 4.18 | 126 | 132 | 137 | 143 | 148 |
| lcg_hash | checked | Rust | 100 | 103 | 2.92 | 98.0 | 102 | 107 | 109 | 109 |
| mandelbrot | release | Sabiko | 100 | 76.9 | 4.12 | 71.2 | 75.5 | 83.4 | 88.8 | 93.1 |
| mandelbrot | release | C# | 100 | 71.8 | 5.67 | 65.1 | 69.9 | 78.7 | 91.7 | 98.3 |
| mandelbrot | release | Rust | 100 | 49.4 | 2.65 | 46.4 | 48.7 | 52.3 | 59.6 | 59.9 |
| mandelbrot | checked | Sabiko | 100 | 81.9 | 5.15 | 75.2 | 80.0 | 88.7 | 97.5 | 98.8 |
| mandelbrot | checked | C# | 100 | 75.0 | 3.51 | 68.8 | 74.1 | 79.6 | 84.5 | 88.1 |
| mandelbrot | checked | Rust | 100 | 50.0 | 2.48 | 46.9 | 49.6 | 51.6 | 58.5 | 65.6 |
| nbody | release | Sabiko | 100 | 117 | 3.82 | 110 | 116 | 121 | 129 | 137 |
| nbody | release | C# | 100 | 98.6 | 3.48 | 93.3 | 98.0 | 101 | 111 | 114 |
| nbody | release | Rust | 100 | 14.3 | 1.63 | 13.3 | 14.0 | 15.7 | 18.6 | 26.3 |
| nbody | checked | Sabiko | 100 | 121 | 5.08 | 115 | 120 | 126 | 135 | 152 |
| nbody | checked | C# | 100 | 102 | 4.39 | 95.3 | 101 | 107 | 117 | 118 |
| nbody | checked | Rust | 100 | 14.6 | 1.26 | 13.4 | 14.2 | 16.6 | 18.9 | 19.5 |
| primes | release | Sabiko | 100 | 52.9 | 3.09 | 48.7 | 52.1 | 56.7 | 63.2 | 66.4 |
| primes | release | C# | 100 | 49.5 | 2.62 | 46.1 | 48.8 | 53.0 | 56.5 | 58.8 |
| primes | release | Rust | 100 | 27.9 | 1.04 | 26.5 | 27.4 | 29.2 | 30.9 | 33.3 |
| primes | checked | Sabiko | 100 | 113 | 17.1 | 101 | 107 | 138 | 169 | 200 |
| primes | checked | C# | 100 | 52.9 | 3.13 | 49.9 | 52.1 | 56.0 | 63.6 | 70.0 |
| primes | checked | Rust | 100 | 29.0 | 2.83 | 27.4 | 28.8 | 29.5 | 32.9 | 55.9 |

## Process peak RSS (MiB)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 30.1 | 0.0 | 30.1 | 30.1 | 30.1 | 30.1 | 30.1 |
| collatz | release | C# | 100 | 28.2 | 0.0 | 28.1 | 28.1 | 28.3 | 28.3 | 28.3 |
| collatz | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| collatz | checked | Sabiko | 100 | 30.2 | 0.0 | 30.1 | 30.2 | 30.2 | 30.2 | 30.3 |
| collatz | checked | C# | 100 | 28.1 | 0.0 | 28.1 | 28.1 | 28.1 | 28.3 | 28.3 |
| collatz | checked | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| fibonacci | release | Sabiko | 100 | 30.4 | 0.0 | 30.3 | 30.3 | 30.4 | 30.5 | 30.5 |
| fibonacci | release | C# | 100 | 28.3 | 0.0 | 28.3 | 28.3 | 28.3 | 28.3 | 28.3 |
| fibonacci | release | Rust | 100 | 17.9 | 0.0 | 17.9 | 17.9 | 17.9 | 17.9 | 17.9 |
| fibonacci | checked | Sabiko | 100 | 30.5 | 0.0 | 30.3 | 30.5 | 30.5 | 30.5 | 30.5 |
| fibonacci | checked | C# | 100 | 28.3 | 0.0 | 28.3 | 28.3 | 28.3 | 28.3 | 28.3 |
| fibonacci | checked | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| lcg_hash | release | Sabiko | 100 | 30.1 | 0.0 | 30.0 | 30.1 | 30.1 | 30.1 | 30.1 |
| lcg_hash | release | C# | 100 | 28.0 | 0.0 | 28.0 | 28.0 | 28.0 | 28.1 | 28.1 |
| lcg_hash | release | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| lcg_hash | checked | Sabiko | 100 | 30.1 | 0.0 | 30.0 | 30.1 | 30.1 | 30.1 | 30.2 |
| lcg_hash | checked | C# | 100 | 28.0 | 0.0 | 28.0 | 28.0 | 28.0 | 28.1 | 28.1 |
| lcg_hash | checked | Rust | 100 | 18.1 | 0.0 | 18.0 | 18.1 | 18.1 | 18.1 | 18.1 |
| mandelbrot | release | Sabiko | 100 | 30.6 | 0.0 | 30.5 | 30.6 | 30.6 | 30.6 | 30.7 |
| mandelbrot | release | C# | 100 | 28.3 | 0.0 | 28.2 | 28.3 | 28.3 | 28.3 | 28.3 |
| mandelbrot | release | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| mandelbrot | checked | Sabiko | 100 | 30.7 | 0.0 | 30.6 | 30.7 | 30.7 | 30.7 | 30.7 |
| mandelbrot | checked | C# | 100 | 28.3 | 0.0 | 28.2 | 28.3 | 28.3 | 28.3 | 28.3 |
| mandelbrot | checked | Rust | 100 | 18.1 | 0.0 | 18.1 | 18.1 | 18.1 | 18.1 | 18.1 |
| nbody | release | Sabiko | 100 | 30.2 | 0.0 | 30.1 | 30.2 | 30.2 | 30.3 | 30.3 |
| nbody | release | C# | 100 | 29.4 | 0.1 | 29.4 | 29.5 | 29.5 | 29.6 | 29.6 |
| nbody | release | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| nbody | checked | Sabiko | 100 | 30.3 | 0.1 | 30.1 | 30.3 | 30.3 | 30.4 | 30.4 |
| nbody | checked | C# | 100 | 29.5 | 0.1 | 29.4 | 29.5 | 29.6 | 29.6 | 29.6 |
| nbody | checked | Rust | 100 | 18.1 | 0.0 | 18.1 | 18.1 | 18.2 | 18.2 | 18.2 |
| primes | release | Sabiko | 100 | 30.1 | 0.0 | 30.0 | 30.1 | 30.1 | 30.1 | 30.1 |
| primes | release | C# | 100 | 28.0 | 0.0 | 28.0 | 28.0 | 28.0 | 28.1 | 28.1 |
| primes | release | Rust | 100 | 18.0 | 0.0 | 18.0 | 18.0 | 18.0 | 18.0 | 18.0 |
| primes | checked | Sabiko | 100 | 30.1 | 0.0 | 29.9 | 30.1 | 30.1 | 30.1 | 30.1 |
| primes | checked | C# | 100 | 28.0 | 0.0 | 28.0 | 28.0 | 28.1 | 28.1 | 28.1 |
| primes | checked | Rust | 100 | 18.2 | 0.0 | 18.2 | 18.2 | 18.2 | 18.2 | 18.2 |

## In-process time per iteration (ms)

| Benchmark | Config | Language | N | mean | stdev | min | p50 | p90 | p99 | max |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| collatz | release | Sabiko | 100 | 52.6 | 2.15 | 50.0 | 51.4 | 54.8 | 61.2 | 61.2 |
| collatz | release | C# | 100 | 53.5 | 2.26 | 50.6 | 53.8 | 54.8 | 62.5 | 64.8 |
| collatz | checked | Sabiko | 100 | 62.2 | 1.58 | 60.8 | 61.2 | 64.4 | 65.9 | 66.1 |
| collatz | checked | C# | 100 | 67.5 | 2.22 | 66.1 | 67.1 | 68.1 | 77.3 | 82.4 |
| fibonacci | release | Sabiko | 100 | 11.3 | 0.44 | 10.8 | 11.2 | 12.0 | 12.5 | 12.7 |
| fibonacci | release | C# | 100 | 11.4 | 0.44 | 10.9 | 11.2 | 11.9 | 12.3 | 12.6 |
| fibonacci | checked | Sabiko | 100 | 20.0 | 1.87 | 18.0 | 19.7 | 21.1 | 27.6 | 27.7 |
| fibonacci | checked | C# | 100 | 15.1 | 0.30 | 14.6 | 15.0 | 15.6 | 15.7 | 15.7 |
| lcg_hash | release | Sabiko | 100 | 108 | 2.45 | 104 | 107 | 111 | 112 | 114 |
| lcg_hash | release | C# | 100 | 108 | 4.33 | 103 | 107 | 112 | 121 | 130 |
| lcg_hash | checked | Sabiko | 100 | 108 | 2.88 | 104 | 107 | 111 | 118 | 123 |
| lcg_hash | checked | C# | 100 | 107 | 3.38 | 102 | 106 | 111 | 117 | 122 |
| mandelbrot | release | Sabiko | 100 | 47.1 | 1.14 | 44.9 | 47.2 | 48.2 | 49.5 | 50.9 |
| mandelbrot | release | C# | 100 | 46.1 | 0.17 | 45.6 | 46.1 | 46.3 | 46.7 | 46.8 |
| mandelbrot | checked | Sabiko | 100 | 47.8 | 1.38 | 45.4 | 48.0 | 48.9 | 51.5 | 53.4 |
| mandelbrot | checked | C# | 100 | 48.1 | 1.31 | 44.5 | 47.7 | 49.8 | 51.5 | 54.0 |
| nbody | release | Sabiko | 100 | 13.2 | 0.35 | 12.7 | 13.1 | 13.5 | 14.3 | 15.1 |
| nbody | release | C# | 100 | 13.6 | 0.84 | 12.9 | 13.4 | 14.0 | 18.4 | 18.9 |
| nbody | checked | Sabiko | 100 | 13.7 | 0.31 | 13.1 | 13.8 | 14.0 | 14.6 | 15.8 |
| nbody | checked | C# | 100 | 13.4 | 0.27 | 13.2 | 13.3 | 13.8 | 13.9 | 15.1 |
| primes | release | Sabiko | 100 | 26.2 | 0.75 | 25.4 | 26.0 | 27.1 | 28.7 | 29.8 |
| primes | release | C# | 100 | 26.2 | 0.51 | 25.4 | 26.0 | 26.9 | 27.6 | 27.7 |
| primes | checked | Sabiko | 100 | 27.6 | 1.29 | 26.5 | 27.3 | 29.3 | 31.9 | 33.2 |
| primes | checked | C# | 100 | 26.7 | 1.06 | 25.6 | 26.4 | 27.4 | 31.4 | 32.6 |

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
| collatz | checked | Sabiko | 0 | 0 | 0 | 32.6 |
| collatz | checked | C# | 0 | 0 | 0 | 31.5 |
| fibonacci | release | Sabiko | 0 | 0 | 0 | 32.7 |
| fibonacci | release | C# | 0 | 0 | 0 | 31.5 |
| fibonacci | checked | Sabiko | 0 | 0 | 0 | 32.7 |
| fibonacci | checked | C# | 0 | 0 | 0 | 31.5 |
| lcg_hash | release | Sabiko | 0 | 0 | 0 | 32.7 |
| lcg_hash | release | C# | 0 | 0 | 0 | 31.6 |
| lcg_hash | checked | Sabiko | 0 | 0 | 0 | 32.9 |
| lcg_hash | checked | C# | 0 | 0 | 0 | 31.6 |
| mandelbrot | release | Sabiko | 0 | 0 | 0 | 33.0 |
| mandelbrot | release | C# | 0 | 0 | 0 | 31.6 |
| mandelbrot | checked | Sabiko | 0 | 0 | 0 | 33.0 |
| mandelbrot | checked | C# | 0 | 0 | 0 | 31.6 |
| nbody | release | Sabiko | 0 | 0 | 0 | 34.7 |
| nbody | release | C# | 0 | 0 | 0 | 33.9 |
| nbody | checked | Sabiko | 0 | 0 | 0 | 34.7 |
| nbody | checked | C# | 0 | 0 | 0 | 33.8 |
| primes | release | Sabiko | 0 | 0 | 0 | 32.5 |
| primes | release | C# | 0 | 0 | 0 | 31.5 |
| primes | checked | Sabiko | 0 | 0 | 0 | 32.5 |
| primes | checked | C# | 0 | 0 | 0 | 31.5 |

*Generated by `benchmarks/run_benchmarks.py` on 2026-10-29T02:29:03Z.*
