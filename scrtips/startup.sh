#!/bin/sh
kill $(pidof rdb_debug)
go build -o rdb_debug cmd/rdb/main.go 
export RAFT_BOOTSTRAP=true;
./rdb_debug -config config/conf_32681.yaml &
sleep 5
export RAFT_BOOTSTRAP=;
export RAFT_JOIN_ADDR=127.0.0.1:12681;
./rdb_debug -config config/conf_32683.yaml &
sleep 5
./rdb_debug -config config/conf_32685.yaml &