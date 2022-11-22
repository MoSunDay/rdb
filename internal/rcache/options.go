package rcache

type options struct {
	DataDir        string // data directory
	HttpAddress    string // http server address
	RaftTCPAddress string // construct Raft Address
	JoinAddress    string // peer address to join
	RaftToken      string
}

func NewOptions() *options {
	opts := &options{}
	return opts
}
