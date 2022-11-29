package store

import (
	"rdb/internal/utils"

	"github.com/cockroachdb/pebble"
	"github.com/cockroachdb/pebble/bloom"
	"github.com/cockroachdb/pebble/sstable"
)

type Pebble struct {
	db *pebble.DB
}

func OpenPebble(path string) (*Pebble, error) {

	option := &pebble.Options{}
	option.EnsureDefaults()
	for i := range option.Levels {
		option.Levels[i].Compression = sstable.NoCompression
		option.Levels[i].FilterPolicy = bloom.FilterPolicy(10)
	}
	db, err := pebble.Open(path, option)

	if err != nil {
		return nil, err
	}

	pdb := new(Pebble)
	pdb.db = db

	return pdb, nil
}

func (pdb *Pebble) Close() {
	pdb.db.Close()
}

func (pdb *Pebble) Size() int {
	batch := pdb.db.NewIndexedBatch()
	return batch.Len()
}

func (pdb *Pebble) MSet(prefix []byte, data [][]byte) error {
	batch := pdb.db.NewIndexedBatch()
	for i := 0; i < len(data); i += 2 {
		item := utils.BytesCombine(prefix, data[i])
		batch.Set(item, data[i+1], nil)
	}
	return batch.Commit(pebble.Sync)
}

func (pdb *Pebble) MGet(prefix []byte, data [][]byte) (resp [][]byte) {
	null := []byte{}
	for _, key := range data {
		item := utils.BytesCombine(prefix, key)
		val, err := pdb.get(item)
		if err != nil {
			resp = append(resp, null)
			continue
		}
		resp = append(resp, val)
	}
	return resp
}

func (pdb *Pebble) set(k, v []byte) error {
	return pdb.db.Set(k, v, pebble.Sync)
}

func (pdb *Pebble) Set(prefix, k, v []byte) error {
	item := utils.BytesCombine(prefix, k)
	return pdb.set(item, v)
}

func (pdb *Pebble) get(k []byte) ([]byte, error) {
	val, _, err := pdb.db.Get(k)
	if err != nil {
		return nil, err
	}
	return val, nil
}

func (pdb *Pebble) Get(prefix, k []byte) ([]byte, error) {
	item := utils.BytesCombine(prefix, k)
	return pdb.get(item)
}

func (pdb *Pebble) Del(prefix, key []byte) error {
	item := utils.BytesCombine(prefix, key)
	pdb.db.Delete(item, pebble.Sync)
	return nil
}

func (pdb *Pebble) Iter(prefix []byte) *pebble.Iterator {
	keyUpperBound := func(b []byte) []byte {
		end := make([]byte, len(b))
		copy(end, b)
		for i := len(end) - 1; i >= 0; i-- {
			end[i] = end[i] + 1
			if end[i] != 0 {
				return end[:i+1]
			}
		}
		return nil // no upper-bound
	}
	prefixIterOptions := func(prefix []byte) *pebble.IterOptions {
		return &pebble.IterOptions{
			LowerBound: prefix,
			UpperBound: keyUpperBound(prefix),
		}
	}
	return pdb.db.NewIter(prefixIterOptions(prefix))
	// for iter.First(); iter.Valid(); iter.Next() {
	// 	log.Printf("%s\n", iter.Key())
	// }
}
