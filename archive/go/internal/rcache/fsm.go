package rcache

import (
	"encoding/json"
	"io"
	"log"
	"rdb/internal/rtypes"

	"github.com/hashicorp/raft"
)

type FSM struct {
	ctx *CachedContext
	log *log.Logger
}

func (f *FSM) Apply(logEntry *raft.Log) interface{} {
	e := rtypes.RaftLogEntryData{}
	if err := json.Unmarshal(logEntry.Data, &e); err != nil {
		panic("Failed unmarshaling Raft log entry. This is a bug.")
	}
	ret := f.ctx.Cache.CM.Set(e.Key, e.Value)
	f.log.Printf("fms.Apply(), logEntry:%s, ret:%v\n", logEntry.Data, ret)
	return ret
}

func (f *FSM) Snapshot() (raft.FSMSnapshot, error) {
	return &snapshot{cm: f.ctx.Cache.CM}, nil
}

func (f *FSM) Restore(serialized io.ReadCloser) error {
	return f.ctx.Cache.CM.UnMarshal(serialized)
}
