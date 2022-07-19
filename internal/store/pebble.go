package store

import (
	"rdb/internal/utils"

	"github.com/cockroachdb/pebble"
)

type Pebble struct {
	db *pebble.DB
}

func OpenPebble(path string) (*Pebble, error) {
	db, err := pebble.Open(path, &pebble.Options{})

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
	prefix = append(prefix, '/')
	batch := pdb.db.NewIndexedBatch()
	for i := 0; i < len(data); i += 2 {
		item := utils.BytesCombine(prefix, data[i])
		batch.Set(item, data[i+1], nil)
	}
	return batch.Commit(pebble.Sync)
}

func (pdb *Pebble) MGet(prefix []byte, data [][]byte) (resp [][]byte) {
	null := []byte{}
	prefix = append(prefix, '/')
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
	prefix = append(prefix, '/')
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
	prefix = append(prefix, '/')
	item := utils.BytesCombine(prefix, k)
	return pdb.get(item)
}

func (pdb *Pebble) Del(prefix, key []byte) error {
	prefix = append(prefix, '/')
	item := utils.BytesCombine(prefix, key)
	pdb.db.Delete(item, pebble.Sync)
	return nil
}
