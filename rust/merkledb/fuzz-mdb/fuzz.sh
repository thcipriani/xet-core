#!/usr/bin/env bash
cargo afl build && cargo afl fuzz -i in -o out ../../target/debug/fuzz-mdb
