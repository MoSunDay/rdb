#!/bin/sh
kill $(pidof rdb_debug)
go build -o rdb_debug cmd/rdb/main.go 
./rdb_debug -config config/conf_32681.yaml &
./rdb_debug -config config/conf_32682.yaml &
./rdb_debug -config config/conf_32683.yaml &