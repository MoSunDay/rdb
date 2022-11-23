package rcache

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"rdb/internal/rtypes"
	"rdb/internal/utils"
	"sync/atomic"
	"time"

	"github.com/hashicorp/raft"
)

// var confLogger = utils.GetLogger("rache/http")

const (
	ENABLE_WRITE_TRUE  = int32(1)
	ENABLE_WRITE_FALSE = int32(0)
)

type HttpServer struct {
	Ctx         *CachedContext
	Log         *log.Logger
	Mux         *http.ServeMux
	EnableWrite int32
}

func NewHttpServer(ctx *CachedContext, log *log.Logger) *HttpServer {
	Mux := http.NewServeMux()
	s := &HttpServer{
		Ctx:         ctx,
		Log:         log,
		Mux:         Mux,
		EnableWrite: ENABLE_WRITE_FALSE,
	}

	// Mux.HandleFunc("/set", s.doSet)
	Mux.HandleFunc("/get", s.doGet)
	Mux.HandleFunc("/join", s.doJoin)
	Mux.HandleFunc("/depart", s.doDepart)
	// Mux.HandleFunc("/info", s.doInfo)
	// Mux.HandleFunc("/hash", s.doHash)
	return s
}

func (h *HttpServer) checkWritePermission() bool {
	return atomic.LoadInt32(&h.EnableWrite) == ENABLE_WRITE_TRUE
}

func (h *HttpServer) SetWriteFlag(flag bool) {
	if flag {
		atomic.StoreInt32(&h.EnableWrite, ENABLE_WRITE_TRUE)
	} else {
		atomic.StoreInt32(&h.EnableWrite, ENABLE_WRITE_FALSE)
	}
}

func (h *HttpServer) doGet(w http.ResponseWriter, r *http.Request) {
	vars := r.URL.Query()
	ret := ""
	key := vars.Get("key")
	if key == "" {
		h.Log.Println("doGet() error, get nil key")
		fmt.Fprint(w, "")
		return
	}

	if r.URL.Query().Get("raft-token") == h.Ctx.Cache.Opts.RaftToken {
		ret = h.Ctx.Cache.CM.Get(key)
	}

	fmt.Fprintf(w, "%s\n", ret)
}

func (h *HttpServer) doSet(w http.ResponseWriter, r *http.Request) {
	if !h.checkWritePermission() {
		fmt.Fprint(w, "write method not allowed\n")
		return
	}
	vars := r.URL.Query()

	key := vars.Get("key")
	value := vars.Get("value")
	if key == "" || value == "" {
		h.Log.Println("doSet() error, get nil key or nil value")
		fmt.Fprint(w, "param error\n")
		return
	}

	event := rtypes.RaftLogEntryData{Key: key, Value: value}
	eventBytes, err := json.Marshal(event)
	if err != nil {
		h.Log.Printf("json.Marshal failed, err:%v", err)
		fmt.Fprint(w, "internal error\n")
		return
	}

	applyFuture := h.Ctx.Cache.Raft.Raft.Apply(eventBytes, 5*time.Second)
	if err := applyFuture.Error(); err != nil {
		h.Log.Printf("raft.Apply failed:%v", err)
		fmt.Fprint(w, "internal error\n")
		return
	}

	fmt.Fprintf(w, "ok\n")
}

func (h *HttpServer) doJoin(w http.ResponseWriter, r *http.Request) {
	vars := r.URL.Query()

	peerAddress := vars.Get("peerAddress")
	if peerAddress == "" {
		h.Log.Println("invalid PeerAddress")
		fmt.Fprint(w, "invalid peerAddress\n")
		return
	}
	if r.URL.Query().Get("raft-token") == h.Ctx.Cache.Opts.RaftToken {
		addPeerFuture := h.Ctx.Cache.Raft.Raft.AddVoter(raft.ServerID(peerAddress), raft.ServerAddress(peerAddress), 0, 0)
		if err := addPeerFuture.Error(); err != nil {
			h.Log.Printf("Error joining peer to raft, peeraddress:%s, err:%v, code:%d", peerAddress, err, http.StatusInternalServerError)
			fmt.Fprint(w, "internal error\n")
			return
		}
	} else {
		log.Println("join cluster failed")
	}
	fmt.Fprint(w, "ok")
}

func (h *HttpServer) doDepart(w http.ResponseWriter, r *http.Request) {
	vars := r.URL.Query()

	peerAddress := vars.Get("peerAddress")
	if peerAddress == "" {
		h.Log.Println("invalid PeerAddress")
		fmt.Fprint(w, "invalid peerAddress\n")
		return
	}
	if r.URL.Query().Get("raft-token") == h.Ctx.Cache.Opts.RaftToken {
		addPeerFuture := h.Ctx.Cache.Raft.Raft.RemoveServer(raft.ServerID(peerAddress), 0, 0)
		if err := addPeerFuture.Error(); err != nil {
			h.Log.Printf("Error depart peer to raft, peeraddress:%s, err:%v, code:%d", peerAddress, err, http.StatusInternalServerError)
			fmt.Fprint(w, "internal error\n")
			return
		}
	} else {
		log.Println("join cluster failed")
	}
	fmt.Fprint(w, "ok")
}

func (h *HttpServer) doInfo(w http.ResponseWriter, r *http.Request) {
	fmt.Fprintf(w, "%+v\nlast_index: %d\n", h.Ctx.Cache.Raft.Raft.Stats(), h.Ctx.Cache.Raft.Raft.LastIndex())
}

func (h *HttpServer) doHash(w http.ResponseWriter, r *http.Request) {
	vars := r.URL.Query()

	key := vars.Get("key")
	if key == "" {
		h.Log.Println("doHash() error, get nil key")
		fmt.Fprint(w, "")
		return
	}

	fmt.Fprintf(w, "key: %s, slot: %d\n", key, utils.GetSlotNumber([]byte(key)))
}
