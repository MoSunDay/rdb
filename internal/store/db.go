package store

type DB interface {
	Set(prefixKey, k, v []byte) error
	MSet(prefixKey []byte, ata [][]byte) error
	Get(prefixKey []byte, k []byte) ([]byte, error)
	MGet(prefixKey []byte, data [][]byte) []string
	Del(prefixKey, keys []byte) error
	Size() int64
	Close()
}
