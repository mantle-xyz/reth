#!/bin/bash

make build-op 
make install-op
cp target/release/op-reth /Users/sh001/Documents/codes/rde-v3/src/op-geth/build/bin/op-reth_dump
ls -l /Users/sh001/Documents/codes/rde-v3/src/op-geth/build/bin/op-reth_dump
