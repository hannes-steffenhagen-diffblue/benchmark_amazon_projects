Simple script to run AWS proofs and measure their runtimes
===========================================================

This is a rust project. Get a rust toolchain either using
[rustup](https://rustup.rs/) or with your system package manager (for ubuntu
you'd need the `rustc` and `cargo` packages).

Either way once you have them run `cargo build --release` to produce the executable in
`target/release/benchmark_aws_projects`.

To run them you'll need to have the aws project you want to benchmark setup
first (submodules checked out, python dependencies installed etc - exact steps
depend on which project you want to check). You'll also want the version of the
cprover tools you want to benchmark on PATH.

Then run with

```
benchmark_aws_projects
  --csv-file <filename>
  --proofs-path <path>
  --iterations <N>
  --parallel-jobs <N>
```

csv-file: where to store the runtime results. The format in this file will be

```
<proof-name>(,runtime in seconds){iterations times}
```

proofs-path: the "proofs" directory, e.g. `verification/cbmc/proofs` in aws-c-common

iterations: How many repeated measurements to run on the same proof

parallel-jobs: How many proofs should run in parallel (note that multiple iterations of the same proof can't run in parallel).
