package rcache

type options struct {
	DataDir        string // data directory
	HttpAddress    string // http server address
	RaftTCPAddress string // construct Raft Address
	Bootstrap      bool   // start as master or not
	JoinAddress    string // peer address to join
}

func NewOptions() *options {
	opts := &options{}
	return opts
}
