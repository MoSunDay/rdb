package rcache

import (
	"fmt"
	"log"
	"net/http"
	"sync/atomic"

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
