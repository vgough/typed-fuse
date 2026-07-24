.PHONY: test hello_ll memory_fs run_memory_fs memory_fs_release benchmark benchmark-save-baseline clean

test:
	cargo nextest run --workspace

hello_ll:
	cargo build -p libfuse-sys --example hello_ll_raw --features fuse_35

memory_fs:
	cargo build -p fuse3 --example memory_fs

run_memory_fs:
	mkdir -p /tmp/memfs
	cargo run -p fuse3 --example memory_fs -- /tmp/memfs

BENCH_BASELINE ?= .benchmarks/filesystem-baseline.json
BENCH_ITEMS ?= 1000
BENCH_ITERATIONS ?= 5
BENCH_PROCS ?= 1
BENCH_BYTES ?= 4096
MDTEST_BIN ?= mdtest
MPIRUN_BIN ?= mpirun
BENCH_MOUNT_TIMEOUT_SECS ?= 30

benchmark: memory_fs_release
	test -f 'fuse3/$(BENCH_BASELINE)' || (echo "Benchmark baseline fuse3/$(BENCH_BASELINE) is missing; run 'make benchmark-save-baseline' first" >&2; exit 1)
	BENCH_BASELINE='$(BENCH_BASELINE)' BENCH_ITEMS='$(BENCH_ITEMS)' \
		BENCH_ITERATIONS='$(BENCH_ITERATIONS)' BENCH_PROCS='$(BENCH_PROCS)' \
		BENCH_BYTES='$(BENCH_BYTES)' MDTEST_BIN='$(MDTEST_BIN)' \
		MPIRUN_BIN='$(MPIRUN_BIN)' BENCH_MOUNT_TIMEOUT_SECS='$(BENCH_MOUNT_TIMEOUT_SECS)' \
		cargo bench -p fuse3 --bench filesystem

benchmark-save-baseline: memory_fs_release
	BENCH_SAVE_BASELINE=1 BENCH_BASELINE='$(BENCH_BASELINE)' BENCH_ITEMS='$(BENCH_ITEMS)' \
		BENCH_ITERATIONS='$(BENCH_ITERATIONS)' BENCH_PROCS='$(BENCH_PROCS)' \
		BENCH_BYTES='$(BENCH_BYTES)' MDTEST_BIN='$(MDTEST_BIN)' \
		MPIRUN_BIN='$(MPIRUN_BIN)' BENCH_MOUNT_TIMEOUT_SECS='$(BENCH_MOUNT_TIMEOUT_SECS)' \
		cargo bench -p fuse3 --bench filesystem

memory_fs_release:
	cargo build --release -p fuse3 --example memory_fs

clean:
	cargo clean
