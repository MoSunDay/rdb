package store

import (
	"bytes"
	"sync"

	"github.com/syndtr/goleveldb/leveldb"
	"github.com/syndtr/goleveldb/leveldb/filter"
	"github.com/syndtr/goleveldb/leveldb/iterator"
	"github.com/syndtr/goleveldb/leveldb/opt"
	"github.com/syndtr/goleveldb/leveldb/util"
)

type LevelDB struct {
	db *leveldb.DB
	sync.RWMutex
}

func OpenLevelDB(path string) (*LevelDB, error) {
	db, err := leveldb.OpenFile(path, &opt.Options{
		Filter:      filter.NewBloomFilter(10000),
		WriteBuffer: 25 * 1000 * 1000})

	if err != nil {
		return nil, err
	}

	ldb := new(LevelDB)
	ldb.db = db

	return ldb, nil
}

func (ldb *LevelDB) Size() int64 {
	var stats leveldb.DBStats
	if nil != ldb.db.Stats(&stats) {
		return -1
	}
	size := int64(0)
	for _, v := range stats.LevelSizes {
		size += v
	}
	return size
}

func (ldb *LevelDB) Close() {
	ldb.db.Close()
}

func (ldb *LevelDB) GC() error {
	return ldb.db.CompactRange(util.Range{})
}

func (ldb *LevelDB) set(k, v []byte) error {
	return ldb.db.Put(k, v, nil)
}

func (ldb *LevelDB) Set(k, v []byte) error {
	return ldb.set(k, v)
}

func (ldb *LevelDB) MSet(data [][]byte) error {
	batch := new(leveldb.Batch)
	for i := 0; i < len(data); i += 2 {
		batch.Put([]byte(data[i]), []byte(data[i+1]))
	}
	return ldb.db.Write(batch, nil)
}

func (ldb *LevelDB) get(k []byte) ([]byte, error) {
	item, err := ldb.db.Get(k, nil)
	if err != nil {
		return nil, err
	}
	return item, nil
}

func (ldb *LevelDB) Get(k []byte) ([]byte, error) {
	return ldb.get(k)
}

func (ldb *LevelDB) MGet(keys [][]byte) (data [][]byte) {
	null := []byte{}
	for _, key := range keys {
		val, err := ldb.get(key)
		if err != nil {
			data = append(data, null)
			continue
		}
		data = append(data, val)
	}
	return data
}

func (ldb *LevelDB) Del(key []byte) error {
	batch := new(leveldb.Batch)
	batch.Delete(key)
	return ldb.db.Write(batch, nil)
}

func (ldb *LevelDB) Scan(scannerOpt ScannerOptions) error {
	var iter iterator.Iterator

	if len(scannerOpt.Offset) == 0 {
		iter = ldb.db.NewIterator(nil, nil)
	} else {
		iter = ldb.db.NewIterator(&util.Range{Start: scannerOpt.Offset}, nil)
		if !scannerOpt.IncludeOffset {
			iter.Next()
		}
	}

	valid := func(k []byte) bool {
		if k == nil {
			return false
		}

		if len(scannerOpt.Prefix) != 0 && !bytes.HasPrefix(k, scannerOpt.Prefix) {
			return false
		}

		return true
	}

	for iter.Next() {
		key := iter.Key()
		val := iter.Value()
		if !valid(key) || !scannerOpt.Handler(key, val) {
			break
		}
	}

	iter.Release()

	return iter.Error()
}
