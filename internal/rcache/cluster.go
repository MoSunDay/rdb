package rcache

import (
	"errors"
	"fmt"
	"io/ioutil"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/raft"
	Raftboltdb "github.com/hashicorp/raft-boltdb"
)

type Cached struct {
	HttpServer *HttpServer
	Opts       *options
	Log        *log.Logger
	CM         *cacheManager
	Raft       *RaftNodeInfo
}

type CachedContext struct {
	Cache *Cached
}

type RaftNodeInfo struct {
	Raft           *raft.Raft
	Fsm            *FSM
	LeaderNotifyCh chan bool
}

func newRaftTransport(opts *options) (*raft.NetworkTransport, error) {
	address, err := net.ResolveTCPAddr("tcp", opts.RaftTCPAddress)
	if err != nil {
		return nil, err
	}
	transport, err := raft.NewTCPTransport(address.String(), address, 3, 10*time.Second, os.Stderr)
	if err != nil {
		return nil, err
	}
	return transport, nil
}

func NewRaftNode(opts *options, ctx *CachedContext) (*RaftNodeInfo, error) {
	RaftConfig := raft.DefaultConfig()
	RaftConfig.LocalID = raft.ServerID(opts.RaftTCPAddress)
	RaftConfig.Logger = hclog.New(&hclog.LoggerOptions{
		Name:       "Raft",
		Output:     os.Stderr,
		TimeFormat: `2006/01/02 15:04:05`,
	})
	RaftConfig.SnapshotInterval = 30 * time.Second
	RaftConfig.SnapshotThreshold = 2
	LeaderNotifyCh := make(chan bool, 1)
	RaftConfig.NotifyCh = LeaderNotifyCh

	transport, err := newRaftTransport(opts)
	if err != nil {
		return nil, err
	}

	if err := os.MkdirAll(opts.DataDir, 0700); err != nil {
		return nil, err
	}

	Fsm := &FSM{
		ctx: ctx,
		log: log.New(os.Stderr, "FSM: ", log.Ldate|log.Ltime),
	}
	snapshotStore, err := raft.NewFileSnapshotStore(opts.DataDir, 1, os.Stderr)
	if err != nil {
		return nil, err
	}

	logStore, err := Raftboltdb.NewBoltStore(filepath.Join(opts.DataDir, "raft-log.bolt"))
	if err != nil {
		return nil, err
	}

	stableStore, err := Raftboltdb.NewBoltStore(filepath.Join(opts.DataDir, "raft-stable.bolt"))
	if err != nil {
		return nil, err
	}

	RaftNode, err := raft.NewRaft(RaftConfig, Fsm, logStore, stableStore, snapshotStore, transport)
	if err != nil {
		return nil, err
	}
	address := transport.LocalAddr()
	if opts.Bootstrap {
		configuration := raft.Configuration{
			Servers: []raft.Server{
				{
					ID:      RaftConfig.LocalID,
					Address: address,
				},
			},
		}
		RaftNode.BootstrapCluster(configuration)
	}

	return &RaftNodeInfo{Raft: RaftNode, Fsm: Fsm, LeaderNotifyCh: LeaderNotifyCh}, nil
}

func JoinRaftCluster(opts *options) error {
	url := fmt.Sprintf("http://%s/join?peerAddress=%s", opts.JoinAddress, opts.RaftTCPAddress)

	resp, err := http.Get(url)
	if err != nil {
		return err
	}

	defer resp.Body.Close()
	body, err := ioutil.ReadAll(resp.Body)
	if err != nil {
		return err
	}

	if string(body) != "ok" {
		return errors.New(fmt.Sprintf("Error joining cluster: %s", body))
	}

	return nil
}
