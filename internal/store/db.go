package store

type DB interface {
	Set(k, v []byte) error
	MSet(data map[string]string) error
	Get(k []byte) ([]byte, error)
	MGet(keys []string) []string
	Del(keys []byte) error
	Scan(ScannerOpt ScannerOptions) error
	Size() int64
	GC() error
	Close()
}

type ScannerOptions struct {
	Offset        []byte
	IncludeOffset bool
	Prefix        []byte
	FetchValues   bool
	Handler       func(k, v []byte) bool
}
